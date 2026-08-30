#!/usr/bin/env python3
"""Aggregate the latest complete run for each benchmarked model."""

from __future__ import annotations

import argparse
from collections import defaultdict
from datetime import datetime, timezone
import json
from pathlib import Path
from typing import Any

from _common import geometric_mean, load_json, median, percentile, write_json
from _schema import require_valid


def trial_files(root: Path) -> list[tuple[Path, dict[str, Any]]]:
    found = []
    for path in root.rglob("trial-*.json"):
        try:
            value = load_json(path)
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(value, dict) and "task" in value and "trial" in value and "run_id" in value:
            value["_source"] = str(path.resolve())
            found.append((path, value))
    return found


def numeric(values: list[Any]) -> list[float]:
    return [float(value) for value in values if isinstance(value, (int, float))]


def task_summary(trials: list[dict[str, Any]]) -> dict[str, Any]:
    applicable = [trial for trial in trials if trial["status"] != "not_applicable"]
    passed = [trial for trial in applicable if trial["status"] == "passed"]
    ordered = sorted(applicable, key=lambda trial: trial["trial"])
    first = ordered[0]["status"] == "passed" if ordered else None
    first_three = ordered[:3]
    reliable = all(trial["status"] == "passed" for trial in first_three) if len(first_three) == 3 else None
    judge_scores = numeric([trial["grading"]["judge"].get("score") for trial in applicable])
    calibrated = bool(judge_scores) and all(trial["grading"]["judge"].get("calibrated") is True for trial in applicable if isinstance(trial["grading"]["judge"].get("score"), (int, float)))
    context_costs = []
    token_costs = []
    for trial in passed:
        usage = trial["usage"]
        context_costs.append(usage["prompt_chars"] + usage["response_chars"] + usage["tool_result_chars"])
        if isinstance(usage.get("input_tokens"), int) and isinstance(usage.get("output_tokens"), int):
            token_costs.append(usage["input_tokens"] + usage["output_tokens"])
    return {
        "id": trials[0]["task"]["id"], "title": trials[0]["task"]["title"], "tier": trials[0]["task"]["tier"], "category": trials[0]["task"]["category"],
        "applicable": bool(applicable), "trials": len(applicable), "passed_trials": len(passed),
        "pass_rate": len(passed) / len(applicable) if applicable else None, "pass_at_1": first, "pass_power_3": reliable,
        "deterministic_score": median(numeric([trial["grading"]["deterministic"].get("score") for trial in applicable])),
        "judge_score": median(judge_scores), "judge_calibrated": calibrated,
        "median_wall_ms": median(numeric([trial["timing"]["wall_ms"] for trial in passed])),
        "median_context_chars": median(context_costs), "median_total_tokens": median(token_costs),
        "median_tool_calls": median(numeric([trial["trajectory"]["tool_call_count"] for trial in passed])),
        "failed_tool_calls": sum(trial["trajectory"]["failed_tool_calls"] for trial in applicable),
        "retries": sum(trial["trajectory"]["retries"] for trial in applicable),
        "efficiency_score": None, "raw_trials": [trial["_source"] for trial in sorted(trials, key=lambda value: value["trial"])],
    }


def group_metric(tasks: list[dict[str, Any]], field: str) -> dict[str, Any]:
    groups: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for task in tasks:
        groups[task[field]].append(task)
    return {
        name: {
            "applicable_tasks": sum(1 for task in items if task["applicable"]),
            "pass_at_1": sum(1 for task in items if task["pass_at_1"] is True) / max(1, sum(1 for task in items if task["pass_at_1"] is not None)),
            "deterministic_score": median(numeric([task["deterministic_score"] for task in items if task["applicable"]])),
        }
        for name, items in sorted(groups.items())
    }


