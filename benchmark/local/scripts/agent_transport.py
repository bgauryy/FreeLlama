"""Shared Ollama/FreeLlama chat transport for local research adapters."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any


def request_headers() -> dict[str, str]:
    """Build transport headers, reading a bearer token from a file rather than process args."""
    headers = {"content-type": "application/json"}
    token_file = os.environ.get("FREELLAMA_AUTH_TOKEN_FILE", "").strip()
    if not token_file:
        return headers
    token = Path(token_file).read_text(encoding="utf-8").strip()
    if len(token) < 32 or any(character.isspace() for character in token):
        raise ValueError("FREELLAMA_AUTH_TOKEN_FILE must contain one token of at least 32 bytes")
    headers["authorization"] = f"Bearer {token}"
    return headers


def chat_request(
    endpoint: str,
    model: str,
    messages: list[dict[str, Any]],
    options: dict[str, Any],
    think: bool,
    keep_alive: str,
    execution_preference: str = "auto",
    min_placement_evidence: str = "configured",
) -> tuple[str, dict[str, Any]]:
    """Build one direct-Ollama or managed-FreeLlama request.

    `endpoint` selects the transport: a URL ending in `/_freellama/v1/tasks` is managed. The
    benchmark can still target raw Ollama for controlled comparisons; MCP always supplies the
    managed URL so coding-agent turns share routing, admission, physical-placement receipts, and
    adaptive feedback with ordinary `run_task` calls.
    """
    endpoint = endpoint.rstrip("/")
    if endpoint.endswith("/_freellama/v1/tasks"):
        managed_options = {key: value for key, value in options.items() if key != "num_ctx"}
        return endpoint, {
            "task": "coding",
            "objective": "fastest",
            "model": model,
            "context_tokens": options["num_ctx"],
            "execution_preference": execution_preference,
            "min_placement_evidence": min_placement_evidence,
            "messages": messages,
            "keep_alive": keep_alive,
            "request_options": {
                "format": "json",
                "think": think,
                "options": managed_options,
            },
        }
    return f"{endpoint}/api/chat", {
        "model": model,
        "messages": messages,
        "stream": False,
        "format": "json",
        "think": think,
        "keep_alive": keep_alive,
        "options": options,
    }


def unwrap_chat_response(
    payload: dict[str, Any], execution_receipts: list[dict[str, Any]]
) -> dict[str, Any]:
    """Return the Ollama response and retain managed execution proof when present."""
    response = payload.get("response")
    if not isinstance(response, dict):
        return payload
    execution_receipts.append(
        {
            "model": payload.get("route", {}).get("selected_model"),
            "execution": payload.get("execution"),
            "admission": payload.get("admission"),
            "feedback": payload.get("feedback"),
        }
    )
    return response
