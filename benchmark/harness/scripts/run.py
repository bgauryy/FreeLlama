#!/usr/bin/env python3
"""Run isolated FreeLlama agent benchmark trials."""

from __future__ import annotations

import argparse
from datetime import date, datetime, timezone
import json
import os
from pathlib import Path
import re
import signal
import shlex
import shutil
import subprocess
import sys
import time
from typing import Any
import uuid

from _common import RUNNER_VERSION, cache_ratio, changed_files, load_json, manifest, manifest_hash, safe_slug, write_json
from _schema import require_valid


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run a frozen benchmark suite against one model-agent command.",
        epilog="Example: python3 run.py --suite ../tasks/suite.json --model qwen --agent-command 'agent --model {model} --prompt {prompt_file} --workspace {workspace}' --capability filesystem --trials 3 --results ./results",
    )
    parser.add_argument("--suite", required=True, type=Path)
    parser.add_argument("--model", required=True)
    parser.add_argument("--agent", default="custom-command")
    parser.add_argument("--agent-command", required=True, help="Command template with {model}, {prompt_file}, {workspace}, and optional {result_file}.")
    parser.add_argument("--capability", action="append", default=[], choices=["filesystem", "shell", "tools", "skills", "mcp"])
    parser.add_argument("--task", action="append", default=[], help="Task id to run; repeat for multiple. Default: all 20.")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--results", required=True, type=Path)
    parser.add_argument("--judge-model", help="Optional local Ollama judge model.")
    parser.add_argument("--judge-endpoint", default="http://127.0.0.1:11434")
    parser.add_argument("--judge-calibration", type=Path, help="Dated calibration artifact produced by calibrate_judge.py.")
    parser.add_argument("--discard-workspaces", action="store_true")
    parser.add_argument("--allow-stale-suite", action="store_true", help="Smoke-only override for an expired suite review date.")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--run-id", help="Resume into an existing run id when running an explicit task subset.")
    return parser.parse_args()


def tool_stats(tool_calls: list[dict[str, Any]]) -> tuple[int, int, int, int]:
    failed = sum(1 for call in tool_calls if call.get("status") in {"error", "failed", "timeout"})
    signatures: dict[str, int] = {}
    result_chars = 0
    for call in tool_calls:
        signature = json.dumps([call.get("name"), call.get("arguments")], sort_keys=True, default=str)
        signatures[signature] = signatures.get(signature, 0) + 1
        result_chars += len(json.dumps(call.get("result", ""), ensure_ascii=False, default=str))
    retries = sum(max(0, count - 1) for count in signatures.values())
    return len(tool_calls), failed, retries, result_chars


def calibration_status(path: Path | None, judge_model: str | None) -> tuple[bool, str | None, dict[str, Any] | None]:
    if path is None or judge_model is None: return False, "no calibration artifact", None
    try:
        value = load_json(path)
        if value.get("judge_model") != judge_model: return False, "calibration model does not match judge", value
        if value.get("passed") is not True: return False, "calibration gates did not pass", value
        if date.fromisoformat(value["expires_at"]) < date.today(): return False, f"calibration expired on {value['expires_at']}", value
        return True, None, value
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        return False, f"invalid calibration artifact: {error}", None


def process_tree_rss_kb(root_pid: int) -> int | None:
    try:
        snapshot = subprocess.run(["ps", "-axo", "pid=,ppid=,rss="], text=True, capture_output=True, timeout=2, check=False)
        rows = []
        for line in snapshot.stdout.splitlines():
            parts = line.split()
            if len(parts) == 3: rows.append(tuple(map(int, parts)))
        descendants = {root_pid}
        changed = True
        while changed:
            changed = False
            for pid, parent, _ in rows:
                if parent in descendants and pid not in descendants:
                    descendants.add(pid); changed = True
        values = [rss for pid, _, rss in rows if pid in descendants]
        return sum(values) if values else None
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return None


