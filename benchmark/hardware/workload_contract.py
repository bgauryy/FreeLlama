"""Workload input, deterministic quality gates, and receipt measurements."""

from __future__ import annotations

import base64
import hashlib
import json
import math
import platform
import statistics
from pathlib import Path
from typing import Any


def positive_number(value: Any) -> bool:
    return (type(value) is int and 0 <= value <= 2**64) or (
        type(value) is float and math.isfinite(value) and value >= 0)


def as_object(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def load_workload(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    allowed = {"schema_version", "model", "task", "trials", "concurrency", "num_predict",
               "num_ctx", "request_timeout_seconds", "run_budget_seconds", "stop", "cases",
               "minimum_speedup", "keep_alive_seconds"}
    if not isinstance(value, dict) or set(value) - allowed:
        raise ValueError("workload contains unknown fields or is not an object")
    if type(value.get("schema_version")) is not int or value["schema_version"] != 1:
        raise ValueError("schema_version must be 1")
    if not isinstance(value.get("model"), str) or not value["model"].strip():
        raise ValueError("model must name one exact installed tag")
    if value.get("task") not in ("coding", "completion", "code_repair", "long_context", "vision"):
        raise ValueError("task must be a GPU generation workload")
    limits = {"trials": (3, 100, 3), "concurrency": (2, 16, 2),
              "num_predict": (1, 8192, 512), "num_ctx": (256, 262144, 8192),
              "request_timeout_seconds": (1, 900, 45), "keep_alive_seconds": (1, 300, 30)}
    for name, (minimum, maximum, default) in limits.items():
        number = value.setdefault(name, default)
        if type(number) is not int or not minimum <= number <= maximum:
            raise ValueError(f"{name} must be an integer in [{minimum}, {maximum}]")
    budget = value.setdefault("run_budget_seconds", 180)
    if not positive_number(budget) or not .1 <= budget <= 3600:
        raise ValueError("run_budget_seconds must be finite and in [0.1, 3600]")
    target = value.get("minimum_speedup")
    if target is not None and (not positive_number(target) or target <= 0):
        raise ValueError("minimum_speedup must be finite and positive")
    stops = value.setdefault("stop", [])
    if not isinstance(stops, list) or len(stops) > 16 or any(
            not isinstance(stop, str) or not stop or len(stop) > 512 for stop in stops):
        raise ValueError("stop must contain at most 16 nonempty strings")
    cases = value.get("cases")
    if not isinstance(cases, list) or not 2 <= len(cases) <= 32:
        raise ValueError("cases must contain 2 to 32 distinct workloads")
    if value["concurrency"] > len(cases):
        raise ValueError("concurrency cannot exceed the distinct case count")
    ids, identities = set(), set()
    for case in cases:
        if not isinstance(case, dict) or set(case) - {"id", "prompt", "expected_text", "image"}:
            raise ValueError("case contains unknown fields or is not an object")
        for name in ("id", "prompt", "expected_text"):
            if not isinstance(case.get(name), str) or not case[name].strip():
                raise ValueError(f"case {name} must be a nonempty string")
            if len(case[name]) > (256 if name == "id" else 1024 * 1024):
                raise ValueError(f"case {name} is too large")
        if case["id"] in ids:
            raise ValueError("case ids must be distinct")
        ids.add(case["id"])
        image = b""
        if value["task"] == "vision":
            if not isinstance(case.get("image"), str) or not case["image"]:
                raise ValueError("vision case image must name a local file")
            image_path = path.parent / case["image"]
            if image_path.stat().st_size > 10 * 1024 * 1024:
                raise ValueError("case image exceeds 10 MiB")
            image = image_path.read_bytes()
            if not image:
                raise ValueError("case image is empty")
            case["image"] = str(image_path.resolve())
            case["image_base64"] = base64.b64encode(image).decode()
        elif "image" in case:
            raise ValueError("case image requires task vision")
        identity = hashlib.sha256(case["prompt"].encode() + b"\0" + image).hexdigest()
        if identity in identities:
            raise ValueError("case inputs must be distinct; cloned requests do not qualify")
        identities.add(identity)
        case["input_sha256"] = identity
    return value


def new_report(config: dict[str, Any], endpoint: str) -> dict[str, Any]:
    workload = {key: value for key, value in config.items() if key != "cases"}
    workload["cases"] = [{key: value for key, value in case.items() if key != "image_base64"}
                         for case in config["cases"]]
    return {
        "schema_version": 1, "qualification": "gpu_workload",
        "host": {"system": platform.system(), "release": platform.release(),
                 "machine": platform.machine(), "python": platform.python_version()},
        "endpoint": endpoint, "workload": workload, "health": None, "warmup": None,
        "performance": {"trials": [], "primary_kpi": "quality_qualified_requests_per_minute",
                        "minimum_speedup": config.get("minimum_speedup"),
                        "cache_policy": "identical inputs in both modes after one warmup per case",
                        "cache_hit_rate": None, "measured_hardware_utilization": "unknown",
                        "time_to_first_token": "not measured: nonstreaming tasks"},
        "fatal_failures": [], "failures": [], "verdict": "reject",
        "upstream_cancellation": "unknown",
    }


def write_report(path: Path, report: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def quality_errors(result: dict[str, Any], case: dict[str, Any], model: str) -> list[str]:
    if result.get("status") == "error":
        return [f"transport {result.get('error')}"]
    if result.get("status") != "success":
        return ["submitted request has no completed receipt"]
    payload = as_object(result.get("payload"))
    errors = []
    if as_object(payload.get("route")).get("selected_model") != model:
        errors.append("selected model does not match exact requested tag")
    observation = as_object(as_object(payload.get("execution")).get("observation"))
    if observation.get("status") != "verified" or observation.get("processor") != "gpu":
        errors.append(f"expected verified gpu, got {observation}")
    if not positive_number(as_object(payload.get("admission")).get("queue_wait_ms")):
        errors.append("missing finite admission queue_wait_ms")
    response = as_object(payload.get("response"))
    if response.get("done") is not True:
        errors.append("response done must be true")
    content = as_object(response.get("message")).get("content")
    expected = " ".join(case["expected_text"].split())
    if not isinstance(content, str) or " ".join(content.split()) != expected:
        errors.append(f"expected {expected!r}, got {content!r}")
    return errors


def distribution(values: list[float]) -> dict[str, Any]:
    ordered = sorted(values)
    return {"sample_count": len(values), "p50": statistics.median(values) if values else None,
            "p95": ordered[math.ceil(.95 * len(ordered)) - 1] if values else None}


def phase_summary(phases: list[dict[str, Any]], qualified: bool) -> dict[str, Any]:
    completed = [result for phase in phases for result in phase["cases"]
                 if result.get("status") == "success"]
    wall = sum(phase.get("wall_seconds", 0) for phase in phases)
    summary = {"batch_count": len(phases), "measured_wall_seconds": wall,
               "completed_requests": len(completed),
               "qualified_requests_per_minute": 60 * len(completed) / wall if qualified and wall > 0 else None}
    fields = {"latency_seconds": (None, "wall_seconds", 1),
              "queue_wait_ms": ("admission", "queue_wait_ms", 1),
              "total_admission_wait_ms": ("admission", "total_wait_ms", 1),
              "load_seconds": ("metrics", "load_duration_ns", 1e-9),
              "prefill_seconds": ("metrics", "prompt_duration_ns", 1e-9),
              "decode_seconds": ("metrics", "output_duration_ns", 1e-9)}
    for name, (section, field, scale) in fields.items():
        values = [as_object(as_object(result.get("payload")).get(section)).get(field) if section else result.get(field)
                  for result in completed]
        summary[name] = distribution([value * scale for value in values if positive_number(value)])
    for name in ("prompt_tokens", "cached_prompt_tokens", "output_tokens"):
        values = [as_object(as_object(result.get("payload")).get("metrics")).get(name) for result in completed]
        observed = [value for value in values if type(value) is int and value >= 0]
        summary[name] = {"sample_count": len(observed), "reported_total": sum(observed) if observed else None}
    output = summary["output_tokens"]
    summary["qualified_output_tokens_per_second"] = (
        output["reported_total"] / wall if qualified and wall > 0 and
        output["sample_count"] == len(completed) and completed else None)
    return summary


def health_errors(value: Any) -> list[str]:
    failures = []
    contracts = {"authentication": "optional_bearer_all_routes",
                 "placement_feedback_persistence": "versioned_atomic_snapshot_v1",
                 "placement_observation": "ollama_api_ps_after_execution"}
    health = as_object(value)
    if health.get("status") != "ok":
        failures.append("health: status is not ok")
    for name, expected in contracts.items():
        if as_object(health.get("contracts")).get(name) != expected:
            failures.append(f"health: contract {name} is not {expected}")
    if not as_object(as_object(health.get("feedback")).get("persistence")).get("enabled"):
        failures.append("health: persistent feedback is not enabled")
    resources = as_object(as_object(health.get("admission")).get("resources"))
    if resources.get("holding") is not False:
        failures.append("health: host resource hold is active or unknown")
    return failures


def phase_errors(label: str, phase: dict[str, Any], case_ids: set[str]) -> list[str]:
    failures = []
    ids = [result.get("id") for result in phase.get("cases", [])]
    if len(ids) != len(case_ids) or not all(isinstance(value, str) for value in ids) or set(ids) != case_ids:
        failures.append(f"{label}: case set must contain every supplied case exactly once")
    if not positive_number(phase.get("wall_seconds")) or phase["wall_seconds"] <= 0:
        failures.append(f"{label}: wall_seconds must be a positive measured batch duration")
    return failures


def summarize(report: dict[str, Any], config: dict[str, Any]) -> None:
    failures = list(report["fatal_failures"]) + health_errors(report.get("health"))
    phases = [("warmup", report["warmup"])] if report["warmup"] else []
    trials = report["performance"]["trials"]
    phases.extend((f"trial{index + 1}/{mode}", trial[mode]) for index, trial in enumerate(trials)
                  for mode in ("sequential", "parallel") if mode in trial)
    cases = {case["id"]: case for case in config["cases"]}
    shape_errors = []
    if not report["warmup"]:
        shape_errors.append("warmup: missing required phase")
    if len(trials) != config["trials"]:
        shape_errors.append(f"expected exactly {config['trials']} matched trials, got {len(trials)}")
    for index, trial in enumerate(trials):
        expected_order = ["sequential", "parallel"] if index % 2 == 0 else ["parallel", "sequential"]
        if trial.get("order") != expected_order:
            shape_errors.append(f"trial{index + 1}: order must be {expected_order}")
        if set(trial) != {"order", "sequential", "parallel"}:
            shape_errors.append(f"trial{index + 1}: requires exactly one sequential and one parallel phase")
    completed, successful, submitted = 0, 0, 0
    for label, phase in phases:
        shape_errors.extend(phase_errors(label, phase, set(cases)))
        for result in phase["cases"]:
            submitted += 1
            completed += result.get("status") in ("success", "error")
            errors = (quality_errors(result, cases[result["id"]], config["model"])
                      if result["id"] in cases else ["receipt references an unknown case"])
            successful += not errors
            failures.extend(f"{label}/{result['id']}: {error}" for error in errors)
    planned = len(cases) * (1 + 2 * config["trials"])
    complete = completed == planned and not shape_errors
    failures.extend(shape_errors)
    if completed != planned:
        failures.append(f"incomplete workload: {completed}/{planned} requests have terminal receipts")
    paired = [trial for trial in trials if all(
        mode in trial and len(trial[mode]["cases"]) == len(cases) and
        all(result.get("status") in ("success", "error") for result in trial[mode]["cases"])
        for mode in ("sequential", "parallel"))]
    performance = report["performance"]
    for mode in ("sequential", "parallel"):
        performance[f"{mode}_median_seconds"] = statistics.median(
            trial[mode]["wall_seconds"] for trial in paired) if paired else None
    serial, parallel = performance["sequential_median_seconds"], performance["parallel_median_seconds"]
    performance["speedup"] = serial / parallel if serial is not None and parallel and complete else None
    target = config.get("minimum_speedup")
    if target is not None and (performance["speedup"] is None or performance["speedup"] < target):
        failures.append(f"measured speedup {performance['speedup']} below target {target}")
    for mode in ("sequential", "parallel"):
        performance[mode] = phase_summary([trial[mode] for trial in trials if mode in trial], not failures)
    performance["percentile_method"] = "nearest rank p95; descriptive sample counts, not a statistical qualification"
    performance["timing_scope"] = "client batch wall includes scheduling and checkpoint writes; request latency times HTTP calls"
    report["completion"] = {"planned_requests": planned, "submitted_requests": submitted,
                            "completed_requests": completed, "successful_requests": successful,
                            "complete": complete}
    report["failures"] = failures
    report["verdict"] = "accept" if not failures else "reject"
