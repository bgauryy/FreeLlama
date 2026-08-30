"""Stable telemetry interface."""


def event(name: str, **fields: object) -> dict[str, object]:
    return {"name": name, "fields": fields}

