#!/usr/bin/env python3
"""Run a model-agent matrix, then aggregate and render one dashboard."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys

from _common import load_json, safe_slug
from _schema import require_valid


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the same suite sequentially across a model-agent matrix.")
    parser.add_argument("--matrix", required=True, type=Path)
    parser.add_argument("--suite", required=True, type=Path)
    parser.add_argument("--results", required=True, type=Path)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--judge-endpoint", default="http://127.0.0.1:11434")
    parser.add_argument("--continue-on-error", action="store_true")
    parser.add_argument("--allow-stale-suite", action="store_true")
    parser.add_argument("--discard-workspaces", action="store_true", help="Keep scored artifacts but remove copied repository workspaces.")
    parser.add_argument("--skip-complete", action="store_true", help="Skip models that already have a complete run for this suite.")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    matrix = load_json(args.matrix)
    require_valid(matrix, load_json(Path(__file__).resolve().parent.parent / "schemas/matrix.schema.json"), "matrix")
    models = matrix.get("models", [])
    if not models:
        raise SystemExit("matrix has no models")
    scripts = Path(__file__).resolve().parent
    if args.dry_run:
        print(json.dumps({"suite": str(args.suite), "trials": args.trials, "results": str(args.results), "models": [{"id": model.get("id"), "agent": model.get("agent"), "capabilities": model.get("capabilities", [])} for model in models]}, indent=2))
        return 0
    failures = []
    expected_tasks = {task["id"] for task in load_json(args.suite)["tasks"]}
    for model in models:
        if args.skip_complete:
            runs: dict[str, set[str]] = {}
            for trial_path in (args.results / safe_slug(model["id"])).glob("*/raw/*/trial-*.json"):
                try:
                    trial = load_json(trial_path)
                except (OSError, json.JSONDecodeError):
                    continue
                runs.setdefault(str(trial.get("run_id", "")), set()).add(str(trial.get("task", {}).get("id", "")))
            if any(task_ids == expected_tasks for task_ids in runs.values()):
                print(json.dumps({"model": model["id"], "status": "skipped_complete"}), flush=True)
                continue
        command = [
            sys.executable, str(scripts / "run.py"), "--suite", str(args.suite), "--model", model["id"],
            "--agent", model["agent"], "--agent-command", model["agent_command"], "--trials", str(args.trials), "--results", str(args.results),
        ]
        for capability in model.get("capabilities", []):
            command.extend(["--capability", capability])
        judge_model = model.get("judge_model", matrix.get("judge_model"))
        if judge_model:
            command.extend(["--judge-model", judge_model, "--judge-endpoint", args.judge_endpoint])
            calibration = model.get("judge_calibration", matrix.get("judge_calibration"))
            if calibration:
                command.extend(["--judge-calibration", calibration])
        if args.allow_stale_suite:
            command.append("--allow-stale-suite")
        if args.discard_workspaces:
            command.append("--discard-workspaces")
        process = subprocess.run(command, check=False)
        if process.returncode != 0:
            failures.append({"model": model["id"], "exit_code": process.returncode})
            if not args.continue_on_error:
                break
    aggregate = args.results / "aggregate.json"
    dashboard = args.results / "index.html"
    if any(args.results.rglob("trial-*.json")):
        subprocess.run([sys.executable, str(scripts / "aggregate.py"), "--results", str(args.results), "--output", str(aggregate), "--suite", str(args.suite)], check=True)
        subprocess.run([sys.executable, str(scripts / "render_html.py"), "--aggregate", str(aggregate), "--suite", str(args.suite), "--output", str(dashboard)], check=True)
    print(json.dumps({"models_requested": len(models), "failures": failures, "aggregate": str(aggregate), "dashboard": str(dashboard)}))
    return 2 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