def apply_efficiency(models: list[dict[str, Any]]) -> None:
    task_ids = sorted({task["id"] for model in models for task in model["tasks"]})
    for task_id in task_ids:
        candidates = []
        for model in models:
            task = next((item for item in model["tasks"] if item["id"] == task_id), None)
            if task and task["applicable"] and task["pass_rate"] >= 0.8 and task["median_wall_ms"] is not None:
                candidates.append(task)
        if not candidates:
            continue
        best_time = min(task["median_wall_ms"] for task in candidates if task["median_wall_ms"] > 0)
        cost_field = "median_total_tokens" if all(task["median_total_tokens"] for task in candidates) else "median_context_chars"
        positive_costs = [task[cost_field] for task in candidates if task[cost_field] and task[cost_field] > 0]
        best_cost = min(positive_costs) if positive_costs else None
        tool_values = [task["median_tool_calls"] for task in candidates if task["median_tool_calls"] is not None]
        best_tools = min(tool_values) if tool_values else None
        for task in candidates:
            time_points = 4 * best_time / task["median_wall_ms"]
            cost_points = 3 * best_cost / task[cost_field] if best_cost and task[cost_field] else 0
            tool_points = 3 if best_tools == task["median_tool_calls"] == 0 else (3 * max(1, best_tools) / max(1, task["median_tool_calls"]) if best_tools is not None else 0)
            task["efficiency_score"] = round(min(10, time_points + cost_points + tool_points), 3)


def build_model(model_id: str, run_id: str, trials: list[dict[str, Any]]) -> dict[str, Any]:
    by_task: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for trial in trials: by_task[trial["task"]["id"]].append(trial)
    tasks = [task_summary(by_task[key]) for key in sorted(by_task)]
    applicable = [trial for trial in trials if trial["status"] != "not_applicable"]
    passed = [trial for trial in applicable if trial["status"] == "passed"]
    applicable_tasks = [task for task in tasks if task["applicable"]]
    reliable_tasks = [task for task in tasks if task["pass_power_3"] is not None]
    total_wall = sum(trial["timing"]["wall_ms"] for trial in applicable)
    usage_fields = ["input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"]
    usage = {field: sum(values) if (values := numeric([trial["usage"].get(field) for trial in applicable])) else None for field in usage_fields}
    cache_ratio = usage["cache_read_tokens"] / usage["input_tokens"] if usage["cache_read_tokens"] is not None and usage["input_tokens"] else None
    usage.update({
        "cache_hit_ratio": cache_ratio,
        "context_chars": sum(trial["usage"]["prompt_chars"] + trial["usage"]["response_chars"] + trial["usage"]["tool_result_chars"] for trial in applicable),
        "tool_calls": sum(trial["trajectory"]["tool_call_count"] for trial in applicable),
        "failed_tool_calls": sum(trial["trajectory"]["failed_tool_calls"] for trial in applicable),
        "retries": sum(trial["trajectory"]["retries"] for trial in applicable),
    })
    return {
        "id": model_id, "agent": trials[0]["model"].get("agent"), "run_id": run_id, "benchmark_date": trials[0].get("benchmark_date"), "trial_budget": max((task["trials"] for task in tasks), default=0),
        "coverage": {"applicable_tasks": len(applicable_tasks), "total_tasks": len(tasks), "rate": len(applicable_tasks) / len(tasks) if tasks else 0},
        "deterministic_pass_rate": len(passed) / len(applicable) if applicable else None,
        "pass_at_1": sum(task["pass_at_1"] is True for task in applicable_tasks) / len(applicable_tasks) if applicable_tasks else None,
        "pass_power_3": sum(task["pass_power_3"] is True for task in reliable_tasks) / len(reliable_tasks) if reliable_tasks else None,
        "timing": {"median_wall_ms": median(numeric([trial["timing"]["wall_ms"] for trial in applicable])), "p95_wall_ms": percentile(numeric([trial["timing"]["wall_ms"] for trial in applicable]), 0.95), "total_wall_ms": total_wall, "successful_tasks_per_hour": len(passed) * 3_600_000 / total_wall if total_wall > 0 else None, "peak_rss_kb": max(numeric([trial["timing"].get("peak_rss_kb") for trial in applicable]), default=None)},
        "usage": usage, "categories": group_metric(tasks, "category"), "tiers": group_metric(tasks, "tier"),
        "judge": {"models": sorted({str(trial["grading"]["judge"].get("model")) for trial in applicable if trial["grading"]["judge"].get("model")}), "scored_trials": (scored_trials := sum(isinstance(trial["grading"]["judge"].get("score"), (int, float)) for trial in applicable)), "calibrated": scored_trials > 0 and all(trial["grading"]["judge"].get("calibrated") is True for trial in applicable if isinstance(trial["grading"]["judge"].get("score"), (int, float)))},
        "missing_capability_tasks": [task["id"] for task in tasks if not task["applicable"]],
        "composite_score": None, "advisory_score": None, "common_task_metrics": None, "tasks": tasks,
    }


