#!/usr/bin/env python3
"""Hidden deterministic behavior checks for benchmark fixture tasks."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from typing import Callable

CONFIG: dict[str, object] = {}


def run_python(workspace: Path, source: str, timeout: int = 20) -> subprocess.CompletedProcess[str]:
    environment = os.environ.copy()
    environment["PYTHONPATH"] = str(workspace / "src")
    return subprocess.run(
        [sys.executable, "-c", source], cwd=workspace, env=environment,
        text=True, capture_output=True, timeout=timeout, check=False,
    )


def regression(workspace: Path) -> tuple[bool, str]:
    environment = os.environ.copy()
    environment["PYTHONPATH"] = str(workspace / "src")
    result = subprocess.run(
        [sys.executable, "-m", "unittest", "discover", "-s", "tests"],
        cwd=workspace, env=environment, text=True, capture_output=True, timeout=30, check=False,
    )
    return result.returncode == 0, (result.stdout + result.stderr)[-2000:]


def run_repo_python(
    workspace: Path, repo_name: str, source: str, timeout: int = 20
) -> subprocess.CompletedProcess[str]:
    repo = workspace / repo_name
    environment = os.environ.copy()
    environment["PYTHONPATH"] = str(repo / "src")
    return subprocess.run(
        [sys.executable, "-c", source],
        cwd=repo,
        env=environment,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )


def repo_pytest(workspace: Path, repo_name: str, target: str) -> tuple[bool, str]:
    repo = workspace / repo_name
    environment = os.environ.copy()
    environment["PYTHONPATH"] = str(repo / "src")
    result = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", target],
        cwd=repo,
        env=environment,
        text=True,
        capture_output=True,
        timeout=60,
        check=False,
    )
    return result.returncode == 0, (result.stdout + result.stderr)[-2000:]


def q04(workspace: Path) -> tuple[bool, str]:
    result = run_python(workspace, "from atlas.math_utils import inclusive_range; assert inclusive_range(2, 5) == [2, 3, 4, 5]; assert inclusive_range(1, 1) == [1]")
    return result.returncode == 0, result.stderr


def q08(workspace: Path) -> tuple[bool, str]:
    status = CONFIG.get("health_status", "ok")
    result = run_python(workspace, f"from atlas.service import Service; assert Service().health() == {{'status': {status!r}}}")
    regress, detail = regression(workspace)
    return result.returncode == 0 and regress, result.stderr + detail


def q11(workspace: Path) -> tuple[bool, str]:
    result = run_python(workspace, "from atlas.service import Service; s=Service(); a=s.execute('alpha','7'); b=s.execute('beta','7'); assert a['value']=='alpha:7'; assert b['value']=='beta:7'")
    regress, detail = regression(workspace)
    return result.returncode == 0 and regress, result.stderr + detail


def q12(workspace: Path) -> tuple[bool, str]:
    result = run_python(workspace, "from atlas.auth import is_allowed; assert is_allowed('reader','read'); assert not is_allowed('reader','write'); assert not is_allowed('reader','delete'); assert is_allowed('admin','delete'); assert not is_allowed('unknown','read')")
    regress, detail = regression(workspace)
    return result.returncode == 0 and regress, result.stderr + detail


def q13(workspace: Path) -> tuple[bool, str]:
    state = workspace / ".flaky-read-state"
    source = workspace / "tools" / "flaky_read.py"
    expected = str(CONFIG.get("recovery_code", "RECOVERED-731"))
    if not state.exists() or expected not in source.read_text(encoding="utf-8"):
        return False, "recovery state missing or tool source was altered"
    result = subprocess.run([sys.executable, str(source)], cwd=workspace, text=True, capture_output=True, timeout=10, check=False)
    return result.returncode == 0 and result.stdout.strip() == expected, result.stdout + result.stderr


def q15(workspace: Path) -> tuple[bool, str]:
    path = workspace / "mcp-result.json"
    if not path.exists():
        return False, "mcp-result.json missing"
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        return False, str(error)
    expected = {"build_code": str(CONFIG.get("build_code", "MCP-2048")), "checksum": str(CONFIG.get("checksum", "7f3a9c1d"))}
    return value == expected, f"expected {expected!r}, got {value!r}"


def q16(workspace: Path) -> tuple[bool, str]:
    path = workspace / "audit.json"
    if not path.exists():
        return False, "audit.json missing"
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        return False, str(error)
    required = {"entrypoint", "test_command", "evidence"}
    okay = set(value) == required and value["entrypoint"] == "src/atlas/api.py" and "unittest" in value["test_command"] and isinstance(value["evidence"], list) and bool(value["evidence"])
    return okay, f"audit keys/content invalid: {value!r}"


def q18(workspace: Path) -> tuple[bool, str]:
    source = (workspace / "src/atlas/service.py").read_text(encoding="utf-8")
    private_method = "def _" in source and "execute" in source
    result = run_python(workspace, "from atlas.service import Service; assert Service().execute('x','9')['value']=='x:9'")
    regress, detail = regression(workspace)
    return private_method and result.returncode == 0 and regress, ("private method missing\n" if not private_method else "") + result.stderr + detail


def q19(workspace: Path) -> tuple[bool, str]:
    source = (workspace / "src/atlas/catalog.py").read_text(encoding="utf-8")
    size = int(CONFIG.get("catalog_size", 20_000))
    result = run_python(workspace, f"import time; from atlas.catalog import ITEMS, contains; assert len(ITEMS)=={size}; assert contains('item-{size - 1}'); assert not contains('absent'); s=time.perf_counter(); [contains('absent') for _ in range(2000)]; assert time.perf_counter()-s < 0.12")
    optimized_shape = "set(" in source or "{" in source or "_INDEX" in source
    regress, detail = regression(workspace)
    return optimized_shape and result.returncode == 0 and regress, ("no indexed representation found\n" if not optimized_shape else "") + result.stderr + detail


def q20(workspace: Path) -> tuple[bool, str]:
    default = int(CONFIG.get("default_timeout", 30))
    result = run_python(workspace, f"import os; from atlas.config import timeout_seconds; os.environ['ATLAS_TIMEOUT']=''; assert timeout_seconds({{'timeout':'17'}})==17; assert timeout_seconds({{}})=={default}; os.environ['ATLAS_TIMEOUT']='9'; assert timeout_seconds({{'timeout':'17'}})==9")
    regress, detail = regression(workspace)
    return result.returncode == 0 and regress, result.stderr + detail


def r03(workspace: Path) -> tuple[bool, str]:
    result = run_repo_python(
        workspace,
        "itsdangerous",
        "from itsdangerous.encoding import base64_encode, base64_decode; "
        "values=[b'', b'a', b'ab', b'abc', bytes(range(32))]; "
        "assert all(base64_decode(base64_encode(value)) == value for value in values); "
        "assert all(not base64_encode(value).endswith(b'=') for value in values)",
    )
    regress, detail = repo_pytest(
        workspace, "itsdangerous", "tests/test_itsdangerous/test_encoding.py"
    )
    return result.returncode == 0 and regress, result.stderr + detail


def r04(workspace: Path) -> tuple[bool, str]:
    result = run_repo_python(
        workspace,
        "itsdangerous",
        "from itsdangerous import Signer; from itsdangerous.exc import BadSignature; "
        "s=Signer(['old-key','new-key']); token=s.sign(b'payload'); "
        "assert s.unsign(token)==b'payload'; assert s.validate(token); "
        "assert not s.validate(token[:-1]+b'x')",
    )
    regress, detail = repo_pytest(
        workspace, "itsdangerous", "tests/test_itsdangerous/test_signer.py"
    )
    return result.returncode == 0 and regress, result.stderr + detail


def r06(workspace: Path) -> tuple[bool, str]:
    result = run_repo_python(
        workspace,
        "click",
        "from click.formatting import measure_table; "
        "assert measure_table([(chr(27)+'[31mred'+chr(27)+'[0m','x')]) == (3,1)",
    )
    regress, detail = repo_pytest(workspace, "click", "tests/test_formatting.py")
    return result.returncode == 0 and regress, result.stderr + detail


def r08(workspace: Path) -> tuple[bool, str]:
    result = run_repo_python(
        workspace,
        "itsdangerous",
        "from itsdangerous.timed import TimestampSigner; "
        "from itsdangerous.exc import SignatureExpired; "
        "s=TimestampSigner('secret'); s.get_timestamp=lambda:100; token=s.sign(b'x'); "
        "s.get_timestamp=lambda:105; assert s.unsign(token,max_age=5)==b'x'; "
        "s.get_timestamp=lambda:106; "
        "ok=False; "
        "\ntry: s.unsign(token,max_age=5)\nexcept SignatureExpired: ok=True\nassert ok",
    )
    return result.returncode == 0, result.stderr


def r10(workspace: Path) -> tuple[bool, str]:
    result = run_repo_python(
        workspace,
        "click",
        "from click import Command, Context; from click.core import UNSET; "
        "factory=lambda:'lazy'; c=Context(Command('x')); "
        "c.default_map={'zero':0,'none':None,'lazy':factory,'missing':UNSET}; "
        "assert c._default_map_has('zero') and c.lookup_default('zero')==0; "
        "assert c._default_map_has('none') and c.lookup_default('none') is None; "
        "assert c.lookup_default('lazy')=='lazy'; "
        "assert c.lookup_default('lazy',call=False) is factory; "
        "assert not c._default_map_has('missing')",
    )
    regress, detail = repo_pytest(workspace, "click", "tests/test_defaults.py")
    return result.returncode == 0 and regress, result.stderr + detail


VERIFIERS: dict[str, Callable[[Path], tuple[bool, str]]] = {
    "Q04": q04, "Q08": q08, "Q11": q11, "Q12": q12, "Q13": q13,
    "Q15": q15, "Q16": q16, "Q18": q18, "Q19": q19, "Q20": q20,
    "R03": r03, "R04": r04, "R06": r06, "R08": r08, "R10": r10,
}


def main() -> int:
    parser = argparse.ArgumentParser(description="Run a hidden benchmark task verifier.")
    parser.add_argument("--task", required=True, choices=sorted(VERIFIERS))
    parser.add_argument("--workspace", required=True, type=Path)
    parser.add_argument("--config-json", default="{}")
    args = parser.parse_args()
    global CONFIG
    CONFIG = json.loads(args.config_json)
    started = time.perf_counter()
    try:
        passed, detail = VERIFIERS[args.task](args.workspace.resolve())
    except Exception as error:  # verifier failures must be explicit, not crashes
        passed, detail = False, f"verifier error: {type(error).__name__}: {error}"
    print(json.dumps({"key": f"verifier:{args.task}", "score": passed, "comment": detail[-2000:], "duration_ms": round((time.perf_counter() - started) * 1000, 3)}))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
