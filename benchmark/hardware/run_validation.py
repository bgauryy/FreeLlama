#!/usr/bin/env python3
"""Create a portable live CPU/GPU placement receipt against a prepared FreeLlama host."""

from __future__ import annotations

import argparse
import base64
import json
import platform
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any
from urllib.request import Request, urlopen


def call(url: str, body: dict[str, Any] | None, token: str | None) -> dict[str, Any]:
    headers = {"accept": "application/json"}
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    if token:
        headers["authorization"] = f"Bearer {token}"
    request = Request(url, data=data, headers=headers, method="POST" if body is not None else "GET")
    with urlopen(request, timeout=900) as response:
        return json.load(response)


def timed_call(url: str, body: dict[str, Any], token: str | None) -> dict[str, Any]:
    started = time.monotonic()
    payload = call(url, body, token)
    return {"wall_seconds": round(time.monotonic() - started, 6), "payload": payload}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--endpoint", default="http://127.0.0.1:11435")
    parser.add_argument("--gpu-model", required=True)
    parser.add_argument("--cpu-model", required=True)
    parser.add_argument("--vision-model")
    parser.add_argument("--vision-image", type=Path)
    parser.add_argument("--vision-expected-text")
    parser.add_argument("--vision-stop", action="append", default=[])
    parser.add_argument("--auth-token-file", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    token = args.auth_token_file.read_text(encoding="utf-8").strip() if args.auth_token_file else None
    root = args.endpoint.rstrip("/")
    health = call(f"{root}/_freellama/v1/health", None, token)
    tasks_url = f"{root}/_freellama/v1/tasks"
    gpu = {
        "task": "coding",
        "objective": "fastest",
        "model": args.gpu_model,
        "prompt": "Reply with exactly HARDWARE_GPU_OK.",
        "execution_preference": "prefer_gpu",
        "keep_alive": "5m",
        "request_options": {
            "think": False,
            "options": {"temperature": 0, "num_predict": 32},
        },
    }
    cpu = {
        "task": "embedding",
        "objective": "fastest",
        "model": args.cpu_model,
        "input": ["hardware validation alpha", "hardware validation beta"],
        "execution_preference": "prefer_cpu",
        "keep_alive": "5m",
    }
    started = time.monotonic()
    with ThreadPoolExecutor(max_workers=2) as executor:
        gpu_future = executor.submit(timed_call, tasks_url, gpu, token)
        cpu_future = executor.submit(timed_call, tasks_url, cpu, token)
        gpu_result = gpu_future.result()
        cpu_result = cpu_future.result()
    parallel_wall = round(time.monotonic() - started, 6)

    cases = {"gpu_coding": gpu_result, "cpu_embedding": cpu_result}
    if args.vision_model or args.vision_image:
        if not (args.vision_model and args.vision_image):
            parser.error("--vision-model and --vision-image must be provided together")
        vision_stops = ["```", *(value.replace("\\n", "\n") for value in args.vision_stop)]
        vision = {
            "task": "vision",
            "objective": "fastest",
            "model": args.vision_model,
            "prompt": "Transcribe the visible text. Return plain text only.",
            "images": [base64.b64encode(args.vision_image.read_bytes()).decode()],
            "execution_preference": "prefer_gpu",
            "keep_alive": "5m",
            "request_options": {
                "think": False,
                "options": {"temperature": 0, "num_predict": 512, "stop": vision_stops},
            },
        }
        cases["gpu_vision"] = timed_call(tasks_url, vision, token)

    failures: list[str] = []
    required_contracts = {
        "authentication": "optional_bearer_all_routes",
        "placement_feedback_persistence": "versioned_atomic_snapshot_v1",
        "placement_observation": "ollama_api_ps_after_execution",
    }
    for name, expected in required_contracts.items():
        if health.get("contracts", {}).get(name) != expected:
            failures.append(f"health: contract {name} is not {expected}")
    if not health.get("feedback", {}).get("persistence", {}).get("enabled"):
        failures.append("health: persistent feedback is not enabled")
    expectations = {"gpu_coding": "gpu", "cpu_embedding": "cpu", "gpu_vision": "gpu"}
    for name, result in cases.items():
        observation = result["payload"].get("execution", {}).get("observation", {})
        expected = expectations[name]
        if observation.get("status") != "verified" or observation.get("processor") != expected:
            failures.append(f"{name}: expected verified {expected}, got {observation}")
        if result["payload"].get("admission", {}).get("queue_wait_ms") is None:
            failures.append(f"{name}: missing admission receipt")
    gpu_message = gpu_result["payload"].get("response", {}).get("message", {}).get("content", "")
    if "HARDWARE_GPU_OK" not in gpu_message:
        failures.append(f"gpu_coding: unexpected response {gpu_message!r}")
    embeddings = cpu_result["payload"].get("response", {}).get("embeddings")
    if not isinstance(embeddings, list) or len(embeddings) != 2:
        failures.append("cpu_embedding: expected two embedding vectors")
    if "gpu_vision" in cases:
        vision_text = cases["gpu_vision"]["payload"].get("response", {}).get("message", {}).get("content", "")
        if not vision_text.strip():
            failures.append("gpu_vision: empty transcription")
        if args.vision_expected_text:
            normalized = " ".join(vision_text.split())
            expected = " ".join(args.vision_expected_text.split())
            if normalized != expected:
                failures.append(f"gpu_vision: expected {expected!r}, got {normalized!r}")
    if parallel_wall >= gpu_result["wall_seconds"] + cpu_result["wall_seconds"]:
        failures.append("parallel CPU/GPU wall time did not beat the sum of isolated request durations")

    report = {
        "schema_version": 1,
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "endpoint": root,
        "health": health,
        "parallel_wall_seconds": parallel_wall,
        "cases": cases,
        "vision_expected_text": args.vision_expected_text,
        "failures": failures,
        "verdict": "accept" if not failures else "reject",
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"verdict": report["verdict"], "failures": failures}, indent=2))
    return 0 if not failures else 1


if __name__ == "__main__":
    raise SystemExit(main())
