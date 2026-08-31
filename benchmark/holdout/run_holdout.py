#!/usr/bin/env python3
"""Held-out eval: baseline adapter vs current adapter, over clean or dirty working copies.

Usage:
    python3 benchmark/holdout/run_holdout.py [--condition clean|dirty] [--limit N] [--trials K]

## Grading is an accept-SET, not one string

Strict single-form matching is the largest source of false negatives in answer and trajectory
grading. It punishes an answer for being *more* informative — `requests/src/requests/auth.py`
instead of `auth.py`, `HTTPDigestAuth.build_digest_header` instead of `build_digest_header`. Every
form in `accept` still requires having found the right thing; none of them can be produced by an
agent that found nothing. Answers are normalised (case, backticks, quotes, trailing parens) before
matching.

Each incorrect result carries a `failureSignature` so failures are interpretable rather than a bare
zero: did it read nothing, exhaust the turn budget, fail to parse, or simply answer wrong?
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path

EVAL = Path(__file__).resolve().parent
REPO = EVAL.parents[1]
CLONES = REPO / ".clones"
BASELINE_ADAPTER = os.environ.get("HOLDOUT_BASELINE", "/tmp/evalrun/bash_agent_unscoped.py")
CURRENT_ADAPTER = str(REPO / "benchmark/local/scripts/bash_agent.py")


def normalise(text: str) -> str:
    text = text.lower().replace("`", "").replace('"', "").replace("'", "")
    return re.sub(r"\s+", " ", text)


def graded(answer: str, accept: list[str]) -> tuple[bool, str | None]:
    a = normalise(answer)
    for form in accept:
        if normalise(form) in a:
            return True, form
    return False, None


def failure_signature(res: dict, answer: str) -> str:
    calls = res.get("tool_calls", [])
    if not answer.strip():
        return "no_answer"
    if answer.startswith("agent response failed") or "unparseable" in answer:
        return "unparseable_reply"
    if not calls:
        return "ungrounded_no_tool_calls"
    if all(c.get("status") != "ok" for c in calls):
        return "all_tool_calls_failed"
    if len(calls) >= 8:
        return "turn_budget_exhausted"
    return "wrong_answer"


def run_one(adapter: str, case: dict, workspace: Path, turns: str) -> dict:
    d = tempfile.mkdtemp()
    prompt, result = os.path.join(d, "p.md"), os.path.join(d, "r.json")
    Path(prompt).write_text(case["q"] + "\n")
    env = {
        **os.environ,
        "FREELLAMA_TARGET_MODEL": os.environ.get("HOLDOUT_MODEL", "qwen3.8:27b-mlx"),
        "FREELLAMA_OLLAMA_ENDPOINT": os.environ.get(
            "FREELLAMA_OLLAMA_ENDPOINT", "http://127.0.0.1:11435"
        ),
        "FREELLAMA_BENCH_WORKSPACE": str(workspace),
        "FREELLAMA_BENCH_PROMPT": prompt,
        "FREELLAMA_AGENT_RESULT": result,
        "FREELLAMA_AGENT_MAX_TURNS": turns,
    }
    t0 = time.time()
    try:
        subprocess.run(["python3", adapter], env=env, capture_output=True, timeout=420)
    except subprocess.TimeoutExpired:
        pass
    elapsed = time.time() - t0
    try:
        res = json.loads(Path(result).read_text())
    except Exception:
        res = {"final_answer": "", "tool_calls": [], "usage": {}}
    answer = res.get("final_answer", "")
    calls = res.get("tool_calls", [])
    ok, matched = graded(answer, case["accept"])
    return {
        "correct": ok,
        "matchedForm": matched,
        "failureSignature": None if ok else failure_signature(res, answer),
        "calls": len(calls),
        "okCalls": sum(1 for c in calls if c.get("status") == "ok"),
        "localIn": res.get("usage", {}).get("input_tokens"),
        "answerTokens": max(1, len(answer) // 4),
        "compactions": res.get("model_metadata", {}).get("context_compactions"),
        "sec": round(elapsed, 1),
        "answer": answer[:300],
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--condition", choices=["clean", "dirty"], default="dirty")
    ap.add_argument("--limit", type=int, default=0, help="cap cases (deterministic stride)")
    ap.add_argument("--trials", type=int, default=1)
    ap.add_argument("--turns", default="8")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    cases = json.loads((CLONES / "_eval" / "truth.json").read_text())
    if args.limit and args.limit < len(cases):
        stride = max(1, len(cases) // args.limit)
        cases = cases[::stride][: args.limit]

    suffix = "-dirty" if args.condition == "dirty" else ""
    out = Path(args.out or CLONES / "_eval" / f"holdout-{args.condition}.json")

    rows = []
    for trial in range(1, args.trials + 1):
        for arm, adapter in (("baseline", BASELINE_ADAPTER), ("current", CURRENT_ADAPTER)):
            for i, case in enumerate(cases, 1):
                ws = CLONES / f"{case['repo']}{suffix}"
                if not ws.exists():
                    print(f"skip {case['repo']}{suffix}: missing")
                    continue
                r = run_one(adapter, case, ws, args.turns)
                rows.append({"trial": trial, "arm": arm, "condition": args.condition,
                             "kind": case["kind"], "tier": case["tier"], "repo": case["repo"],
                             "accept": case["accept"][0], "truth": case["truth"], **r})
                print(f"t{trial} {arm:<9} {i:>2}/{len(cases)} [{case['tier'][:4]}/{case['kind']:<13}] "
                      f"{'OK' if r['correct'] else ' X'} {r['calls']}c {r['sec']:>5.0f}s "
                      f"{r['failureSignature'] or ''}", flush=True)
                out.write_text(json.dumps(rows, indent=2))
    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
