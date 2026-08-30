"""Configuration resolution."""

import os


def timeout_seconds(config: dict[str, str] | None = None) -> int:
    values = config or {}
    raw = os.environ.get("ATLAS_TIMEOUT", values.get("timeout", "30"))
    return int(raw)

