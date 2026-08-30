#!/usr/bin/env python3
"""Shared standard-library helpers for the benchmark scripts."""

from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path
import statistics
import tempfile
from typing import Any, Iterable

RUNNER_VERSION = "2.0.0"


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent, delete=False) as handle:
        json.dump(value, handle, indent=2, sort_keys=True, ensure_ascii=False)
        handle.write("\n")
        temporary = Path(handle.name)
    os.replace(temporary, path)


def manifest(root: Path) -> dict[str, str]:
    result: dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        if not path.is_file() or ".git" in path.parts or "__pycache__" in path.parts:
            continue
        relative = path.relative_to(root).as_posix()
        result[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
    return result


def manifest_hash(files: dict[str, str]) -> str:
    payload = json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()


def changed_files(before: dict[str, str], after: dict[str, str]) -> list[str]:
    return sorted(path for path in set(before) | set(after) if before.get(path) != after.get(path))


def median(values: Iterable[float]) -> float | None:
    items = list(values)
    return statistics.median(items) if items else None


def percentile(values: Iterable[float], quantile: float) -> float | None:
    items = sorted(values)
    if not items:
        return None
    index = (len(items) - 1) * quantile
    lower = math.floor(index)
    upper = math.ceil(index)
    if lower == upper:
        return items[lower]
    return items[lower] * (upper - index) + items[upper] * (index - lower)


def geometric_mean(values: Iterable[float]) -> float | None:
    items = [value for value in values if value > 0]
    return math.exp(sum(math.log(value) for value in items) / len(items)) if items else None


def safe_slug(value: str) -> str:
    slug = "".join(character if character.isalnum() or character in "-_." else "-" for character in value)
    return slug.strip("-.") or "model"


def cache_ratio(usage: dict[str, Any]) -> float | None:
    read = usage.get("cache_read_tokens")
    input_tokens = usage.get("input_tokens")
    if not isinstance(read, int) or not isinstance(input_tokens, int) or input_tokens <= 0:
        return None
    return min(1.0, max(0.0, read / input_tokens))
