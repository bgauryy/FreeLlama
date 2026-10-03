"""Shared Ollama/FreeLlama chat transport for local research adapters."""

from __future__ import annotations

import os
import hashlib
import json
import re
from pathlib import Path
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


def _read_identity_json(url: str) -> dict[str, Any]:
    with urlopen(Request(url, headers=request_headers()), timeout=5) as response:
        payload = response.read(1024 * 1024 + 1)
    if len(payload) > 1024 * 1024:
        raise ValueError("model identity catalog exceeds size limit")
    return json.loads(payload)


def resolve_model_identity(
    endpoint: str, model: str, system_prompt: str, *, read_json=None,
) -> dict[str, Any]:
    """Resolve immutable manifest identity once per adapter run, without loading a model.

    Managed catalogs honor exact CPU backend assignment. Failure disables persistent calibration;
    a mutable tag or a hash of the prompt alone must never masquerade as model identity.
    """
    endpoint = endpoint.rstrip("/")
    managed_suffix = "/_freellama/v1/tasks"
    url = (endpoint[:-len(managed_suffix)] + "/_freellama/v1/models"
           if endpoint.endswith(managed_suffix) else endpoint + "/api/tags")
    unknown = {"identity": "", "verified": False, "scope": "current_process_only"}
    try:
        payload = (read_json or _read_identity_json)(url)
        entries = payload.get("models", []) if isinstance(payload, dict) else []
        if not isinstance(entries, list):
            return unknown
        names = {model}
        if ":" not in model.rsplit("/", 1)[-1]:
            names.add(model + ":latest")
        entry = next((item for item in entries if isinstance(item, dict)
                      and item.get("name", item.get("model")) in names), None)
        digest = entry.get("digest") if entry else None
        if not isinstance(digest, str) or not re.fullmatch(r"(?:sha256:)?[0-9a-fA-F]{64}", digest):
            return unknown
        digest = digest.removeprefix("sha256:").lower()
        contract = {"endpoint": endpoint, "model": model, "digest": digest,
                    "system_hash": hashlib.sha256(system_prompt.encode("utf-8")).hexdigest()}
        return {"identity": hashlib.sha256(json.dumps(contract, sort_keys=True).encode()).hexdigest(),
                "verified": True, "scope": "manifest_digest_and_system_prompt", "digest": digest}
    except (OSError, ValueError, TypeError):
        return unknown


def retryable_chat_error(error: Exception) -> bool:
    """Retry known refusals only; a timeout may leave inference running upstream."""
    if isinstance(error, HTTPError):
        return error.code == 503
    return isinstance(error, URLError) and isinstance(error.reason, ConnectionRefusedError)


def chat_error_details(error: Exception) -> tuple[str, dict[str, Any] | None]:
    """Retain bounded application refusal receipts, excluding request/auth payloads."""
    diagnostic = f"{type(error).__name__}: {error}"
    if not isinstance(error, HTTPError):
        return diagnostic, None
    details: dict[str, Any] = {"status": error.code}
    try:
        body = error.read(64 * 1024 + 1)
        if len(body) > 64 * 1024:
            return diagnostic, details
        payload = json.loads(body)
        if not isinstance(payload, dict) or not isinstance(payload.get("error"), str):
            return diagnostic, details
        receipt_fields = {
            "error", "code", "reason", "retry_after_seconds", "resource_admission", "lifecycle",
            "placement", "execution", "admission", "route", "decision", "quality", "feedback",
        }
        payload_fields = {
            "prompt", "system", "system_prompt", "messages", "content", "input", "images", "tools",
            "request", "request_options", "headers", "authorization", "token", "api_key", "auth_token",
        }

        def receipt_value(value: Any) -> Any:
            if isinstance(value, dict):
                return {key: receipt_value(item) for key, item in value.items() if key.lower() not in payload_fields}
            if isinstance(value, list):
                return [receipt_value(item) for item in value]
            return value

        receipt = {key: receipt_value(value) for key, value in payload.items() if key in receipt_fields}
        details["receipt"] = receipt
        diagnostic = f"HTTP {error.code}: {receipt['error']}"
    except (OSError, ValueError, RecursionError):
        pass
    finally:
        error.close()
    return diagnostic, details


class PromptCacheUsage:
    """Aggregate optional Ollama cache reads without treating unavailable turns as zero."""

    def __init__(self) -> None:
        self.calls = 0
        self.reported_calls = 0
        self.reported_tokens = 0
        self.reported_prompt_tokens = 0

    def observe(self, response: dict[str, Any]) -> None:
        self.calls += 1
        cached = response.get("prompt_eval_cached_count")
        prompt = response.get("prompt_eval_count")
        if type(cached) is int and type(prompt) is int and 0 <= cached <= prompt:
            self.reported_calls += 1
            self.reported_tokens += cached
            self.reported_prompt_tokens += prompt

    @property
    def tokens(self) -> int | None:
        return self.reported_tokens if self.calls > 0 and self.calls == self.reported_calls else None

    def metadata(self) -> dict[str, Any]:
        return {
            "status": "reported" if self.tokens is not None else (
                "partially_reported" if self.reported_calls else "not_reported"
            ),
            "calls": self.calls,
            "reported_calls": self.reported_calls,
            "reported_tokens": self.reported_tokens if self.reported_calls else None,
            "hit_ratio": self.reported_tokens / self.reported_prompt_tokens
            if self.tokens is not None and self.reported_prompt_tokens else None,
        }


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
        "truncate": False,
        "shift": False,
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
