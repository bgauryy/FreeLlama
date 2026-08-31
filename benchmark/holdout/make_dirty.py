#!/usr/bin/env python3
"""Create a realistic *working-directory* variant of each held-out clone.

The first held-out experiment could not discriminate: both arms scored 12/15. The reason was a
harness fault, not a null result — the adapter change under test scopes searches away from build
and vendor directories, and a `--depth 1` clone has none. There was nothing for the fix to fix.

Real repositories are not clean checkouts. The most damaging case, and the one this builds, is a
**vendored copy of the package inside its own virtualenv**: `.venv/lib/python3.13/site-packages/`
holds a second, near-identical copy of the source, so every unscoped `grep` returns each hit twice
from two different paths. An agent then has to decide which copy is authoritative, and nothing in a
plain grep tells it.

Also added: `__pycache__` (stale `.pyc` alongside real files), `node_modules` and `dist` with decoy
matches, and a `build/lib` staging copy — all things `pip install -e .`, a JS toolchain, or a
packaging run leave behind.

Nothing here modifies the pristine clone; the dirty variant is a separate directory, so the clean
and dirty arms are the same questions over the same source with only the surrounding noise changed.
"""

from __future__ import annotations

import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CLONES = REPO / ".clones"


def package_root(clone: Path) -> Path:
    """The importable package directory, e.g. requests/src/requests."""
    for candidate in (clone / "src", clone):
        if candidate.is_dir():
            for child in sorted(candidate.iterdir()):
                if child.is_dir() and (child / "__init__.py").exists():
                    return child
    raise SystemExit(f"no package root under {clone}")


def dirty(name: str) -> None:
    src = CLONES / name
    dst = CLONES / f"{name}-dirty"
    if not src.exists():
        print(f"skip {name}: not cloned")
        return
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst, ignore=shutil.ignore_patterns(".git"))

    pkg = package_root(dst)
    pkgname = pkg.name

    # 1. The vendored self-copy — the realistic killer. Same symbols, different path.
    site = dst / ".venv" / "lib" / "python3.13" / "site-packages"
    site.mkdir(parents=True, exist_ok=True)
    shutil.copytree(pkg, site / pkgname)
    # A couple of unrelated installed deps, so site-packages is not obviously just one copy.
    for dep in ("charset_normalizer", "urllib3"):
        d = site / dep
        d.mkdir(exist_ok=True)
        (d / "__init__.py").write_text(
            f'"""Vendored {dep} stub."""\n\n'
            "DEFAULT_TIMEOUT = 100000\n\n"
            "def build_digest_header(self, method, url):\n"
            "    raise InvalidSchema('decoy')\n"
        )

    # 2. build/lib staging copy, left by `python -m build` / `setup.py build`.
    build = dst / "build" / "lib"
    build.mkdir(parents=True, exist_ok=True)
    shutil.copytree(pkg, build / pkgname)

    # 3. Stale bytecode next to every real module.
    n_pyc = 0
    for py in list(pkg.rglob("*.py")):
        cache = py.parent / "__pycache__"
        cache.mkdir(exist_ok=True)
        (cache / f"{py.stem}.cpython-313.pyc").write_bytes(b"\x00\x0f\r\n" + py.read_bytes()[:600])
        n_pyc += 1

    # 4. A JS toolchain's leavings, with decoy text matches.
    nm = dst / "node_modules" / "@vendor" / "toolkit"
    nm.mkdir(parents=True, exist_ok=True)
    (nm / "index.js").write_text(
        "// decoy: mentions the same identifiers as the real source\n"
        "export const DEFAULT_TIMEOUT = 100000;\n"
        "export function build_digest_header() { throw new InvalidSchema(); }\n"
    )
    (nm / "package.json").write_text('{"name":"@vendor/toolkit","version":"1.0.0"}\n')
    dist = dst / "dist"
    dist.mkdir(exist_ok=True)
    (dist / f"{pkgname}-bundle.js").write_text("var DEFAULT_TIMEOUT=100000;\n")

    copies = 3  # real + venv + build/lib
    print(
        f"{name}-dirty: {copies} copies of `{pkgname}` on disk "
        f"(source, .venv/site-packages, build/lib), {n_pyc} stale .pyc, node_modules + dist decoys"
    )


def main() -> None:
    names = sys.argv[1:] or ["requests", "jinja"]
    for n in names:
        dirty(n)


if __name__ == "__main__":
    main()
