#!/usr/bin/env python3
"""Bounded mixed-workload probe. No pulls, configuration writes, or optimization."""
from __future__ import annotations

import argparse
import collections
import datetime as dt
import hashlib
import json
import math
import platform
import queue
import re
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from agent_context import ContextPolicy, fit_to_context

ROOT = Path(__file__).resolve().parents[1]
API = "/_freellama/v1"
SUITE = ROOT / "tasks/mixed-workload.json"
KPI = ROOT / "tasks/mixed-workload-kpi.json"
SCHEMA = 1
MAX_RESPONSE_BYTES = 1024 * 1024
REFUSAL_CODES = {409, 429, 503}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()).hexdigest()


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def percentile(values, quantile):
    values = sorted(v for v in values if number(v))
    if not values:
        return None
    position = (len(values) - 1) * quantile
    lower = int(position)
    upper = min(lower + 1, len(values) - 1)
    return values[lower] + (values[upper] - values[lower]) * (position - lower)


def load_suite(path=SUITE):
    suite = json.loads(Path(path).read_text())
    if not isinstance(suite, dict) or suite.get("schema_version") != SCHEMA:
        raise ValueError("unsupported fixture schema")
    cases = suite.get("cases")
    if not isinstance(cases, list) or not cases or len(cases) > 16:
        raise ValueError("fixture needs 1..16 cases")
    ids = set()
    for case in cases:
        if not isinstance(case, dict) or not isinstance(case.get("id"), str) or case["id"] in ids:
            raise ValueError("invalid or duplicate case id")
        ids.add(case["id"])
        if case.get("kind") not in {"short", "json", "long", "independent"} or case.get("grader") not in {"exact", "json"}:
            raise ValueError("unknown case kind/grader")
        if not isinstance(case.get("prompt"), str) or not case["prompt"] or len(case["prompt"]) > 4096:
            raise ValueError("invalid case prompt")
        if case["grader"] == "exact" and not isinstance(case.get("expected"), str):
            raise ValueError("exact grader requires string expected")
        if case["grader"] == "json" and not isinstance(case.get("expected"), dict):
            raise ValueError("JSON grader requires object expected")
        if case["kind"] == "long":
            rows = case.get("archive_rows")
            if type(rows) is not int or not 1 <= rows <= 180 or not isinstance(case.get("record"), str):
                raise ValueError("invalid bounded long-context fixture")
    dt.date.fromisoformat(suite["review_due_at"])
    return suite


def request(endpoint, path, payload, timeout):
    started = time.monotonic()
    try:
        data = None if payload is None else json.dumps(payload).encode()
        req = urllib.request.Request(endpoint + path, data=data, headers={"Content-Type": "application/json"})
        try:
            response = urllib.request.urlopen(req, timeout=max(.01, timeout))
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw = response.read(MAX_RESPONSE_BYTES + 1)
            if len(raw) > MAX_RESPONSE_BYTES:
                raise ValueError("response exceeds bounded capture limit")
            text = raw.decode("utf-8", errors="replace")
            try:
                body = json.loads(text)
            except ValueError:
                body = {"unparsed_body": text}
            return {"http_status": response.code, "body": body, "latency_seconds": time.monotonic() - started}
    except Exception as error:
        return {"http_status": None, "body": None, "error": str(error), "latency_seconds": time.monotonic() - started}


def start_request(endpoint, path, payload, timeout):
    # Daemon workers let the controller enforce an absolute deadline even if a peer
    # trickles bytes forever. Closing this client does not prove server cancellation.
    result = queue.Queue(maxsize=1)
    threading.Thread(target=lambda: result.put(request(endpoint, path, payload, timeout)), daemon=True).start()
    return result


def bounded_request(endpoint, path, payload, timeout):
    try:
        return start_request(endpoint, path, payload, timeout).get(timeout=timeout)
    except queue.Empty:
        return {"http_status": None, "body": None, "error": "client_deadline; server completion unknown", "latency_seconds": timeout}


