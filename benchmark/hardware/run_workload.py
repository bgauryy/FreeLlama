#!/usr/bin/env python3
"""Measure a supplied GPU workload with exact quality gates and a client wall deadline."""

from __future__ import annotations

import argparse
import json
import math
import multiprocessing
import tempfile
import time
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from pathlib import Path
from typing import Any
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from workload_contract import health_errors, load_workload, new_report, summarize, write_report


def call(url: str, body: dict[str, Any] | None, token: str | None, timeout: float) -> dict[str, Any]:
    headers = {"accept": "application/json"}
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    if token:
        headers["authorization"] = f"Bearer {token}"
    request = Request(url, data=data, headers=headers, method="POST" if body is not None else "GET")
    try:
        with urlopen(request, timeout=timeout) as response:
            payload = json.load(response, parse_constant=reject_nonfinite)
    except HTTPError as error:
        detail = error.read(8192).decode("utf-8", errors="replace")
        raise RuntimeError(f"HTTP {error.code}: {detail}") from error
    if not isinstance(payload, dict):
        raise ValueError("HTTP response must be a JSON object")
    return payload


def reject_nonfinite(value: str) -> None:
    raise ValueError(f"nonfinite JSON number {value}")


def task_body(config: dict[str, Any], case: dict[str, Any], timeout: float) -> dict[str, Any]:
    options = {"temperature": 0, "seed": 42, "num_predict": config["num_predict"]}
    if config["stop"]:
        options["stop"] = config["stop"]
    body = {"task": config["task"], "objective": "fastest", "model": config["model"],
            "prompt": case["prompt"], "context_tokens": config["num_ctx"],
            "execution_preference": "prefer_gpu", "keep_alive": f"{config['keep_alive_seconds']}s",
            "timeout_seconds": max(1, math.ceil(timeout)),
            "request_options": {"think": False, "options": options}}
    if "image_base64" in case:
        body["images"] = [case["image_base64"]]
    return body


def timed_task(config: dict[str, Any], case: dict[str, Any], endpoint: str,
               token: str | None, deadline: float) -> dict[str, Any]:
    started = time.monotonic()
    result = {"id": case["id"], "status": "success"}
    try:
        remaining = deadline - started
        if remaining <= 0:
            raise TimeoutError("whole-run deadline expired before HTTP submission")
        timeout = min(config["request_timeout_seconds"], remaining)
        result["payload"] = call(f"{endpoint}/_freellama/v1/tasks", task_body(config, case, timeout),
                                 token, timeout)
    except Exception as error:
        result["status"] = "error"
        result["error"] = {"type": type(error).__name__, "message": str(error)}
    result["wall_seconds"] = round(time.monotonic() - started, 6)
    return result


def measure_phase(config: dict[str, Any], endpoint: str, token: str | None, deadline: float,
                  phase: dict[str, Any], workers: int, checkpoint, execute=timed_task) -> bool:
    started = time.monotonic()
    waiting = iter(config["cases"])
    transport_failed = False
    with ThreadPoolExecutor(max_workers=workers) as executor:
        pending = {}

        def submit() -> bool:
            if transport_failed or time.monotonic() >= deadline:
                return False
            case = next(waiting, None)
            if case is None:
                return False
            receipt = {"id": case["id"], "status": "submitted"}
            if "repeat" in case:
                receipt["repeat"] = case["repeat"]
            phase["cases"].append(receipt)
            pending[executor.submit(execute, config, case, endpoint, token, deadline)] = receipt
            checkpoint()
            return True

        for _ in range(workers):
            submit()
        while pending:
            done, _ = wait(pending, return_when=FIRST_COMPLETED)
            for future in done:
                receipt = pending.pop(future)
                receipt.update(future.result())
                transport_failed |= receipt["status"] == "error" or bool(receipt.get("quality_failures"))
                phase["wall_seconds"] = round(time.monotonic() - started, 6)
                checkpoint()
            while len(pending) < workers and submit():
                pass
    phase["wall_seconds"] = round(time.monotonic() - started, 6)
    checkpoint()
    return not transport_failed and len(phase["cases"]) == len(config["cases"])


