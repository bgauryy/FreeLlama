#!/usr/bin/env python3
"""Small dependency-free validator for the JSON Schema subset used by this skill."""

from __future__ import annotations

import json
import re
from typing import Any


class SchemaError(ValueError):
    pass


def _is_type(value: Any, expected: str) -> bool:
    return {
        "object": lambda: isinstance(value, dict),
        "array": lambda: isinstance(value, list),
        "string": lambda: isinstance(value, str),
        "integer": lambda: isinstance(value, int) and not isinstance(value, bool),
        "number": lambda: isinstance(value, (int, float)) and not isinstance(value, bool),
        "boolean": lambda: isinstance(value, bool),
        "null": lambda: value is None,
    }.get(expected, lambda: True)()


def validate(value: Any, schema: dict[str, Any], path: str = "$") -> list[str]:
    errors: list[str] = []
    expected = schema.get("type")
    if expected is not None:
        types = expected if isinstance(expected, list) else [expected]
        if not any(_is_type(value, item) for item in types):
            return [f"{path}: expected type {types}, got {type(value).__name__}"]
    if "const" in schema and value != schema["const"]:
        errors.append(f"{path}: expected constant {schema['const']!r}")
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{path}: expected one of {schema['enum']!r}")
    if isinstance(value, str):
        if len(value) < schema.get("minLength", 0): errors.append(f"{path}: string shorter than minLength")
        if "pattern" in schema and re.search(schema["pattern"], value) is None: errors.append(f"{path}: does not match {schema['pattern']!r}")
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]: errors.append(f"{path}: below minimum")
        if "exclusiveMinimum" in schema and value <= schema["exclusiveMinimum"]: errors.append(f"{path}: not above exclusiveMinimum")
    if isinstance(value, list):
        if len(value) < schema.get("minItems", 0): errors.append(f"{path}: fewer than minItems")
        if "maxItems" in schema and len(value) > schema["maxItems"]: errors.append(f"{path}: more than maxItems")
        if schema.get("uniqueItems"):
            encoded = [json.dumps(item, sort_keys=True, default=str) for item in value]
            if len(encoded) != len(set(encoded)): errors.append(f"{path}: items are not unique")
        if isinstance(schema.get("items"), dict):
            for index, item in enumerate(value): errors.extend(validate(item, schema["items"], f"{path}[{index}]"))
    if isinstance(value, dict):
        required = schema.get("required", [])
        for key in required:
            if key not in value: errors.append(f"{path}: missing required property {key!r}")
        properties = schema.get("properties", {})
        for key, child in value.items():
            if key in properties: errors.extend(validate(child, properties[key], f"{path}.{key}"))
            elif schema.get("additionalProperties") is False: errors.append(f"{path}: unexpected property {key!r}")
    return errors


def require_valid(value: Any, schema: dict[str, Any], label: str) -> None:
    errors = validate(value, schema)
    if errors:
        raise SchemaError(label + ": " + "; ".join(errors[:20]))