def health_fields(body):
    if not isinstance(body, dict):
        raise ValueError("health must be an object")
    resources = body.get("admission", {}).get("resources") if isinstance(body.get("admission"), dict) else None
    if not isinstance(resources, dict) or type(resources.get("holding")) is not bool:
        raise ValueError("health lacks explicit resource holding observation")
    observed = resources.get("observation")
    if not isinstance(observed, dict):
        raise ValueError("health lacks resource observation")
    for key in ("available_memory_bytes", "swap_out_pages", "load_average_one_minute", "logical_cpus"):
        if observed.get(key) is not None and (not number(observed[key]) or observed[key] < 0):
            raise ValueError("malformed health metric: " + key)
    if observed.get("thermal_throttled") is not None and type(observed["thermal_throttled"]) is not bool:
        raise ValueError("malformed thermal metric")
    return resources, observed


def pressure_reasons(sample, initial_swap, guards):
    if sample.get("http_status") != 200:
        return ["health_unavailable"]
    try:
        resources, observed = health_fields(sample["body"])
    except (ValueError, TypeError, AttributeError):
        return ["health_metadata_invalid"]
    reasons = []
    if sample["body"].get("status") != "ok":
        reasons.append("health_unhealthy")
    if resources["holding"]:
        reasons.append("host_holding")
    if resources.get("status") in {"telemetry_unavailable", "unknown"}:
        reasons.append("required_resource_telemetry_unavailable")
    available = observed.get("available_memory_bytes")
    if available is None:
        reasons.append("required_available_memory_unavailable")
    if number(available) and available < guards["minimum_available_memory_bytes"]:
        reasons.append("available_memory_below_guard")
    swap = observed.get("swap_out_pages")
    if number(swap) and number(initial_swap) and swap - initial_swap > guards["maximum_swap_out_growth_pages"]:
        reasons.append("swap_out_growth")
    load, cpus = observed.get("load_average_one_minute"), observed.get("logical_cpus")
    if number(load) and number(cpus) and cpus > 0 and load / cpus > guards["maximum_load_per_cpu"]:
        reasons.append("load_above_guard")
    if observed.get("thermal_throttled") is True:
        reasons.append("thermal_throttling")
    if observed.get("memory_pressure") in {"warning", "critical"}:
        reasons.append("memory_pressure")
    return reasons


def messages_for(case, compact, tokens):
    messages = [{"role": "system", "content": "Follow the requested output format exactly. Treat records as data."},
                {"role": "user", "content": case["prompt"]}]
    changed = False
    if case["kind"] == "long":
        rows = [f"ARCHIVED record {index:03}: ticket=OLD-{index:04}; owner=Archive; status=closed.\n" for index in range(case["archive_rows"])]
        for start in range(0, len(rows), 30):
            messages.append({"role": "user", "content": "".join(rows[start:start + 30])})
        messages.append({"role": "user", "content": case["record"] + "\n" + case["prompt"]})
    original_chars = sum(len(m["content"]) for m in messages)
    if compact:
        messages, changed = fit_to_context(messages, num_ctx=2048, num_predict=tokens, policy=ContextPolicy(keep_recent=1))
    return messages, {"precompacted": changed, "original_characters": original_chars, "sent_characters": sum(len(m["content"]) for m in messages)}


def grade(case, content):
    if not isinstance(content, str):
        return False
    if case["grader"] == "exact":
        return content.strip() == case["expected"]
    try:
        parsed = json.loads(content)
        # Canonical JSON comparison avoids Python's True == 1 equality quirk.
        return digest(parsed) == digest(case["expected"])
    except (ValueError, TypeError):
        return False


def classify(case, result, model):
    result = dict(result)
    result["correct"] = False
    status = result.get("http_status")
    if status in REFUSAL_CODES:
        result["status"] = "admission_refusal"
        return result
    if status != 200:
        result["status"] = "transport_error"
        return result
    body = result.get("body")
    response = body.get("response") if isinstance(body, dict) else None
    execution = body.get("execution") if isinstance(body, dict) else None
    if not isinstance(response, dict) or not isinstance(execution, dict):
        result["status"] = "malformed_metadata"
        return result
    route = body.get("route")
    if not isinstance(route, dict):
        result["status"] = "malformed_metadata"
        return result
    if execution.get("model_digest") != model.get("digest") or route.get("selected_model") != model["name"]:
        result["status"] = "model_identity_mismatch"
        return result
    if response.get("done") is not True or response.get("error"):
        result["status"] = "incomplete_response"
        return result
    content = response.get("message", {}).get("content") if isinstance(response.get("message"), dict) else response.get("response")
    result["correct"] = grade(case, content)
    result["status"] = "correct" if result["correct"] else "quality_failure"
    return result