def terminate_process_group(process: subprocess.Popen[Any]) -> None:
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=2)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            try: os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError: pass
    else:
        try: os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError: pass


def execute_agent(command: list[str], workspace: Path, environment: dict[str, str], timeout_seconds: int, stdout_path: Path, stderr_path: Path) -> tuple[str, str, int | None, str, float, int | None]:
    started = time.perf_counter()
    peak_rss: int | None = None
    status = "ok"
    return_code: int | None = None
    with stdout_path.open("w", encoding="utf-8") as stdout_handle, stderr_path.open("w", encoding="utf-8") as stderr_handle:
        process = subprocess.Popen(command, cwd=workspace, env=environment, text=True, stdout=stdout_handle, stderr=stderr_handle, start_new_session=True)
        deadline = started + timeout_seconds
        while process.poll() is None:
            rss = process_tree_rss_kb(process.pid)
            if rss is not None: peak_rss = max(peak_rss or 0, rss)
            if time.perf_counter() >= deadline:
                status = "timeout"
                terminate_process_group(process)
                break
            time.sleep(0.05)
        if status != "timeout":
            return_code = process.wait()
            if return_code != 0: status = "error"
            terminate_process_group(process)
    wall_ms = round((time.perf_counter() - started) * 1000, 3)
    return stdout_path.read_text(encoding="utf-8"), stderr_path.read_text(encoding="utf-8"), return_code, status, wall_ms, peak_rss


def evaluate_check(check: dict[str, Any], answer: str, calls: list[dict[str, Any]], workspace: Path, changed: list[str], scripts_dir: Path) -> dict[str, Any]:
    kind = check["type"]
    comment = ""
    passed = False
    names = [str(call.get("name", "")) for call in calls]
    try:
        if kind == "response_json_equals":
            parsed = json.loads(answer)
            passed = parsed == check["value"]
            comment = f"expected exact JSON {check['value']!r}"
        elif kind == "response_contains":
            missing = [value for value in check["values"] if value not in answer]
            passed, comment = not missing, f"missing: {missing}"
        elif kind == "response_contains_any":
            lowered = answer.lower()
            passed = any(value.lower() in lowered for value in check["values"])
            comment = f"expected any of {check['values']!r}"
        elif kind == "response_not_contains":
            found = [value for value in check["values"] if value in answer]
            passed, comment = not found, f"forbidden values found: {found}"
        elif kind == "evidence_paths_exist":
            cited = sorted(set(re.findall(r"(?:(?:click|itsdangerous)/)?(?:src|tests|tools)/[A-Za-z0-9_./-]+", answer)))
            missing = [path for path in cited if not (workspace / path.rstrip(".,:;)")).is_file()]
            passed = len(cited) >= int(check.get("minimum", 1)) and not missing
            comment = f"cited={cited}; missing={missing}; minimum={check.get('minimum', 1)}"
        elif kind == "no_changes":
            passed, comment = not changed, f"changed files: {changed}"
        elif kind == "max_changed_files":
            passed, comment = len(changed) <= int(check["value"]), f"changed {len(changed)}, max {check['value']}"
        elif kind == "file_exists":
            passed = (workspace / check["path"]).is_file()
            comment = f"required file {check['path']}"
        elif kind == "file_contains":
            path = workspace / check["path"]
            passed = path.is_file() and check["value"] in path.read_text(encoding="utf-8")
            comment = f"{check['path']} must contain {check['value']!r}"
        elif kind == "file_not_contains":
            path = workspace / check["path"]
            passed = path.is_file() and check["value"] not in path.read_text(encoding="utf-8")
            comment = f"{check['path']} must not contain {check['value']!r}"
        elif kind == "tool_required_any":
            passed = any(name in check["names"] for name in names)
            comment = f"tool names {names!r}; expected any {check['names']!r}"
        elif kind == "tool_forbidden":
            found = [name for name in names if name in check["names"]]
            passed, comment = not found, f"forbidden calls: {found}"
        elif kind == "tool_required_prefix":
            passed = any(name.startswith(check["prefix"]) for name in names)
            comment = f"expected prefix {check['prefix']!r}; got {names!r}"
        elif kind == "verifier":
            process = subprocess.run(
                [sys.executable, str(scripts_dir / "task_verifier.py"), "--task", check["task"], "--workspace", str(workspace), "--config-json", json.dumps(check.get("config", {}), sort_keys=True)],
                text=True, capture_output=True, timeout=60, check=False,
            )
            passed = process.returncode == 0
            try:
                detail = json.loads(process.stdout.strip().splitlines()[-1])
                comment = detail.get("comment", "")
            except (json.JSONDecodeError, IndexError):
                comment = (process.stdout + process.stderr)[-2000:]
        else:
            comment = f"unknown check type: {kind}"
    except Exception as error:
        passed, comment = False, f"check error: {type(error).__name__}: {error}"
    return {"key": kind, "score": passed, "weight": check["weight"], "comment": comment}


