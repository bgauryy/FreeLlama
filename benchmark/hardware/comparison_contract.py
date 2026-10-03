"""Input, profile identity, quality and timing contracts for a matched Ollama comparison."""
from __future__ import annotations

import copy
import hashlib
import json
import statistics
from collections import Counter
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from workload_contract import (as_object, health_errors, load_workload, new_report,
                               phase_summary, positive_number, quality_errors)

MODES = ("direct", "managed")


def load_comparison(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    allowed = {"schema_version", "workload", "repeats_per_phase", "minimum_measured_seconds",
               "maximum_overhead_ratio", "latency_slo_ms"}
    if not isinstance(value, dict) or set(value) - allowed:
        raise ValueError("comparison contains unknown fields or is not an object")
    if type(value.get("schema_version")) is not int or value["schema_version"] != 1:
        raise ValueError("comparison schema_version must be 1")
    if not isinstance(value.get("workload"), str) or not value["workload"]:
        raise ValueError("workload must reference a supplied golden workload file")
    config = load_workload(path.parent / value["workload"])
    if config.get("minimum_speedup") is not None:
        raise ValueError("minimum_speedup applies to serial/parallel qualification, not this comparison")
    repeats = value.get("repeats_per_phase", 1)
    if type(repeats) is not int or not 1 <= repeats <= 100:
        raise ValueError("repeats_per_phase must be an integer in [1, 100]")
    config["repeats_per_phase"] = repeats
    if planned_requests(config) > 10000:
        raise ValueError("comparison cannot plan more than 10000 requests")
    for name in ("minimum_measured_seconds", "maximum_overhead_ratio", "latency_slo_ms"):
        number = value.get(name)
        if number is not None:
            if not positive_number(number) or number <= 0:
                raise ValueError(f"{name} must be finite and positive")
            if name == "minimum_measured_seconds" and number > config["run_budget_seconds"]:
                raise ValueError("minimum_measured_seconds cannot exceed the whole-run budget")
            config[name] = number
    return config


def validate_endpoints(managed: str, direct: str) -> None:
    for endpoint in (managed, direct):
        url = urlsplit(endpoint)
        if (url.scheme != "http" or url.hostname not in ("127.0.0.1", "localhost", "::1") or
                url.username or url.password or url.path or url.query or url.fragment or not url.port):
            raise ValueError("comparison requires explicit owned loopback HTTP endpoints with ports")
    if managed == direct:
        raise ValueError("direct endpoint must be the owned Ollama process, not the managed gateway")


def planned_requests(config: dict[str, Any]) -> int:
    return len(config["cases"]) * (2 + 2 * config["trials"] * config["repeats_per_phase"])


def chat_options(config: dict[str, Any]) -> dict[str, Any]:
    options = {"temperature": 0, "seed": 42, "num_predict": config["num_predict"], "num_ctx": config["num_ctx"]}
    if config["stop"]:
        options["stop"] = config["stop"]
    return options


def chat_body(config: dict[str, Any], case: dict[str, Any]) -> dict[str, Any]:
    message = {"role": "user", "content": case["prompt"]}
    if "image_base64" in case:
        message["images"] = [case["image_base64"]]
    return {"model": config["model"], "messages": [message], "stream": False, "think": False,
            "keep_alive": f"{config['keep_alive_seconds']}s", "options": chat_options(config)}


def digest_json(value: Any) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def new_comparison_report(config: dict[str, Any], endpoint: str) -> dict[str, Any]:
    report = new_report(config, endpoint)
    report.update(qualification="matched_direct_managed_ollama", direct_endpoint=config["direct_endpoint"],
                  profile=None, warmup=None, pairs=[], requests={}, journal_applied=0)
    report["performance"] = {
        "primary_kpi": "managed_over_direct_wall_ratio", "engine_ranking": "not evaluated",
        "maximum_overhead_ratio": config.get("maximum_overhead_ratio"),
        "minimum_measured_seconds": config.get("minimum_measured_seconds"),
        "latency_slo_ms": config.get("latency_slo_ms"), "arrival_policy": "closed loop, rolling bounded concurrency",
        "cache_policy": "one pass per path before timing; shared runner and prefix cache; no resets; alternating pairs",
        "cache_hit_rate": None, "energy_joules": None, "thermal_attribution": None,
        "time_to_first_token": "not measured: nonstreaming completion",
        "placement_scope": "direct: phase-bracketing /api/ps; managed: returned post-execution observation",
        "isolation_scope": "operator-owned process; API snapshots cannot exclude other direct clients",
        "timing_scope": "batch wall includes request dispatch and incremental receipt writes; profile reads and full receipt serialization excluded",
        "percentile_method": "nearest rank p95, descriptive sample counts; no statistical significance claim",
        "sources": ["https://docs.ollama.com/api/chat", "https://docs.ollama.com/api/ps",
                    "https://docs.ollama.com/faq", "https://docs.vllm.ai/en/latest/cli/bench/serve/"],
    }
    return report


def resident(snapshot: dict[str, Any], model: str) -> dict[str, Any]:
    matches = [item for item in model_entries(snapshot.get("ps"))
               if isinstance(item, dict) and item.get("name", item.get("model")) == model]
    return matches[0] if len(matches) == 1 else {}


def model_entries(value: Any) -> list[Any]:
    models = as_object(value).get("models")
    return models if isinstance(models, list) else []


def profile_identity(snapshot: dict[str, Any], config: dict[str, Any]) -> dict[str, Any]:
    status = as_object(snapshot.get("status"))
    process = as_object(as_object(status.get("ollama")).get("config"))
    loaded = resident(snapshot, config["model"])
    return {"version": as_object(snapshot.get("version")).get("version"),
            "upstream": as_object(as_object(status.get("backends")).get("gpu")).get("upstream"),
            "process": {k: process.get(k) for k in ("process_inspection", "effective", "settings")},
            "resident": {k: loaded.get(k) for k in ("digest", "context_length", "size", "size_vram")},
            "runtime": as_object(snapshot.get("runtime")).get("settings"),
            "costs": as_object(as_object(snapshot.get("health")).get("admission")).get("costs")}


def profile_errors(snapshot: Any, config: dict[str, Any]) -> list[str]:
    snapshot = as_object(snapshot)
    failures = health_errors(snapshot.get("health"))
    status = as_object(snapshot.get("status"))
    host = as_object(status.get("host"))
    # A resident runner has no new load reservation. Only the documented low-memory hysteresis
    # hold is compatible; any pressure/swap/load/thermal hold still stops this experiment.
    reserve = as_object(as_object(as_object(snapshot.get("health")).get("admission")).get("resources")).get("hold_reserve_bytes")
    low_only = (host.get("holding") is True and host.get("reasons") == ["low_available_memory"] and
                host.get("memory_pressure") == "normal" and host.get("thermal_throttled") is not True and
                positive_number(host.get("effective_available_bytes")) and
                positive_number(reserve) and host["effective_available_bytes"] >= reserve)
    if low_only:
        failures = [error for error in failures if "host resource hold" not in error]
    if host.get("holding") is not False and not low_only:
        failures.append("host resource guard is holding or unknown")
    if host.get("memory_pressure") != "normal" or host.get("thermal_throttled") is True:
        failures.append("host pressure/thermal guard is not ready")
    if not positive_number(host.get("sample_age_ms")) or host["sample_age_ms"] > 2000:
        failures.append("host sample is missing or stale")
    backend = as_object(as_object(status.get("backends")).get("gpu"))
    if backend.get("upstream") != config["direct_endpoint"]:
        failures.append("managed GPU upstream differs from direct endpoint")
    admission = as_object(backend.get("admission"))
    if any(admission.get(name) != 0 for name in ("active_units", "queue_depth", "resource_waiters")):
        failures.append("managed backend is not idle")
    if as_object(backend.get("circuit")).get("state") != "closed":
        failures.append("managed backend circuit is not closed")
    raw = as_object(status.get("raw_proxy"))
    if raw.get("active") != 0 or raw.get("waiting") != 0:
        failures.append("raw proxy is not idle")
    process = as_object(as_object(status.get("ollama")).get("config"))
    inspection = process.get("process_inspection", "")
    if not isinstance(inspection, str) or not inspection.startswith("observed:") or config["direct_endpoint"] not in inspection:
        failures.append("endpoint-attributed Ollama process was not observed")
    if process.get("observation_scope") != "process_snapshot":
        failures.append("Ollama process observation scope missing")
    observed = process.get("observed_at")
    captured = snapshot.get("observed_at_unix_seconds")
    if not positive_number(observed) or not positive_number(captured) or not -1 <= captured - observed <= 15:
        failures.append("Ollama process snapshot missing or stale")
    settings = as_object(process.get("settings"))
    parallel = as_object(settings.get("OLLAMA_NUM_PARALLEL"))
    if parallel.get("source") != "process" or parallel.get("value") != str(config["concurrency"]):
        failures.append("observed Ollama parallelism must equal requested concurrency")
    if as_object(process.get("effective")).get("num_parallel") != config["concurrency"]:
        failures.append("effective Ollama parallelism differs from requested concurrency")
    if not settings or any(as_object(s).get("source") not in ("process", "ollama_default") for s in settings.values()):
        failures.append("process settings use unobserved fallback sources")
    loaded = resident(snapshot, config["model"])
    if (loaded.get("context_length") != config["num_ctx"] or not positive_number(loaded.get("size")) or
            loaded.get("size", 0) <= 0 or not positive_number(loaded.get("size_vram")) or
            loaded.get("size_vram") != loaded.get("size")):
        failures.append("exact model is not resident entirely on GPU at requested context")
    digest = loaded.get("digest")
    installed = [item for item in model_entries(snapshot.get("tags")) if isinstance(item, dict)
                 and item.get("name", item.get("model")) == config["model"] and item.get("digest") == digest]
    if not isinstance(digest, str) or not digest:
        failures.append("resident digest unavailable; installed revision comparison unqualified")
    elif len(installed) != 1:
        failures.append("resident digest does not match one exact installed tag")
    if not isinstance(as_object(snapshot.get("version")).get("version"), str):
        failures.append("Ollama version missing")
    if not as_object(as_object(snapshot.get("runtime")).get("settings")):
        failures.append("effective managed runtime settings missing")
    return failures


def comparison_quality_errors(result: dict[str, Any], case: dict[str, Any], config: dict[str, Any],
                              mode: str, profile: dict[str, Any]) -> list[str]:
    if result.get("status") != "success":
        return [f"unfinished/failed request: {result.get('error', result.get('status'))}"]
    payload = as_object(result.get("payload"))
    response = payload if mode == "direct" else as_object(payload.get("response"))
    failures = quality_errors(result, case, config["model"]) if mode == "managed" else []
    if response.get("model") != config["model"] or response.get("done") is not True:
        failures.append("raw completion must identify exact model and done=true")
    content = as_object(response.get("message")).get("content")
    if not isinstance(content, str) or " ".join(content.split()) != " ".join(case["expected_text"].split()):
        failures.append("raw completion does not match exact normalized golden")
    if not positive_number(result.get("wall_seconds")) or result["wall_seconds"] <= 0:
        failures.append("request completion has no positive measured latency")
    logical_hash = digest_json(chat_body(config, case))
    if result.get("logical_request_sha256") != logical_hash:
        failures.append("request identity differs from the supplied logical payload")
    if mode == "direct" and result.get("request_sha256") != logical_hash:
        failures.append("direct wire request differs from the supplied logical payload")
    if mode == "managed":
        execution = as_object(payload.get("execution"))
        observation = as_object(execution.get("observation"))
        if execution.get("upstream") != config["direct_endpoint"] or execution.get("runtime_options") != chat_options(config):
            failures.append("managed applied upstream/options differ from direct logical request")
        digest = resident(profile, config["model"]).get("digest")
        if execution.get("model_digest") != digest or observation.get("digest") != digest or observation.get("context_length") != config["num_ctx"]:
            failures.append("managed execution digest/context differs from frozen profile")
    return failures


def summarize_comparison(report: dict[str, Any], config: dict[str, Any]) -> None:
    failures = list(report["fatal_failures"])
    profile = as_object(report.get("profile"))
    failures.extend(profile_errors(profile, config))
    identity = profile_identity(profile, config)
    shape_errors, phases = [], []
    warmup = report.get("warmup")
    if not warmup:
        shape_errors.append("missing equal warmup passes")
    if len(report["pairs"]) != config["trials"]:
        shape_errors.append("missing exactly configured matched pairs")
    all_pairs = ([(-1, warmup)] if warmup else []) + list(enumerate(report["pairs"]))
    cases = {case["id"]: case for case in config["cases"]}
    completed = correct = submitted = 0
    for index, pair in all_pairs:
        order = ["direct", "managed"] if index <= 0 or index % 2 == 0 else ["managed", "direct"]
        if pair.get("order") != order or set(pair) != {"order", *MODES}:
            shape_errors.append(f"pair{index}: requires the declared alternating order and both paths")
        repeats = 1 if index == -1 else config["repeats_per_phase"]
        expected = Counter((case_id, repeat) for repeat in range(repeats) for case_id in cases)
        for mode in MODES:
            if mode not in pair:
                continue
            phase = pair[mode]
            phases.append((index, mode, phase))
            actual = Counter((r.get("id"), r.get("repeat")) for r in phase.get("cases", []))
            if actual != expected or not positive_number(phase.get("wall_seconds")) or phase.get("wall_seconds", 0) <= 0:
                shape_errors.append(f"pair{index}/{mode}: exact case/repeat set or positive batch wall missing")
            for boundary in ("before", "after"):
                snapshot = as_object(phase.get(boundary))
                failures.extend(f"pair{index}/{mode}/{boundary}: {error}" for error in profile_errors(snapshot, config))
                if profile_identity(snapshot, config) != identity:
                    failures.append(f"pair{index}/{mode}/{boundary}: process/model/runtime profile changed")
            for result in phase.get("cases", []):
                submitted += 1
                completed += result.get("status") in ("success", "error")
                errors = (comparison_quality_errors(result, cases[result["id"]], config, mode, profile)
                          if result.get("id") in cases else ["unknown case"])
                correct += not errors
                failures.extend(f"pair{index}/{mode}/{result.get('id')}: {error}" for error in errors)
    planned = planned_requests(config)
    complete = completed == planned and not shape_errors
    if not complete:
        shape_errors.append(f"incomplete comparison: {completed}/{planned} terminal receipts")
    failures.extend(shape_errors)
    performance = report["performance"]
    measured = [(mode, phase) for index, mode, phase in phases if index >= 0]
    performance["measured_seconds"] = sum(phase.get("wall_seconds", 0) for _, phase in measured)
    duration = config.get("minimum_measured_seconds")
    performance["duration_target_met"] = duration is None or performance["measured_seconds"] >= duration
    if not performance["duration_target_met"]:
        failures.append("measured duration below declared minimum; sustained qualification incomplete")
    performance["pair_count"] = len(report["pairs"])
    valid_profile = not failures
    ratios = [pair["managed"]["wall_seconds"] / pair["direct"]["wall_seconds"] for pair in report["pairs"]
              if all(mode in pair and positive_number(pair[mode].get("wall_seconds")) and pair[mode]["wall_seconds"] > 0 for mode in MODES)]
    ratio = statistics.median(ratios) if valid_profile and len(ratios) == config["trials"] else None
    performance["managed_over_direct_wall_ratio"] = ratio
    performance["paired_wall_ratios"] = ratios if valid_profile else []
    target = config.get("maximum_overhead_ratio")
    if target is not None and ratio is None:
        failures.append("managed/direct wall ratio unavailable; declared overhead gate unqualified")
    elif target is not None and ratio > target:
        failures.append(f"managed/direct wall ratio {ratio} exceeds declared maximum {target}")
    for mode in MODES:
        normalized = []
        for path, phase in measured:
            if path != mode:
                continue
            item = copy.deepcopy(phase)
            if mode == "direct":
                for result in item["cases"]:
                    raw = as_object(result.get("payload"))
                    fields = {"output_tokens": "eval_count", "prompt_tokens": "prompt_eval_count",
                              "cached_prompt_tokens": "prompt_eval_cached_count", "load_duration_ns": "load_duration",
                              "prompt_duration_ns": "prompt_eval_duration", "output_duration_ns": "eval_duration"}
                    result["payload"] = {"metrics": {name: raw.get(field) for name, field in fields.items()}}
            normalized.append(item)
        summary = phase_summary(normalized, not failures)
        results = [r for index, path, phase in phases if path == mode and index >= 0 for r in phase.get("cases", [])]
        good = [r for r in results if r.get("id") in cases and not comparison_quality_errors(r, cases[r["id"]], config, mode, profile)]
        slo = config.get("latency_slo_ms")
        within = [r for r in good if slo is not None and r["wall_seconds"] * 1000 <= slo]
        summary.update(correct_requests=len(good), latency_slo_ms=slo,
                       correctness_qualified_requests_per_minute=summary["qualified_requests_per_minute"],
                       slo_qualified_requests=len(within) if slo is not None else None,
                       slo_misses=len(good) - len(within) if slo is not None else None,
                       slo_pass_rate=len(within) / len(results) if slo is not None and results else None,
                       slo_qualified_requests_per_minute=60 * len(within) / summary["measured_wall_seconds"]
                       if slo is not None and not failures and summary["measured_wall_seconds"] > 0 else None)
        performance[mode] = summary
    report["completion"] = {"planned_requests": planned, "submitted_requests": submitted,
                            "completed_requests": completed, "successful_requests": correct, "complete": complete}
    report["failures"] = failures
    report["verdict"] = "accept" if not failures else "reject"
