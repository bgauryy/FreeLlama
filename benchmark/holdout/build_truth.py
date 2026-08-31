#!/usr/bin/env python3
"""Derive held-out ground truth from the AST — never from a model.

This is the anchor node: every expected answer is computed by `ast`, before any model runs, over
repositories cloned into `.clones/` that were never used to tune an adapter prompt.

## Question design

The first version of this suite was almost entirely single-hop (one file, one lookup) and both arms
of the first experiment scored an identical 12/15 — it could not discriminate. Repository-level QA
benchmarks converge on the same conclusion: the signal is in **cross-file, multi-hop** questions —
dependency tracing, feature localization, intent inference (SWE-QA arXiv:2509.14635; DeepRepoQA
arXiv:2608.24221; CoReQA arXiv:2501.03447).

So cases are now tiered, following the capability-vs-regression split:

| Tier | Kinds | Expectation |
|---|---|---|
| `regression` | `location`, `constant` | near-100%; these must not break |
| `capability` | `complexity`, `signature`, `decorator`, `inheritance` | single file, real work |
| `advanced` | `callsite`, `import_origin`, `raises` | cross-file / multi-hop |

## Grading

Each case ships an `accept` SET, not one string. Strict single-form matching is the largest source
of false negatives in trajectory and answer grading, and it punishes an answer for being *more*
informative (`requests/auth.py` instead of `auth.py`, `HTTPDigestAuth.build_digest_header` instead
of `build_digest_header`). Every accepted form still requires having found the right thing.

Cases whose answer is not distinctive enough to grade by substring are dropped, not graded loosely.
"""

from __future__ import annotations

import ast
import json
from collections import Counter, defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CLONES = REPO / ".clones"
BRANCHES = (ast.If, ast.For, ast.While, ast.Try, ast.ExceptHandler, ast.With)

# Names too common to grade on: an answer that found nothing can still contain them.
GENERIC = {
    "parse", "resolve", "render", "visit", "update", "encode", "decode", "format", "handle",
    "process", "prepare", "extend", "append", "iterate", "wrapper", "inner", "decorator",
    "get", "set", "run", "main", "init", "call", "read", "write", "close", "open", "send",
    "value", "result", "data", "item", "name", "self", "cls", "args", "kwargs", "test",
}

TIERS = {
    "location": "regression", "constant": "regression",
    "complexity": "capability", "signature": "capability",
    "decorator": "capability", "inheritance": "capability",
    "callsite": "advanced", "import_origin": "advanced", "raises": "advanced",
}


def distinctive(name: str) -> bool:
    return len(name) >= 6 and not name.startswith("__") and name.lower() not in GENERIC


def py_files(root: Path) -> list[Path]:
    return sorted(
        p for p in root.rglob("*.py")
        if "test" not in p.parts and not p.name.startswith("test_") and "docs" not in p.parts
    )


def branch_count(fn: ast.AST) -> int:
    return sum(isinstance(n, BRANCHES) for n in ast.walk(fn))


def name_forms(name: str, owner: str | None = None) -> list[str]:
    """Every phrasing that proves the model found this symbol."""
    forms = [name, f"{name}()"]
    if owner:
        forms += [f"{owner}.{name}", f"{owner}.{name}()"]
    return forms


def path_forms(path: Path) -> list[str]:
    """Basename through full relative path — a longer answer is more informative, not wrong."""
    rel = path.relative_to(CLONES)
    parts = rel.parts
    return [path.name] + ["/".join(parts[i:]) for i in range(len(parts))]


