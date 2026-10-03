#!/usr/bin/env python3
"""Compare a held-out workload directly and through FreeLlama on one owned resident Ollama."""
from __future__ import annotations

import argparse
import copy
import json
import time
from pathlib import Path
from typing import Any

from comparison_contract import (chat_body, comparison_quality_errors, digest_json, load_comparison,
                                 new_comparison_report, profile_errors, profile_identity,
                                 summarize_comparison, validate_endpoints)
from run_workload import call, measure_phase, run_bounded, task_body
from workload_contract import write_report


def apply_event(report: dict[str, Any], event: dict[str, Any]) -> None:
    kind = event["kind"]
    if kind == "profile":
        report["profile"] = event["value"]
    elif kind == "requests":
        report["requests"] = event["value"]
    elif kind == "fatal":
        report["fatal_failures"].append(event["value"])
    else:
        index, mode = event["pair"], event["mode"]
        if index == -1:
            if report["warmup"] is None:
                report["warmup"] = {"order": ["direct", "managed"]}
            pair = report["warmup"]
        else:
            while len(report["pairs"]) <= index:
                number = len(report["pairs"])
                report["pairs"].append({"order": ["direct", "managed"] if number % 2 == 0 else ["managed", "direct"]})
            pair = report["pairs"][index]
        if kind == "phase":
            pair[mode] = event["value"]
        elif kind == "result":
            phase = pair[mode]
            while len(phase["cases"]) <= event["index"]:
                phase["cases"].append({})
            phase["cases"][event["index"]] = event["value"]
            phase["wall_seconds"] = event["wall_seconds"]
        elif kind == "boundary":
            pair[mode]["after"] = event["value"]
            pair[mode]["wall_seconds"] = event["wall_seconds"]
    report["journal_applied"] = event["sequence"]


class ReceiptJournal:
    """Append only changed receipts; full atomic serialization stays outside measured phases."""
    def __init__(self, path: Path, report: dict[str, Any]):
        self.path, self.report, self.seconds = path.with_suffix(".events"), report, 0.0
        self.path.write_text("", encoding="utf-8")

    def emit(self, kind: str, **fields: Any) -> None:
        started = time.monotonic()
        event = {"sequence": self.report["journal_applied"] + 1, "kind": kind, **fields}
        with self.path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(event, separators=(",", ":"), allow_nan=False) + "\n")
        apply_event(self.report, copy.deepcopy(event))
        self.seconds += time.monotonic() - started


def read_receipt(path: Path) -> dict[str, Any]:
    report = json.loads(path.read_text(encoding="utf-8"))
    journal = path.with_suffix(".events")
    if journal.exists():
        for line in journal.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                report["fatal_failures"].append("incremental receipt ended with an incomplete event")
                break
            if event["sequence"] > report["journal_applied"]:
                if event["sequence"] != report["journal_applied"] + 1:
                    report["fatal_failures"].append("incremental receipt event sequence is incomplete")
                    break
                apply_event(report, event)
    return report


def observe(config: dict[str, Any], endpoint: str, token: str | None, deadline: float) -> dict[str, Any]:
    snapshot = {}
    for name, base, path, auth in (
            ("health", endpoint, "/_freellama/v1/health", token),
            ("status", endpoint, "/_freellama/v1/status", token),
            ("runtime", endpoint, "/_freellama/v1/config", token),
            ("version", config["direct_endpoint"], "/api/version", None),
            ("tags", config["direct_endpoint"], "/api/tags", None),
            ("ps", config["direct_endpoint"], "/api/ps", None)):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("whole-run deadline expired during profile observation")
        snapshot[name] = call(base + path, None, auth, min(config["request_timeout_seconds"], remaining))
    snapshot["observed_at_unix_seconds"] = time.time()
    return snapshot


def execute(config: dict[str, Any], case: dict[str, Any], endpoint: str, token: str | None,
            deadline: float, mode: str, profile: dict[str, Any]) -> dict[str, Any]:
    started = time.monotonic()
    result = {"id": case["id"], "repeat": case["repeat"], "status": "success"}
    try:
        remaining = deadline - started
        if remaining <= 0:
            raise TimeoutError("whole-run deadline expired before submission")
        timeout = min(config["request_timeout_seconds"], remaining)
        logical = chat_body(config, case)
        body = logical if mode == "direct" else task_body(config, case, timeout)
        if mode == "managed":
            body.update(min_placement_evidence="observed", max_wait_seconds=min(3, body["timeout_seconds"]))
        result["logical_request_sha256"] = digest_json(logical)
        result["request_sha256"] = digest_json(body)
        result["server_timeout_seconds"] = body.get("timeout_seconds")
        url = config["direct_endpoint"] + "/api/chat" if mode == "direct" else endpoint + "/_freellama/v1/tasks"
        result["payload"] = call(url, body, None if mode == "direct" else token, timeout)
    except Exception as error:
        result.update(status="error", error={"type": type(error).__name__, "message": str(error)})
    result["wall_seconds"] = round(time.monotonic() - started, 6)
    result["quality_failures"] = comparison_quality_errors(result, case, config, mode, profile)
    return result