def summarize(report):
    results = report["tasks"]
    correct = sum(item.get("correct") is True for item in results)
    latencies = [item["latency_seconds"] for item in results if number(item.get("latency_seconds"))]
    observations = []
    for sample in report["health_samples"]:
        try:
            observations.append(health_fields(sample.get("body"))[1])
        except (ValueError, TypeError, AttributeError):
            pass
    metric = lambda key: [obs[key] for obs in observations if number(obs.get(key))]
    available, swap, loads = metric("available_memory_bytes"), metric("swap_out_pages"), metric("load_average_one_minute")
    return {"successful_correct_tasks_per_second": correct / max(report["elapsed_seconds"], .000001),
            "correct": correct, "planned": report["planned_tasks"], "correct_fraction": correct / report["planned_tasks"],
            "status_counts": dict(collections.Counter(item["status"] for item in results)),
            "task_latency_p50_seconds": percentile(latencies, .5), "task_latency_p95_seconds": percentile(latencies, .95),
            "health_latency_p95_seconds": percentile([s["latency_seconds"] for s in report["health_samples"] if s.get("http_status") == 200], .95),
            "wake_jitter_p95_seconds": percentile(report["process_wake_jitter_seconds"], .95),
            "minimum_available_memory_bytes": min(available) if available else None,
            "swap_out_growth_pages": max(0, max(swap) - swap[0]) if len(swap) >= 2 else None,
            "maximum_load_average_one_minute": max(loads) if loads else None,
            "thermal_throttled_observations": [o.get("thermal_throttled") for o in observations],
            "unknown_metrics": [key for key in ("available_memory_bytes", "swap_out_pages", "load_average_one_minute", "thermal_throttled", "logical_cpus")
                                if not observations or any(o.get(key) is None or (key == "logical_cpus" and (not number(o[key]) or o[key] <= 0)) for o in observations)]}