class Index:
    """One pass over the package: definitions, call sites, imports, class bases."""

    def __init__(self, root: Path) -> None:
        self.trees: dict[Path, ast.Module] = {}
        self.func_def: dict[str, list[Path]] = defaultdict(list)
        self.class_def: dict[str, list[Path]] = defaultdict(list)
        self.owner: dict[str, str] = {}
        self.calls: dict[str, set[tuple[Path, str]]] = defaultdict(set)
        self.bases: dict[str, tuple[Path, list[str]]] = {}
        self.imports: dict[Path, dict[str, str]] = defaultdict(dict)

        for f in py_files(root):
            try:
                tree = ast.parse(f.read_text(encoding="utf-8", errors="ignore"))
            except SyntaxError:
                continue
            self.trees[f] = tree
            for node in ast.walk(tree):
                if isinstance(node, ast.ClassDef):
                    self.class_def[node.name].append(f)
                    self.bases[node.name] = (
                        f, [b.id for b in node.bases if isinstance(b, ast.Name)]
                    )
                    for m in node.body:
                        if isinstance(m, (ast.FunctionDef, ast.AsyncFunctionDef)):
                            self.owner[m.name] = node.name
                elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    self.func_def[node.name].append(f)
                elif isinstance(node, ast.ImportFrom) and node.module:
                    for alias in node.names:
                        self.imports[f][alias.asname or alias.name] = node.module

            # Call sites, attributed to the enclosing function.
            for parent in ast.walk(tree):
                if not isinstance(parent, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    continue
                for node in ast.walk(parent):
                    if isinstance(node, ast.Call):
                        fn = node.func
                        target = fn.id if isinstance(fn, ast.Name) else (
                            fn.attr if isinstance(fn, ast.Attribute) else None
                        )
                        if target:
                            self.calls[target].add((f, parent.name))

    def unique_func(self, name: str) -> Path | None:
        paths = self.func_def.get(name, [])
        return paths[0] if len(paths) == 1 and not self.class_def.get(name) else None


def build(repo: str) -> list[dict]:
    root = CLONES / repo
    if not root.exists():
        return []
    idx = Index(root)
    cases: list[dict] = []

    def add(kind: str, q: str, accept: list[str], truth: str) -> None:
        accept = sorted({a for a in accept if a and len(a) >= 4}, key=len, reverse=True)
        if accept:
            cases.append({"kind": kind, "tier": TIERS[kind], "repo": repo,
                          "q": q, "accept": accept, "truth": truth})

    # --- regression: location -----------------------------------------------------------------
    for name, paths in idx.func_def.items():
        if len(paths) != 1 or not distinctive(name) or idx.class_def.get(name):
            continue
        add("location",
            f"In this repository, which file defines `{name}`? Answer with the file path.",
            path_forms(paths[0]), f"{name} defined in {paths[0].relative_to(CLONES)}")

    # --- regression: constant -----------------------------------------------------------------
    for f, tree in idx.trees.items():
        for node in tree.body:
            if not isinstance(node, ast.Assign) or len(node.targets) != 1:
                continue
            t = node.targets[0]
            if not isinstance(t, ast.Name) or not t.id.isupper() or len(t.id) < 5:
                continue
            if not isinstance(node.value, ast.Constant):
                continue
            v = node.value.value
            if isinstance(v, str) and (len(v.strip()) < 8 or len(v) > 40 or v.strip().isalpha()):
                continue
            if isinstance(v, int) and abs(v) < 100:
                continue
            if not isinstance(v, (int, str)):
                continue
            add("constant",
                f"In the file {f.relative_to(CLONES)}, what value is assigned to the module-level "
                f"constant `{t.id}`?", [str(v)], f"{t.id} = {v!r}")

    # --- capability: complexity, signature, decorator, inheritance -----------------------------
    for f, tree in idx.trees.items():
        fns = [n for n in tree.body if isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef))]
        fns += [m for n in tree.body if isinstance(n, ast.ClassDef)
                for m in n.body if isinstance(m, (ast.FunctionDef, ast.AsyncFunctionDef))]
        if len(fns) >= 4:
            ranked = sorted(((branch_count(fn), fn.name) for fn in fns), reverse=True)
            if ranked[0][0] >= 5 and ranked[0][0] != ranked[1][0] and distinctive(ranked[0][1]):
                w = ranked[0][1]
                add("complexity",
                    f"In the file {f.relative_to(CLONES)}, which single function or method contains "
                    f"the most branch statements (if/for/while/try/except/with)? Answer with just "
                    f"the function name.",
                    name_forms(w, idx.owner.get(w)),
                    f"{w} with {ranked[0][0]} branches; runner-up {ranked[1][1]} at {ranked[1][0]}")

        for fn in fns:
            if not distinctive(fn.name):
                continue
            if fn.decorator_list:
                decs = [d.id if isinstance(d, ast.Name) else
                        d.attr if isinstance(d, ast.Attribute) else None
                        for d in fn.decorator_list]
                decs = [d for d in decs if d and len(d) >= 6]
                if len(decs) == 1:
                    add("decorator",
                        f"In the file {f.relative_to(CLONES)}, which decorator is applied to "
                        f"`{fn.name}`?", [decs[0], f"@{decs[0]}"], f"@{decs[0]} on {fn.name}")
            # Deliberately NOT "how many parameters" — a bare digit matches any prose, so it
            # grades a miss as a hit. The last parameter's *name* is distinctive and just as exact.
            params = [a.arg for a in fn.args.args + fn.args.kwonlyargs]
            if len(params) >= 3 and idx.unique_func(fn.name) and distinctive(params[-1]):
                add("signature",
                    f"In {f.relative_to(CLONES)}, what is the name of the LAST parameter "
                    f"of `{fn.name}`?",
                    [params[-1]], f"{fn.name}({', '.join(params)}) -> last is {params[-1]}")

    for cls, (f, bases) in idx.bases.items():
        if len(bases) == 1 and distinctive(cls) and distinctive(bases[0]):
            add("inheritance",
                f"In this repository, which class does `{cls}` inherit from?",
                [bases[0]], f"{cls}({bases[0]}) in {f.relative_to(CLONES)}")

    # --- advanced: cross-file / multi-hop ------------------------------------------------------
    for target, sites in idx.calls.items():
        if not distinctive(target):
            continue
        defined = idx.unique_func(target)
        if not defined:
            continue
        callers = {(p, fn) for p, fn in sites if fn != target and distinctive(fn)}
        if len(callers) != 1:
            continue  # exactly one caller keeps the answer unambiguous
        cp, cn = next(iter(callers))
        if cp == defined:
            continue  # must be cross-file to count as multi-hop
        add("callsite",
            f"In this repository, which function calls `{target}`? Answer with the calling "
            f"function's name.",
            name_forms(cn, idx.owner.get(cn)),
            f"{target} (defined {defined.relative_to(CLONES)}) called only by {cn} "
            f"in {cp.relative_to(CLONES)}")

    for f, mapping in idx.imports.items():
        for sym, module in mapping.items():
            if not distinctive(sym) or len(module) < 4 or module.startswith("."):
                continue
            tail = module.split(".")[-1]
            accept = [module] + ([tail] if distinctive(tail) else [])
            add("import_origin",
                f"In the file {f.relative_to(CLONES)}, which module is `{sym}` imported from?",
                accept, f"from {module} import {sym}")

    for f, tree in idx.trees.items():
        for node in ast.walk(tree):
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            raised = {
                r.exc.func.id if isinstance(r.exc, ast.Call) and isinstance(r.exc.func, ast.Name)
                else r.exc.id if isinstance(r.exc, ast.Name) else None
                for r in ast.walk(node) if isinstance(r, ast.Raise) and r.exc
            }
            raised = {r for r in raised if r and len(r) >= 6}
            if len(raised) == 1 and distinctive(node.name) and idx.unique_func(node.name):
                exc = next(iter(raised))
                add("raises",
                    f"Which exception type does `{node.name}` raise in "
                    f"{f.relative_to(CLONES)}?", [exc], f"{node.name} raises {exc}")

    return cases


