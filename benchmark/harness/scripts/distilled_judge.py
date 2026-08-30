#!/usr/bin/env python3
"""Score qualitative benchmark dimensions with a local Ollama judge."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys
import urllib.error
import urllib.request

from _common import load_json, write_json


def extract_json(text: str) -> dict:
    cleaned = re.sub(r"^```(?:json)?\s*|\s*```$", "", text.strip(), flags=re.IGNORECASE)
    try:
        value = json.loads(cleaned)
    except json.JSONDecodeError:
        start, end = cleaned.find("{"), cleaned.rfind("}")
        if start < 0 or end <= start:
            raise
        value = json.loads(cleaned[start : end + 1])
    if not isinstance(value, dict):
        raise ValueError("judge output must be a JSON object")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description="Use an Ollama model as an advisory rubric judge.")
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--model", required=True)
    parser.add_argument("--endpoint", default="http://127.0.0.1:11434")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()
    packet = load_json(args.input)
    instruction = """You are a blind benchmark judge. Grade only observable evidence. Deterministic checks are authoritative. Do not reward verbosity or infer hidden work. Return JSON only with: dimensions (correctness, evidence, coherence, structure; each integer 0-5), comments (one short evidence-backed string per dimension), confidence (low|medium|high), and unknowns (array)."""
    payload = {
        "model": args.model,
        "stream": False,
        "think": False,
        "format": "json",
        "options": {"temperature": 0, "seed": 42},
        "messages": [
            {"role": "system", "content": instruction},
            {"role": "user", "content": json.dumps(packet, ensure_ascii=False, sort_keys=True)},
        ],
    }
    request = urllib.request.Request(
        args.endpoint.rstrip("/") + "/api/chat",
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=args.timeout) as response:
            raw = json.load(response)
        judged = extract_json(raw.get("message", {}).get("content", ""))
        dimensions = judged.get("dimensions", {})
        if set(dimensions) != {"correctness", "evidence", "coherence", "structure"}:
            raise ValueError("judge dimensions must be correctness, evidence, coherence, and structure")
        for key, value in dimensions.items():
            if not isinstance(value, int) or not 0 <= value <= 5:
                raise ValueError(f"judge dimension {key} must be an integer from 0 to 5")
        score = round(sum(dimensions.values()) / 20 * 100, 3)
        output = {
            "model": args.model, "score": score, "dimensions": dimensions,
            "comments": judged.get("comments", {}), "confidence": judged.get("confidence", "unknown"),
            "unknowns": judged.get("unknowns", []), "calibrated": False,
            "usage": {"input_tokens": raw.get("prompt_eval_count"), "output_tokens": raw.get("eval_count"), "load_duration_ns": raw.get("load_duration")},
        }
        write_json(args.output, output)
        print(json.dumps({"output": str(args.output), "score": score}))
        return 0
    except (OSError, ValueError, KeyError, json.JSONDecodeError, urllib.error.URLError) as error:
        print(f"judge failed: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

