#!/usr/bin/env python3
"""Validate suite and trial/aggregate artifacts without external dependencies."""

from __future__ import annotations

import argparse
from datetime import date
import json
from pathlib import Path
import sys
from typing import Any

from _common import load_json
from _schema import validate as validate_schema

TIERS = {"basic", "core", "advanced", "complex"}
CAPABILITIES = {"filesystem", "shell", "tools", "skills", "mcp"}
STATUSES = {"passed", "failed", "error", "timeout", "not_applicable"}


def schema(root: Path, name: str) -> dict[str, Any]:
    return load_json(root / "schemas" / name)


def validate_suite(value: Any, source: Path, root: Path, as_of: date, allow_stale: bool) -> list[str]:
    errors = [f"{source}:{error}" for error in validate_schema(value, schema(root, "suite.schema.json"))]
    if not isinstance(value, dict): return errors
    tasks = value.get("tasks", [])
    if not isinstance(tasks, list) or not tasks:
        errors.append(f"{source}: expected at least one task")
        return errors
    ids = [task.get("id") for task in tasks if isinstance(task, dict)]
    if len(set(ids)) != len(tasks): errors.append(f"{source}: task ids must be unique")
    tier_counts = {tier: 0 for tier in TIERS}
    for task in tasks:
        if not isinstance(task, dict):
            errors.append(f"{source}: task is not an object"); continue
        task_id = task.get("id", "?")
        for key in ["title", "tier", "category", "prompt", "requirements", "timeout_seconds", "checks", "rubric"]:
            if key not in task: errors.append(f"{source}:{task_id}: missing {key}")
        if task.get("tier") not in TIERS: errors.append(f"{source}:{task_id}: invalid tier")
        else: tier_counts[task["tier"]] += 1
        if not set(task.get("requirements", [])).issubset(CAPABILITIES): errors.append(f"{source}:{task_id}: invalid requirement")
        if not task.get("checks"): errors.append(f"{source}:{task_id}: no deterministic checks")
        if not task.get("rubric"): errors.append(f"{source}:{task_id}: no judge rubric")
    missing_tiers = [tier for tier, count in tier_counts.items() if count == 0]
    if missing_tiers: errors.append(f"{source}: every difficulty tier must be represented; missing {missing_tiers}")
    fixture = (source.parent / value.get("fixture", "")).resolve()
    if not fixture.is_dir(): errors.append(f"{source}: fixture does not exist: {fixture}")
    try:
        reviewed = date.fromisoformat(value["last_reviewed_at"])
        due = date.fromisoformat(value["review_due_at"])
        if due < reviewed: errors.append(f"{source}: review_due_at precedes last_reviewed_at")
        if due < as_of and not allow_stale: errors.append(f"{source}: suite review expired on {due}; as-of date is {as_of}")
    except (KeyError, ValueError) as error:
        errors.append(f"{source}: invalid suite review date: {error}")
    return errors


def validate_trial(value: Any, source: Path, root: Path) -> list[str]:
    errors = [f"{source}:{error}" for error in validate_schema(value, schema(root, "trial.schema.json"))]
    if not isinstance(value, dict): return errors
    if value.get("status") not in STATUSES: errors.append(f"{source}: invalid status {value.get('status')!r}")
    deterministic = value.get("grading", {}).get("deterministic", {})
    score = deterministic.get("score")
    if value.get("status") != "not_applicable" and not isinstance(score, (int, float)):
        errors.append(f"{source}: applicable trial requires deterministic score")
    return errors


def validate_aggregate(value: Any, source: Path, root: Path) -> list[str]:
    return [f"{source}:{error}" for error in validate_schema(value, schema(root, "aggregate.schema.json"))]


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate a FreeLlama benchmark suite and result artifacts.")
    parser.add_argument("--suite", required=True, type=Path)
    parser.add_argument("--matrix", type=Path)
    parser.add_argument("--calibration", type=Path)
    parser.add_argument("--results", type=Path)
    parser.add_argument("--as-of", default=date.today().isoformat(), help="Freshness date in YYYY-MM-DD; defaults to today.")
    parser.add_argument("--allow-stale-suite", action="store_true", help="Smoke-only override; publishable runs must not use it.")
    args = parser.parse_args()
    try:
        as_of = date.fromisoformat(args.as_of)
    except ValueError as error:
        raise SystemExit(f"invalid --as-of date: {error}")
    root = Path(__file__).resolve().parent.parent
    errors: list[str] = []
    try:
        errors.extend(validate_suite(load_json(args.suite), args.suite, root, as_of, args.allow_stale_suite))
    except (OSError, json.JSONDecodeError) as error:
        errors.append(f"{args.suite}: {error}")
    checked = 1
    if args.matrix:
        try:
            errors.extend(f"{args.matrix}:{error}" for error in validate_schema(load_json(args.matrix), schema(root, "matrix.schema.json")))
            checked += 1
        except (OSError, json.JSONDecodeError) as error:
            errors.append(f"{args.matrix}: {error}")
    if args.calibration:
        try:
            calibration = load_json(args.calibration)
            errors.extend(f"{args.calibration}:{error}" for error in validate_schema(calibration, schema(root, "judge-calibration.schema.json")))
            if isinstance(calibration, dict) and calibration.get("expires_at") and date.fromisoformat(calibration["expires_at"]) < as_of:
                errors.append(f"{args.calibration}: calibration expired on {calibration['expires_at']}")
            checked += 1
        except (OSError, ValueError, json.JSONDecodeError) as error:
            errors.append(f"{args.calibration}: {error}")
    if args.results:
        for path in sorted(args.results.rglob("*.json")):
            try:
                value = load_json(path)
            except (OSError, json.JSONDecodeError) as error:
                errors.append(f"{path}: {error}"); continue
            if isinstance(value, dict) and "task" in value and "trial" in value:
                errors.extend(validate_trial(value, path, root)); checked += 1
            elif path.name == "aggregate.json" or (isinstance(value, dict) and "common_tasks" in value):
                errors.extend(validate_aggregate(value, path, root)); checked += 1
            elif path.name == "agent-result.json":
                errors.extend(f"{path}:{error}" for error in validate_schema(value, schema(root, "agent-result.schema.json"))); checked += 1
    report = {"valid": not errors, "suite": str(args.suite), "as_of": as_of.isoformat(), "artifacts_checked": checked, "errors": errors}
    print(json.dumps(report, indent=2))
    return 0 if not errors else 1


if __name__ == "__main__":
    raise SystemExit(main())