def measure_comparison(config: dict[str, Any], endpoint: str, token: str | None, output: Path) -> None:
    report = new_comparison_report(config, endpoint)
    write_report(output, report)
    journal = ReceiptJournal(output, report)
    started = time.monotonic()
    deadline = started + config["run_budget_seconds"]
    try:
        profile = observe(config, endpoint, token, deadline)
        journal.emit("profile", value=profile)
        errors = profile_errors(profile, config)
        if errors:
            raise RuntimeError("initial resident profile guard: " + "; ".join(errors))
        journal.emit("requests", value={case["id"]: {"direct": chat_body(config, case),
            "logical_request_sha256": digest_json(chat_body(config, case))} for case in config["cases"]})
        for pair_index in range(-1, config["trials"]):
            order = ["direct", "managed"] if pair_index <= 0 or pair_index % 2 == 0 else ["managed", "direct"]
            for mode in order:
                before = observe(config, endpoint, token, deadline)
                errors = profile_errors(before, config)
                if profile_identity(before, config) != profile_identity(profile, config):
                    errors.append("process/model/runtime changed before phase")
                if errors:
                    raise RuntimeError("pre-phase guard: " + "; ".join(errors))
                phase = {"wall_seconds": 0, "cases": [], "before": before}
                journal.emit("phase", pair=pair_index, mode=mode, value=phase)
                repeats = 1 if pair_index == -1 else config["repeats_per_phase"]
                work = dict(config, cases=[dict(case, repeat=repeat) for repeat in range(repeats) for case in config["cases"]])
                statuses = {}

                def checkpoint() -> None:
                    for index, result in enumerate(phase["cases"]):
                        if statuses.get(index) != result["status"]:
                            journal.emit("result", pair=pair_index, mode=mode, index=index,
                                         value=result, wall_seconds=phase["wall_seconds"])
                            statuses[index] = result["status"]

                def task(work, case, endpoint, token, deadline):
                    return execute(work, case, endpoint, token, deadline, mode, profile)

                ok = measure_phase(work, endpoint, token, deadline, phase, config["concurrency"], checkpoint, execute=task)
                after = observe(config, endpoint, token, deadline)
                journal.emit("boundary", pair=pair_index, mode=mode, value=after, wall_seconds=phase["wall_seconds"])
                errors = profile_errors(after, config)
                if profile_identity(after, config) != profile_identity(profile, config):
                    errors.append("process/model/runtime changed after phase")
                if not ok or errors:
                    raise RuntimeError("phase rejected: " + "; ".join(errors or ["quality, transport or deadline failure"]))
                write_report(output, report)
    except Exception as error:
        journal.emit("fatal", value=f"{type(error).__name__}: {error}")
    finally:
        report["elapsed_seconds"] = round(time.monotonic() - started, 6)
        report["performance"]["incremental_receipt_seconds"] = round(journal.seconds, 6)
        summarize_comparison(report, config)
        write_report(output, report)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--ollama-endpoint", required=True)
    parser.add_argument("--auth-token-file", type=Path)
    parser.add_argument("--comparison", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        managed, direct = args.endpoint.rstrip("/"), args.ollama_endpoint.rstrip("/")
        validate_endpoints(managed, direct)
        config = load_comparison(args.comparison)
        config["direct_endpoint"] = direct
        token = args.auth_token_file.read_text(encoding="utf-8").strip() if args.auth_token_file else None
    except (ValueError, OSError) as error:
        parser.error(str(error))
    report = run_bounded(config, managed, token, args.output, worker=measure_comparison,
                         report_factory=new_comparison_report, report_summary=summarize_comparison,
                         receipt_reader=read_receipt)
    print(json.dumps({"verdict": report["verdict"], "completion": report["completion"],
                      "failures": report["failures"]}, indent=2))
    return 0 if report["verdict"] == "accept" else 1


if __name__ == "__main__":
    raise SystemExit(main())