def not_applicable_trial(suite: dict[str, Any], task: dict[str, Any], args: argparse.Namespace, run_id: str, trial: int, missing: list[str], output: Path) -> dict[str, Any]:
    now = datetime.now(timezone.utc).isoformat()
    return {
        "schema_version": 1, "runner_version": RUNNER_VERSION, "benchmark_date": date.today().isoformat(), "suite_review": {"last_reviewed_at": suite["last_reviewed_at"], "review_due_at": suite["review_due_at"], "fresh": date.fromisoformat(suite["review_due_at"]) >= date.today()}, "suite_visibility": suite["visibility"], "suite_id": suite["suite_id"], "suite_version": suite["suite_version"],
        "run_id": run_id, "started_at": now, "finished_at": now,
        "model": {"id": args.model, "agent": args.agent, "metadata": {}},
        "task": {key: task[key] for key in ["id", "title", "tier", "category", "requirements"]}, "trial": trial,
        "status": "not_applicable", "not_applicable_reason": f"missing capabilities: {', '.join(missing)}",
        "timing": {"wall_ms": 0, "cpu_user_ms": None, "cpu_system_ms": None, "peak_rss_kb": None},
        "usage": {"prompt_chars": len(task["prompt"]), "response_chars": 0, "tool_result_chars": 0, "input_tokens": None, "output_tokens": None, "cache_read_tokens": None, "cache_write_tokens": None, "cache_hit_ratio": None},
        "trajectory": {"tool_calls": [], "tool_call_count": 0, "failed_tool_calls": 0, "retries": 0},
        "workspace": {"changed_files": [], "change_count": 0, "baseline_hash": "", "final_hash": ""},
        "grading": {"deterministic": {"score": None, "passed": None, "checks": []}, "judge": {"status": "not_run", "score": None}, "efficiency": {"status": "pending_aggregation", "score": None}, "composite": None},
        "artifacts": {"trial_json": str(output), "stdout": "", "stderr": "", "prompt": ""},
    }


