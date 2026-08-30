#!/usr/bin/env python3
"""Deterministic mock adapter used only by the harness self-test."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path

argparse.ArgumentParser(description="Run the deterministic Q01 mock agent used by self_test.py.").parse_args()
task = os.environ.get("FREELLAMA_BENCH_TASK_ID")
if task != "Q01":
    raise SystemExit(f"mock agent supports only Q01, got {task}")
answer = json.dumps({"status": "ready", "tasks": 20, "suite": "atlas-v1"}, separators=(",", ":"))
result = {
    "final_answer": answer,
    "tool_calls": [],
    "usage": {"input_tokens": 32, "output_tokens": 14, "cache_read_tokens": 8, "cache_write_tokens": 0},
    "provider_metrics": {"load_ms": 1, "prompt_tokens_per_second": 1000, "decode_tokens_per_second": 500},
}
Path(os.environ["FREELLAMA_AGENT_RESULT"]).write_text(json.dumps(result), encoding="utf-8")
print(answer)