def run(args):
    suite = load_suite()
    contract = json.loads(KPI.read_text())
    config = {name: getattr(args, name) for name in ("trials", "max_wall_seconds", "request_timeout_seconds", "max_output_tokens", "max_parallel", "include_compacted", "context_tokens")}
    config.update(keep_alive="0", health_interval_seconds=1.0, wake_interval_seconds=.02, smoke=args.smoke)
    if args.smoke:
        config["trials"] = 1
    report = {"schema_version": SCHEMA, "created_at": dt.datetime.now(dt.timezone.utc).isoformat(), "endpoint": args.endpoint,
              "suite": suite, "case_hash": digest(suite), "contract": contract, "contract_hash": digest(contract),
              "harness_hash": hashlib.sha256(Path(__file__).read_bytes() + (ROOT / "scripts/agent_context.py").read_bytes()).hexdigest(),
              "workload_config": config, "host_identity_hash": digest([platform.node(), platform.system(), platform.machine()]),
              "models": {}, "health_samples": [], "tasks": [], "process_wake_jitter_seconds": [], "stop_reasons": [],
              "limitations": contract["limitations"] + ["Absolute client deadline cannot prove upstream cancellation; keep_alive=0 asks each managed call to unload its exact model after completion."]}
    jobs = []
    for trial in range(config["trials"]):
        for case in suite["cases"]:
            jobs.append((trial, case, False))
            if args.include_compacted and case["kind"] == "long":
                jobs.append((trial, case, True))
    report["planned_tasks"] = len(jobs)
    started = time.monotonic()
    deadline = started + args.max_wall_seconds
    report["complete"] = False
    if dt.date.today() > dt.date.fromisoformat(suite["review_due_at"]) and not args.smoke:
        report["stop_reasons"].append("suite_review_overdue")
    first = bounded_request(args.endpoint, API + "/health", None, min(3, args.max_wall_seconds))
    first["offset_seconds"] = time.monotonic() - started
    report["health_samples"].append(first)
    try:
        initial_swap = health_fields(first.get("body"))[1].get("swap_out_pages")
    except (ValueError, TypeError, AttributeError):
        initial_swap = None
    report["stop_reasons"].extend(pressure_reasons(first, initial_swap, contract["guardrails"]))
    if not report["stop_reasons"]:
        catalog = bounded_request(args.endpoint, API + "/models", None, max(.01, min(10, deadline - time.monotonic())))
        report["catalog"] = catalog
        try:
            entries = catalog["body"]["models"]
            if catalog["http_status"] != 200 or not isinstance(entries, list):
                raise ValueError("catalog unavailable")
            for role, name in (("primary", args.model), ("independent", args.cpu_model or args.model)):
                found = [entry for entry in entries if isinstance(entry, dict) and entry.get("name") == name]
                if len(found) != 1 or not isinstance(found[0].get("digest"), str) or not found[0]["digest"]:
                    raise ValueError("exact installed model and nonempty digest required: " + name)
                if role == "independent" and args.cpu_model and found[0].get("execution", {}).get("placement") != "cpu":
                    raise ValueError("CPU model must already have an explicit CPU backend assignment")
                if found[0].get("resident") is True:
                    raise ValueError("selected model already resident; keep_alive=0 could disturb another user's resident session")
                report["models"][role] = found[0]
        except (KeyError, TypeError, ValueError, AttributeError) as error:
            report["stop_reasons"].append("preflight: " + str(error))
    next_job = 0
    active = []
    health_pending = None
    health_started = 0
    next_health = time.monotonic()  # Fresh sample after catalog discovery, before dispatch.
    ready_health = False
    expected_wake = time.monotonic()
    try:
        while (next_job < len(jobs) or active) and time.monotonic() < deadline:
            now = time.monotonic()
            report["process_wake_jitter_seconds"].append(max(0, now - expected_wake))
            if report["stop_reasons"] and not active:
                break
            if health_pending is None and now >= next_health:
                health_started = now
                health_pending = start_request(args.endpoint, API + "/health", None, min(2, deadline - now))
            if health_pending is not None:
                try:
                    sample = health_pending.get_nowait()
                except queue.Empty:
                    sample = None
                    if now - health_started >= 2:
                        sample = {"http_status": None, "body": None, "error": "health_deadline", "latency_seconds": now - health_started}
                if sample is not None:
                    sample["offset_seconds"] = now - started
                    report["health_samples"].append(sample)
                    report["stop_reasons"].extend(pressure_reasons(sample, initial_swap, contract["guardrails"]))
                    health_pending = None
                    next_health = now + 1
                    ready_health = True
            for item in active[:]:
                try:
                    result = item["queue"].get_nowait()
                except queue.Empty:
                    result = None
                    if now >= item["deadline"]:
                        result = {"http_status": None, "body": None, "error": "client_deadline; server completion unknown", "latency_seconds": now - item["started"]}
                        report["stop_reasons"].append("request_deadline")
                if result is not None:
                    scored = classify(item["case"], result, item["model"])
                    if scored["status"] == "transport_error":
                        report["stop_reasons"].append("request_transport_error")
                    if scored["status"] == "incomplete_response":
                        report["stop_reasons"].append("request_completion_unknown")
                    if isinstance(scored.get("body"), dict) and scored["body"].get("code") == "resource_admission_unavailable":
                        report["stop_reasons"].append("resource_admission_unavailable")
                    scored.update(item["metadata"])
                    report["tasks"].append(scored)
                    active.remove(item)
            while ready_health and not report["stop_reasons"] and len(active) < args.max_parallel and next_job < len(jobs):
                trial, case, compact = jobs[next_job]
                model = report["models"]["independent" if case["kind"] == "independent" else "primary"]
                messages, context_meta = messages_for(case, compact, args.max_output_tokens)
                payload = {"task": "completion", "objective": "fastest", "model": model["name"], "messages": messages,
                           "context_tokens": args.context_tokens, "keep_alive": "0", "priority": "background" if case["kind"] == "long" else "interactive",
                           "request_options": {"think": False, "options": {"num_predict": args.max_output_tokens, "temperature": 0, "seed": 42}}}
                if case["grader"] == "json":
                    payload["request_options"]["format"] = "json"
                dispatch = time.monotonic()
                budget = min(args.request_timeout_seconds, deadline - dispatch)
                if budget <= 0:
                    break
                active.append({"queue": start_request(args.endpoint, API + "/tasks", payload, budget), "deadline": dispatch + budget,
                               "started": dispatch, "case": case, "model": model,
                               "metadata": {"trial": trial, "case_id": case["id"], "variant": "precompacted" if compact else "original",
                                            "kind": case["kind"], "dispatch_offset_seconds": dispatch - started, "model": model["name"],
                                            "request": payload, "context": context_meta}})
                next_job += 1
            expected_wake = time.monotonic() + .02
            time.sleep(min(.02, max(0, deadline - time.monotonic())))
    except KeyboardInterrupt:
        report["stop_reasons"].append("interrupted")
    if active or next_job < len(jobs):
        if time.monotonic() >= deadline:
            report["stop_reasons"].append("wall_deadline")
        for item in active:
            report["tasks"].append(dict(item["metadata"], status="client_deadline", correct=False, latency_seconds=time.monotonic() - item["started"], server_completion="unknown"))
        for trial, case, compact in jobs[next_job:]:
            report["tasks"].append({"trial": trial, "case_id": case["id"], "variant": "precompacted" if compact else "original", "kind": case["kind"], "status": "not_dispatched", "correct": False})
    report["stop_reasons"] = sorted(set(report["stop_reasons"]))
    report["elapsed_seconds"] = time.monotonic() - started
    report["complete"] = not report["stop_reasons"] and len(report["tasks"]) == len(jobs)
    report["summary"] = summarize(report)
    report["verdict"] = "SMOKE_ONLY" if args.smoke else "MEASUREMENT_ONLY"
    return report