def measure(config: dict[str, Any], endpoint: str, token: str | None, output: Path) -> None:
    report = new_report(config, endpoint)
    started = time.monotonic()
    deadline = started + config["run_budget_seconds"]

    def checkpoint() -> None:
        report["elapsed_seconds"] = round(time.monotonic() - started, 6)
        summarize(report, config)
        write_report(output, report)

    checkpoint()
    try:
        report["health"] = call(f"{endpoint}/_freellama/v1/health", None, token,
                                min(config["request_timeout_seconds"], max(.001, deadline - time.monotonic())))
        checkpoint()
        if health_errors(report["health"]):
            raise RuntimeError("health guard failed; workload was not submitted")
        report["warmup"] = {"wall_seconds": 0, "cases": []}
        if not measure_phase(config, endpoint, token, deadline, report["warmup"], 1, checkpoint):
            raise RuntimeError("warmup stopped after transport failure or deadline")
        for index in range(config["trials"]):
            order = ["sequential", "parallel"] if index % 2 == 0 else ["parallel", "sequential"]
            trial = {"order": order}
            report["performance"]["trials"].append(trial)
            for mode in order:
                trial[mode] = {"wall_seconds": 0, "cases": []}
                workers = config["concurrency"] if mode == "parallel" else 1
                if not measure_phase(config, endpoint, token, deadline, trial[mode], workers, checkpoint):
                    raise RuntimeError(f"{mode} stopped after transport failure or deadline")
    except Exception as error:
        report["fatal_failures"].append(f"{type(error).__name__}: {error}")
    finally:
        checkpoint()


def run_bounded(config: dict[str, Any], endpoint: str, token: str | None, output: Path,
                worker=measure, report_factory=new_report, report_summary=summarize,
                receipt_reader=None) -> dict[str, Any]:
    report = report_factory(config, endpoint)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="freellama-workload-") as directory:
        checkpoint = Path(directory) / "checkpoint.json"
        write_report(checkpoint, report)
        process = None
        try:
            process = multiprocessing.get_context("spawn").Process(
                target=worker, args=(config, endpoint, token, checkpoint))
            process.start()
        except Exception as error:
            report["fatal_failures"].append(f"measurement worker could not start: {type(error).__name__}: {error}")
        else:
            process.join(max(0, config["run_budget_seconds"] - (time.monotonic() - started)))
            timed_out = process.is_alive()
            if timed_out:
                process.terminate()
                process.join(.2)
                if process.is_alive():
                    process.kill()
                    process.join(.2)
            report = (receipt_reader(checkpoint) if receipt_reader else
                      json.loads(checkpoint.read_text(encoding="utf-8")))
            if timed_out:
                report["fatal_failures"].append("whole-run deadline exceeded; measurement worker terminated")
            elif process.exitcode != 0:
                report["fatal_failures"].append(f"measurement worker exited with code {process.exitcode}")
        if process is not None:
            process.close()
    report["elapsed_seconds"] = round(time.monotonic() - started, 6)
    report_summary(report, config)
    write_report(output, report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", default="http://127.0.0.1:11435")
    parser.add_argument("--auth-token-file", type=Path)
    parser.add_argument("--workload", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        config = load_workload(args.workload)
        token = args.auth_token_file.read_text(encoding="utf-8").strip() if args.auth_token_file else None
    except (ValueError, OSError) as error:
        parser.error(str(error))
    report = run_bounded(config, args.endpoint.rstrip("/"), token, args.output)
    print(json.dumps({"verdict": report["verdict"], "completion": report["completion"],
                      "failures": report["failures"]}, indent=2))
    return 0 if report["verdict"] == "accept" else 1


if __name__ == "__main__":
    raise SystemExit(main())