def run_trial(suite: dict[str, Any], task: dict[str, Any], fixture: Path, scripts_dir: Path, args: argparse.Namespace, run_id: str, trial: int, model_root: Path) -> dict[str, Any]:
    task_root = model_root / task["id"] / f"trial-{trial}"
    workspace = task_root / "workspace"
    artifacts = task_root / "artifacts"
    shutil.copytree(
        fixture,
        workspace,
        ignore=shutil.ignore_patterns(".git", ".pytest_cache", "__pycache__", ".mypy_cache"),
    )
    for mutation in task.get("fixture_mutations", []):
        target = (workspace / mutation["path"]).resolve()
        if workspace.resolve() not in target.parents or not target.is_file():
            raise ValueError(f"unsafe or missing fixture mutation target: {mutation['path']}")
        content = target.read_text(encoding="utf-8")
        count = content.count(mutation["old"])
        if count != 1:
            raise ValueError(f"fixture mutation expected one match in {mutation['path']}, found {count}")
        target.write_text(content.replace(mutation["old"], mutation["new"], 1), encoding="utf-8")
    artifacts.mkdir(parents=True, exist_ok=True)
    prompt_file = artifacts / "prompt.md"
    prompt_file.write_text(task["prompt"] + "\n", encoding="utf-8")
    agent_result_path = artifacts / "agent-result.json"
    stdout_path, stderr_path = artifacts / "stdout.txt", artifacts / "stderr.txt"
    trial_path = model_root / "raw" / task["id"] / f"trial-{trial}.json"
    before = manifest(workspace)
    started_at = datetime.now(timezone.utc)
    replacements = {"model": args.model, "prompt_file": str(prompt_file), "workspace": str(workspace), "result_file": str(agent_result_path)}
    # Adapters run with cwd=workspace, so script paths in the matrix cannot be relative.
    # `__REPO_ROOT__` is this checkout (the directory that contains `benchmark/`), portable
    # across machines; `{model}` etc. are still expanded by str.format after that.
    repo_root = Path(__file__).resolve().parents[3]
    command_text = args.agent_command.replace("__REPO_ROOT__", str(repo_root)).format(**replacements)
    command = shlex.split(command_text)
    environment = os.environ.copy()
    environment.update({
        "FREELLAMA_BENCH_MODEL": args.model, "FREELLAMA_BENCH_PROMPT": str(prompt_file),
        "FREELLAMA_BENCH_WORKSPACE": str(workspace), "FREELLAMA_AGENT_RESULT": str(agent_result_path),
        "FREELLAMA_BENCH_TASK_ID": task["id"],
        "FREELLAMA_BENCH_MCP_COMMAND": f"{sys.executable} {scripts_dir / 'mock_mcp_server.py'}",
        "FREELLAMA_MCP_BUILD_CODE": str(task.get("mcp_values", {}).get("build_code", "MCP-2048")),
        "FREELLAMA_MCP_CHECKSUM": str(task.get("mcp_values", {}).get("checksum", "7f3a9c1d")),
    })
    try:
        stdout, stderr, return_code, execution_status, wall_ms, peak_rss = execute_agent(command, workspace, environment, task["timeout_seconds"], stdout_path, stderr_path)
    except OSError as error:
        stdout, stderr, return_code, execution_status, wall_ms, peak_rss = "", str(error), None, "error", 0.0, None
        stdout_path.write_text(stdout, encoding="utf-8"); stderr_path.write_text(stderr, encoding="utf-8")
    adapter: dict[str, Any] = {}
    if agent_result_path.is_file():
        try:
            adapter = load_json(agent_result_path)
            require_valid(adapter, load_json(scripts_dir.parent / "schemas/agent-result.schema.json"), "agent result")
        except (OSError, ValueError, json.JSONDecodeError) as error:
            stderr += f"\ninvalid agent result: {error}"
            execution_status = "error"
    answer = str(adapter.get("final_answer", stdout)).strip()
    calls = adapter.get("tool_calls", []) if isinstance(adapter.get("tool_calls", []), list) else []
    reported_usage = adapter.get("usage", {}) if isinstance(adapter.get("usage", {}), dict) else {}
    provider_metrics = adapter.get("provider_metrics", {}) if isinstance(adapter.get("provider_metrics", {}), dict) else {}
    after = manifest(workspace)
    changed = changed_files(before, after)
    checks = [evaluate_check(check, answer, calls, workspace, changed, scripts_dir) for check in task["checks"]]
    total_weight = sum(float(check["weight"]) for check in checks)
    earned = sum(float(check["weight"]) for check in checks if check["score"])
    deterministic_score = round(100 * earned / total_weight, 3) if total_weight else 0.0
    deterministic_passed = deterministic_score >= 80.0
    call_count, failed_calls, retries, tool_result_chars = tool_stats(calls)
    normalized_usage = {
        "prompt_chars": len(task["prompt"]), "response_chars": len(answer), "tool_result_chars": tool_result_chars,
        "input_tokens": reported_usage.get("input_tokens"), "output_tokens": reported_usage.get("output_tokens"),
        "cache_read_tokens": reported_usage.get("cache_read_tokens"), "cache_write_tokens": reported_usage.get("cache_write_tokens"),
    }
    normalized_usage["cache_hit_ratio"] = cache_ratio(normalized_usage)
    calibrated, calibration_error, calibration = calibration_status(args.judge_calibration, args.judge_model)
    judge = {"status": "not_run", "model": args.judge_model, "score": None, "dimensions": None, "comments": None, "calibrated": False, "calibration": calibration, "calibration_error": calibration_error}
    if args.judge_model and execution_status == "ok":
        judge_input = artifacts / "judge-input.json"
        judge_output = artifacts / "judge-output.json"
        write_json(judge_input, {"task": {"id": task["id"], "prompt": task["prompt"], "rubric": task["rubric"]}, "deterministic_checks": checks, "final_answer": answer, "changed_files": changed, "tool_calls": calls})
        judged = subprocess.run([sys.executable, str(scripts_dir / "distilled_judge.py"), "--input", str(judge_input), "--model", args.judge_model, "--endpoint", args.judge_endpoint, "--output", str(judge_output)], text=True, capture_output=True, timeout=180, check=False)
        if judged.returncode == 0 and judge_output.is_file():
            judge.update(load_json(judge_output))
            judge["calibrated"] = calibrated
            judge["status"] = "calibrated" if calibrated else "advisory"
        else:
            judge["status"] = "error"
            judge["comments"] = (judged.stdout + judged.stderr)[-2000:]
    if execution_status == "timeout":
        status = "timeout"
    elif execution_status == "error":
        status = "error"
    else:
        status = "passed" if deterministic_passed else "failed"
    cpu_user_ms = provider_metrics.get("cpu_user_ms") if isinstance(provider_metrics.get("cpu_user_ms"), (int, float)) else None
    cpu_system_ms = provider_metrics.get("cpu_system_ms") if isinstance(provider_metrics.get("cpu_system_ms"), (int, float)) else None
    judge_points = None
    if isinstance(judge.get("score"), (int, float)):
        judge_points = float(judge["score"]) / 100 * 20
    partial_composite = round(deterministic_score * 0.7 + (judge_points or 0), 3) if judge_points is not None else None
    result = {
        "schema_version": 1, "runner_version": RUNNER_VERSION, "benchmark_date": date.today().isoformat(), "suite_review": {"last_reviewed_at": suite["last_reviewed_at"], "review_due_at": suite["review_due_at"], "fresh": date.fromisoformat(suite["review_due_at"]) >= date.today()}, "suite_visibility": suite["visibility"], "suite_id": suite["suite_id"], "suite_version": suite["suite_version"], "run_id": run_id,
        "started_at": started_at.isoformat(), "finished_at": datetime.now(timezone.utc).isoformat(),
        "model": {"id": args.model, "agent": args.agent, "metadata": adapter.get("model_metadata", {})},
        "task": {key: task[key] for key in ["id", "title", "tier", "category", "requirements"]}, "trial": trial, "status": status,
        "execution": {"command": command, "return_code": return_code, "provider_metrics": provider_metrics},
        "timing": {"wall_ms": wall_ms, "cpu_user_ms": cpu_user_ms, "cpu_system_ms": cpu_system_ms, "peak_rss_kb": peak_rss, "cpu_source": "adapter" if cpu_user_ms is not None or cpu_system_ms is not None else None, "rss_source": "sampled_process_tree" if peak_rss is not None else None},
        "usage": normalized_usage,
        "trajectory": {"tool_calls": calls, "tool_call_count": call_count, "failed_tool_calls": failed_calls, "retries": retries},
        "workspace": {"changed_files": changed, "change_count": len(changed), "baseline_hash": manifest_hash(before), "final_hash": manifest_hash(after)},
        "grading": {"deterministic": {"score": deterministic_score, "passed": deterministic_passed, "checks": checks}, "judge": judge, "efficiency": {"status": "pending_aggregation", "score": None}, "composite": partial_composite},
        "artifacts": {"trial_json": str(trial_path), "stdout": str(stdout_path), "stderr": str(stderr_path), "prompt": str(prompt_file), "workspace": str(workspace)},
    }
    require_valid(result, load_json(scripts_dir.parent / "schemas/trial.schema.json"), "trial result")
    write_json(trial_path, result)
    if args.discard_workspaces:
        shutil.rmtree(workspace)
    return result