def compare(baseline, candidate):
    result = {"verdict": "INSUFFICIENT_EVIDENCE", "reasons": [], "promotion": "No ACCEPT: public synthetic suite requires independent held-out validation."}
    try:
        for report in (baseline, candidate):
            if report.get("schema_version") != SCHEMA or not report.get("complete") or report.get("stop_reasons"):
                raise ValueError("incomplete, interrupted, or malformed report")
            if report["workload_config"].get("smoke") or report["workload_config"]["trials"] < 3:
                raise ValueError("smoke or fewer than three trials")
            if digest(report["suite"]) != report["case_hash"] or digest(report["contract"]) != report["contract_hash"]:
                raise ValueError("fixture/contract metadata hash mismatch")
            if not isinstance(report.get("harness_hash"), str) or not report["harness_hash"]:
                raise ValueError("missing harness identity")
            expected = {(trial, c["id"], variant) for trial in range(report["workload_config"]["trials"]) for c in report["suite"]["cases"]
                        for variant in (["original", "precompacted"] if c["kind"] == "long" and report["workload_config"]["include_compacted"] else ["original"])}
            observed = [(t["trial"], t["case_id"], t["variant"]) for t in report["tasks"]]
            if len(observed) != len(expected) or set(observed) != expected or report["planned_tasks"] != len(expected):
                raise ValueError("partial or duplicate trial/task coverage")
            if not number(report["elapsed_seconds"]) or report["elapsed_seconds"] <= 0:
                raise ValueError("invalid elapsed budget")
            if report["elapsed_seconds"] > report["workload_config"]["max_wall_seconds"] + .1:
                raise ValueError("wall budget exceeded")
            for role in ("primary", "independent"):
                if not report["models"][role].get("digest"):
                    raise ValueError("missing model digest")
            cases = {case["id"]: case for case in report["suite"]["cases"]}
            for task in report["tasks"]:
                case = cases[task["case_id"]]
                model = report["models"]["independent" if case["kind"] == "independent" else "primary"]
                checked = classify(case, task, model)
                if checked["correct"] != task.get("correct") or checked["status"] != task.get("status"):
                    raise ValueError("raw response does not support task grading")
            recalculated = summarize(report)
            if recalculated != report["summary"]:
                raise ValueError("summary does not match raw evidence")
            if recalculated["unknown_metrics"] or len(report["health_samples"]) < 2 or len(report["process_wake_jitter_seconds"]) < 3:
                raise ValueError("missing resource or responsiveness observations")
        keys = ("case_hash", "contract_hash", "harness_hash", "host_identity_hash", "workload_config")
        mismatch = [key for key in keys if baseline.get(key) != candidate.get(key)]
        for role in ("primary", "independent"):
            for key in ("name", "digest"):
                if baseline["models"][role].get(key) != candidate["models"][role].get(key):
                    mismatch.append(role + "_model_" + key)
        if mismatch:
            return dict(result, verdict="NOT_COMPARABLE", reasons=mismatch)
        guards = baseline["contract"]["guardrails"]
        b, c = baseline["summary"], candidate["summary"]
        baseline_rate = b["successful_correct_tasks_per_second"]
        if baseline_rate <= 0:
            raise ValueError("zero successful baseline; relative improvement undefined")
        failures = []
        for report in (baseline, candidate):
            initial = health_fields(report["health_samples"][0]["body"])[1].get("swap_out_pages")
            for sample in report["health_samples"]:
                failures.extend(pressure_reasons(sample, initial, guards))
        if c["correct_fraction"] < max(b["correct_fraction"], guards["minimum_correct_fraction"]):
            failures.append("correctness_guard")
        for metric, guard in (("task_latency_p95_seconds", "maximum_p95_latency_ratio"), ("health_latency_p95_seconds", "maximum_health_p95_ratio"), ("wake_jitter_p95_seconds", "maximum_wake_jitter_p95_ratio")):
            if not number(b[metric]) or not number(c[metric]):
                raise ValueError("missing latency evidence")
            if c[metric] > b[metric] * guards[guard]:
                failures.append(metric)
        improvement = c["successful_correct_tasks_per_second"] / baseline_rate - 1
        result.update(primary_baseline=baseline_rate, primary_candidate=c["successful_correct_tasks_per_second"], relative_improvement=improvement,
                      verdict="MEASURED_IMPROVEMENT" if improvement >= baseline["contract"]["target_relative_improvement"] and not failures else "NO_MEASURED_IMPROVEMENT", reasons=sorted(set(failures)))
    except (KeyError, TypeError, ValueError, AttributeError, ZeroDivisionError) as error:
        result["reasons"].append(str(error))
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    runner = commands.add_parser("run", help="Run bounded synthetic tasks against an existing FreeLlama server")
    runner.add_argument("--endpoint", required=True)
    runner.add_argument("--model", required=True, help="Exact installed, currently non-resident tag; never pulled")
    runner.add_argument("--cpu-model", help="Optional exact installed, non-resident CPU-assigned model for independent tasks")
    runner.add_argument("--trials", type=int, default=3)
    runner.add_argument("--max-wall-seconds", type=float, default=180)
    runner.add_argument("--request-timeout-seconds", type=float, default=45)
    runner.add_argument("--max-output-tokens", type=int, default=96)
    runner.add_argument("--context-tokens", type=int, default=4096, help="Fixed execution context budget shared by all variants (2048..32768)")
    runner.add_argument("--max-parallel", type=int, default=2)
    runner.add_argument("--include-compacted", action="store_true")
    runner.add_argument("--smoke", action="store_true")
    runner.add_argument("--output", type=Path, required=True)
    comparer = commands.add_parser("compare", help="Compare full raw reports without optimizing or promoting")
    comparer.add_argument("--baseline", type=Path, required=True)
    comparer.add_argument("--candidate", type=Path, required=True)
    comparer.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.output.exists():
        parser.error("output already exists; choose a new receipt path")
    if args.command == "run":
        url = urllib.parse.urlparse(args.endpoint)
        if url.scheme != "http" or url.hostname not in {"localhost", "127.0.0.1", "::1"} or url.username or url.password or url.query or url.fragment or url.path not in {"", "/"}:
            parser.error("endpoint must be an explicit unauthenticated loopback HTTP FreeLlama origin")
        args.endpoint = args.endpoint.rstrip("/")
        for name, low, high in (("trials", 1, 5), ("max_wall_seconds", .1, 600), ("request_timeout_seconds", .1, 90), ("max_output_tokens", 16, 256), ("max_parallel", 1, 2), ("context_tokens", 2048, 32768)):
            if not number(getattr(args, name)) or not low <= getattr(args, name) <= high:
                parser.error(f"{name} must be in [{low}, {high}]")
        if not args.smoke and args.trials < 3:
            parser.error("fewer than three trials requires --smoke")
        report = run(args)
    else:
        report = compare(json.loads(args.baseline.read_text()), json.loads(args.candidate.read_text()))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        json.dump(report, output, indent=2, allow_nan=False)
        output.write("\n")
    print(json.dumps({"report": str(args.output), "verdict": report["verdict"], "summary": report.get("summary"), "reasons": report.get("stop_reasons", report.get("reasons"))}))
    return 0 if args.command == "compare" or report.get("complete") else 2


if __name__ == "__main__":
    raise SystemExit(main())