def main() -> None:
    pools: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for repo in ("requests", "jinja"):
        found = build(repo)
        print(f"{repo}: {len(found)} candidates {dict(Counter(c['kind'] for c in found))}")
        for c in found:
            pools[(repo, c["kind"])].append(c)

    # Deterministic stride selection over a sorted pool: reproducible, not cherry-picked.
    want = {"location": 3, "constant": 2, "complexity": 4, "signature": 3, "decorator": 2,
            "inheritance": 3, "callsite": 4, "import_origin": 3, "raises": 3}
    chosen: list[dict] = []
    for repo in ("requests", "jinja"):
        for kind, n in want.items():
            pool = sorted(pools.get((repo, kind), []), key=lambda c: c["q"])
            if not pool:
                continue
            stride = max(1, len(pool) // n)
            chosen += pool[::stride][:n]

    out = CLONES / "_eval" / "truth.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(chosen, indent=2) + "\n")
    tiers = Counter(c["tier"] for c in chosen)
    print(f"\nselected {len(chosen)} cases -> {out}")
    print(f"tiers: {dict(tiers)}")
    for kind in want:
        got = [c for c in chosen if c["kind"] == kind]
        if got:
            print(f"  {kind:<14} {len(got):>2}  e.g. {got[0]['accept'][0]!r}")


if __name__ == "__main__":
    main()