def common_metrics(model: dict[str, Any], task_ids: set[str]) -> dict[str, Any]:
    tasks = [task for task in model["tasks"] if task["id"] in task_ids and task["applicable"]]
    return {
        "task_count": len(tasks),
        "pass_at_1": sum(task["pass_at_1"] is True for task in tasks) / len(tasks) if tasks else None,
        "pass_power_3": sum(task["pass_power_3"] is True for task in tasks if task["pass_power_3"] is not None) / max(1, sum(task["pass_power_3"] is not None for task in tasks)) if tasks else None,
        "deterministic_score": median(numeric([task["deterministic_score"] for task in tasks])),
        "median_wall_ms": median(numeric([task["median_wall_ms"] for task in tasks if task["pass_at_1"] is True])),
        "median_context_chars": median(numeric([task["median_context_chars"] for task in tasks if task["pass_at_1"] is True])),
    }


def pairwise(models: list[dict[str, Any]], common: set[str]) -> list[dict[str, Any]]:
    comparisons = []
    comparable = [model for model in models if model["coverage"]["applicable_tasks"] > 0]
    for left_index, left in enumerate(comparable):
        for right in comparable[left_index + 1:]:
            left_tasks = {task["id"]: task for task in left["tasks"] if task["id"] in common}
            right_tasks = {task["id"]: task for task in right["tasks"] if task["id"] in common}
            left_wins = right_wins = ties = 0
            time_ratios = []
            context_ratios = []
            for task_id in sorted(common):
                a, b = left_tasks[task_id], right_tasks[task_id]
                a_pass, b_pass = a["pass_at_1"] is True, b["pass_at_1"] is True
                if a_pass and not b_pass: left_wins += 1
                elif b_pass and not a_pass: right_wins += 1
                else: ties += 1
                if a_pass and b_pass and a["median_wall_ms"] and b["median_wall_ms"]:
                    time_ratios.append(b["median_wall_ms"] / a["median_wall_ms"])
                if a_pass and b_pass and a["median_context_chars"] and b["median_context_chars"]:
                    context_ratios.append(b["median_context_chars"] / a["median_context_chars"])
            comparisons.append({
                "left": left["id"], "right": right["id"], "common_tasks": len(common),
                "correctness": {"left_wins": left_wins, "ties": ties, "right_wins": right_wins},
                "right_over_left_time_geomean": geometric_mean(time_ratios), "right_over_left_context_geomean": geometric_mean(context_ratios),
                "ratio_interpretation": ">1 means left uses less; <1 means right uses less",
            })
    return comparisons


def pareto(models: list[dict[str, Any]]) -> dict[str, Any]:
    comparable = [model for model in models if model["coverage"]["applicable_tasks"] > 0]
    if not comparable: return {"quality_leaders": [], "efficiency_leaders": [], "dominated": []}
    quality = {model["id"]: ((model["common_task_metrics"]["pass_at_1"] or 0), (model["common_task_metrics"]["deterministic_score"] or 0)) for model in comparable}
    speed = {model["id"]: model["timing"].get("successful_tasks_per_hour") or 0 for model in comparable}
    best_quality, best_speed = max(quality.values()), max(speed.values())
    dominated = []
    for model in comparable:
        for other in comparable:
            if other is model: continue
            if quality[other["id"]] >= quality[model["id"]] and speed[other["id"]] >= speed[model["id"]] and (quality[other["id"]] > quality[model["id"]] or speed[other["id"]] > speed[model["id"]]):
                dominated.append(model["id"]); break
    return {"quality_leaders": [key for key, value in quality.items() if value == best_quality], "efficiency_leaders": [key for key, value in speed.items() if value == best_speed], "dominated": sorted(dominated)}