def main() -> int:
    args = parse_args()
    if args.trials < 1:
        raise SystemExit("--trials must be at least 1")
    suite_path = args.suite.resolve()
    suite = load_json(suite_path)
    require_valid(suite, load_json(Path(__file__).resolve().parent.parent / "schemas/suite.schema.json"), "suite")
    review_due = date.fromisoformat(suite["review_due_at"])
    if review_due < date.today() and not args.allow_stale_suite:
        raise SystemExit(f"suite review expired on {review_due}; review and advance dates or use --allow-stale-suite for smoke-only work")
    tasks = suite.get("tasks", [])
    requested = set(args.task)
    if requested:
        unknown = requested - {task["id"] for task in tasks}
        if unknown:
            raise SystemExit(f"unknown task ids: {', '.join(sorted(unknown))}")
        tasks = [task for task in tasks if task["id"] in requested]
    fixture = (suite_path.parent / suite["fixture"]).resolve()
    if not fixture.is_dir():
        raise SystemExit(f"fixture directory does not exist: {fixture}")
    if args.dry_run:
        print(json.dumps({"suite": suite["suite_id"], "version": suite["suite_version"], "model": args.model, "tasks": [task["id"] for task in tasks], "trials": args.trials, "capabilities": sorted(args.capability)}, indent=2))
        return 0
    if args.run_id and not re.fullmatch(r"[A-Za-z0-9._-]+", args.run_id):
        raise SystemExit("--run-id contains unsafe characters")
    run_id = args.run_id or (datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8])
    model_root = args.results.resolve() / safe_slug(args.model) / run_id
    capabilities = set(args.capability)
    outcomes = []
    for task in tasks:
        missing = sorted(set(task["requirements"]) - capabilities)
        for trial in range(1, args.trials + 1):
            output = model_root / "raw" / task["id"] / f"trial-{trial}.json"
            if missing:
                result = not_applicable_trial(suite, task, args, run_id, trial, missing, output)
                require_valid(result, load_json(Path(__file__).resolve().parent.parent / "schemas/trial.schema.json"), "not-applicable trial")
                write_json(output, result)
            else:
                result = run_trial(suite, task, fixture, Path(__file__).resolve().parent, args, run_id, trial, model_root)
            outcomes.append({"task": task["id"], "trial": trial, "status": result["status"], "score": result["grading"]["deterministic"]["score"]})
            print(json.dumps(outcomes[-1]), flush=True)
    write_json(model_root / "run.json", {"schema_version": 1, "run_id": run_id, "model": args.model, "agent": args.agent, "suite_id": suite["suite_id"], "suite_version": suite["suite_version"], "capabilities": sorted(capabilities), "trials": args.trials, "outcomes": outcomes})
    failed = any(item["status"] in {"error", "timeout"} for item in outcomes)
    print(json.dumps({"run_id": run_id, "result_root": str(model_root), "error_or_timeout": failed}))
    return 2 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