def main() -> int:
    parser = argparse.ArgumentParser(description="Aggregate latest benchmark runs for every model.")
    parser.add_argument("--results", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--suite", type=Path, help="Suite used to determine complete task coverage.")
    parser.add_argument("--allow-partial", action="store_true", help="Allow smoke runs missing some of Q01..Q20; never use for publishable comparisons.")
    args = parser.parse_args()
    discovered: dict[str, dict[str, list[dict[str, Any]]]] = defaultdict(lambda: defaultdict(list))
    for _, trial in trial_files(args.results):
        discovered[trial["model"]["id"]][trial["run_id"]].append(trial)
    if not discovered:
        raise SystemExit(f"no trial JSON found under {args.results}")
    expected_tasks = (
        {task["id"] for task in load_json(args.suite)["tasks"]}
        if args.suite
        else {f"Q{index:02d}" for index in range(1, 21)}
    )
    selected = {}
    for model, runs in discovered.items():
        complete = [run_id for run_id, trials in runs.items() if {trial["task"]["id"] for trial in trials} == expected_tasks]
        if complete:
            selected[model] = max(complete)
        elif args.allow_partial:
            selected[model] = max(runs)
        else:
            raise SystemExit(
                f"model {model} has no complete {len(expected_tasks)}-task run; "
                "use --allow-partial for smoke-only aggregation"
            )
    models = [build_model(model, run_id, discovered[model][run_id]) for model, run_id in sorted(selected.items())]
    apply_efficiency(models)
    for model in models:
        task_scores = []
        advisory_scores = []
        for task in model["tasks"]:
            if not task["applicable"]: continue
            if task["efficiency_score"] is not None and task["deterministic_score"] is not None:
                task_scores.append(task["efficiency_score"])
            if task["judge_score"] is not None and task["deterministic_score"] is not None and task["efficiency_score"] is not None:
                advisory_scores.append(task["deterministic_score"] * 0.7 + task["judge_score"] * 0.2 + task["efficiency_score"])
        model["efficiency_score"] = median(task_scores)
        model["advisory_score"] = round(sum(advisory_scores) / len(advisory_scores), 3) if advisory_scores else None
        if model["judge"]["calibrated"] and model["advisory_score"] is not None:
            model["composite_score"] = model["advisory_score"]
    task_sets = [
        {task["id"] for task in model["tasks"] if task["applicable"]}
        for model in models
        if model["coverage"]["applicable_tasks"] > 0
    ]
    common_tasks = sorted(set.intersection(*task_sets)) if task_sets else []
    common_set = set(common_tasks)
    for model in models:
        model["common_task_metrics"] = common_metrics(model, common_set)
    comparisons = pairwise(models, common_set)
    first_trial = next(iter(next(iter(discovered.values())).values()))[0]
    aggregate = {
        "schema_version": 1, "generated_at": datetime.now(timezone.utc).isoformat(),
        "suite": {"id": first_trial["suite_id"], "version": first_trial["suite_version"], "visibility": first_trial.get("suite_visibility"), "review": first_trial.get("suite_review"), "benchmark_date": first_trial.get("benchmark_date")},
        "methodology": {"runner_version": first_trial.get("runner_version"), "selected_run": f"latest complete {len(expected_tasks)}-task run per model" if not args.allow_partial else "latest complete run, partial smoke fallback allowed", "trials_required_for_publishable_reliability": 3, "quality_gate": "deterministic score >= 80", "weights": {"deterministic": 70, "judge": 20, "efficiency": 10}, "composite_requires_calibrated_judge": True, "cost_aggregation": "per-task medians and geometric mean of paired positive ratios"},
        "models": models, "common_tasks": common_tasks, "pairwise_comparisons": comparisons, "pareto": pareto(models),
        "runs_discovered": {model: sorted(runs) for model, runs in discovered.items()},
    }
    require_valid(aggregate, load_json(Path(__file__).resolve().parent.parent / "schemas/aggregate.schema.json"), "aggregate")
    write_json(args.output, aggregate)
    print(json.dumps({"output": str(args.output), "models": len(models), "common_tasks": len(common_tasks)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
