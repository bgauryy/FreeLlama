#!/usr/bin/env python3
"""Context-window management shared by the research/benchmark adapters.

Why this exists, concretely. Both adapters run at `num_ctx=8192` and append a full observation
(up to 3000 characters) after every turn. At the benchmark's `max_turns=10` the conversation
reaches roughly 9-10k tokens — past the window. Ollama does not error on that: it silently
truncates from the **front** of the prompt, and the front is the system prompt. The agent loses
its own tool schema and output contract mid-run, then returns prose instead of JSON, and the run
is recorded as `model did not return a JSON action` — a parsing failure that reads like a model
weakness but is really a context-management bug in the harness.

Everything here is deterministic and dependency-free, because the adapters are copied verbatim
into the published MCP package (`packages/mcp/scripts/bundle-adapters.mjs`) and must keep running
with nothing but the standard library.
"""

from __future__ import annotations

import json
from typing import Any

# Ollama reports no token counts until after a call, so budgeting has to be done on an estimate.
# Four characters per token is the usual English/code approximation and errs slightly high on
# code, which is the safe direction here: over-estimating trims early, under-estimating overflows
# silently, and only one of those two failures is recoverable.
CHARS_PER_TOKEN = 4
# Headroom held back from `num_ctx` on top of `num_predict`: chat templating, tool-call scaffolding
# and the token estimate's own error all land inside the same window.
SAFETY_MARGIN_TOKENS = 256
# Observations kept verbatim. The two most recent are what the model is actually reasoning over;
# older ones have usually already been distilled into the answer it is assembling.
DEFAULT_KEEP_RECENT = 2
# How much of a compacted observation survives as a breadcrumb, so the model still knows the call
# happened and roughly what it returned rather than silently losing the step.
COMPACT_PREVIEW_CHARS = 180


def estimate_tokens(text: str) -> int:
    """Approximate token count for budgeting. Deliberately cheap, deliberately slightly high."""
    return len(text) // CHARS_PER_TOKEN + 1


def messages_tokens(messages: list[dict[str, str]]) -> int:
    return sum(estimate_tokens(message.get("content", "")) for message in messages)


def clip(text: str, limit: int) -> str:
    """Truncate to `limit` characters keeping both ends.

    The adapters previously did `text[-limit:]`, keeping only the tail. That is the wrong half for
    almost every tool used here: a directory listing, a grep result and a file read all put their
    most identifying content first, so tail-only truncation threw away the part the model needed
    and kept the trailing noise. Keeping a majority head plus a minority tail preserves the
    beginning of the result while still showing where it ended.
    """
    if limit <= 0 or len(text) <= limit:
        return text
    marker_template = "\n… [{dropped} characters elided] …\n"
    # Reserve room for the marker itself so the result never exceeds `limit`.
    marker_budget = len(marker_template.format(dropped=len(text)))
    usable = max(limit - marker_budget, 0)
    if usable == 0:
        return text[:limit]
    head_chars = (usable * 2) // 3
    tail_chars = usable - head_chars
    dropped = len(text) - head_chars - tail_chars
    tail = text[-tail_chars:] if tail_chars else ""
    return text[:head_chars] + marker_template.format(dropped=dropped) + tail


def context_budget(num_ctx: int, num_predict: int) -> int:
    """Tokens available for the conversation itself, once generation and slack are reserved."""
    return max(num_ctx - num_predict - SAFETY_MARGIN_TOKENS, 0)


def _compact(content: str) -> str:
    preview = " ".join(content[:COMPACT_PREVIEW_CHARS].split())
    return f"[earlier observation compacted to save context] {preview}…"


def fit_to_context(
    messages: list[dict[str, str]],
    *,
    num_ctx: int,
    num_predict: int,
    keep_recent: int = DEFAULT_KEEP_RECENT,
) -> tuple[list[dict[str, str]], bool]:
    """Trim a conversation to fit `num_ctx`, protecting the parts that must never be dropped.

    Returns `(messages, compacted)`. The system prompt (index 0) and the original question
    (index 1) are pinned: they are the two messages Ollama's own front-truncation would delete
    first, and losing either is what turns a working agent into one that emits prose instead of
    JSON. Compaction walks oldest-first through the observation turns, so the model keeps a
    breadcrumb of every step it took even when the full text of the early ones is gone.
    """
    budget = context_budget(num_ctx, num_predict)
    if messages_tokens(messages) <= budget:
        return messages, False

    pinned = min(2, len(messages))
    trimmed = [dict(message) for message in messages]
    # Indices of observation turns (user messages after the original question), oldest first.
    # These carry ~90% of the weight and are the only content safe to shrink.
    observations = [
        index
        for index in range(pinned, len(trimmed))
        if trimmed[index].get("role") == "user"
    ]
    protected = set(observations[-keep_recent:]) if keep_recent else set()
    compacted = False
    for index in observations:
        if messages_tokens(trimmed) <= budget:
            break
        if index in protected:
            continue
        content = trimmed[index].get("content", "")
        replacement = _compact(content)
        if len(replacement) < len(content):
            trimmed[index]["content"] = replacement
            compacted = True

    # Still over after compacting every droppable observation: the recent turns alone exceed the
    # window. Clip the protected ones too rather than hand Ollama a prompt it will truncate blind.
    #
    # This shrinks the largest observation repeatedly instead of computing one subtraction, because
    # the token estimate rounds up per message: a single-pass calculation lands just over budget
    # and "just over" is exactly as silently truncated as "far over". Loop until it actually fits.
    guard = 0
    while messages_tokens(trimmed) > budget and guard < 200:
        guard += 1
        widest = max(
            observations,
            key=lambda index: len(trimmed[index].get("content", "")),
            default=None,
        )
        if widest is None:
            break
        content = trimmed[widest].get("content", "")
        keep = max(int(len(content) * 0.8), COMPACT_PREVIEW_CHARS)
        if keep >= len(content):
            break
        trimmed[widest]["content"] = clip(content, keep)
        compacted = True
    return trimmed, compacted


def call_signature(payload: Any) -> str:
    """Stable identity for a tool call, so an exact repeat can be recognized.

    Both adapters tell the model "do not repeat prior calls" and neither enforced it. A repeat is
    not just a wasted turn out of ten — it also re-appends an observation the model has already
    seen, spending context twice on one fact.
    """
    return json.dumps(payload, sort_keys=True, ensure_ascii=False, default=str)


REPEAT_NOTICE = (
    "(identical call already made this run — its result is above and was not re-executed. "
    "Use a different call, or finish now with the evidence already collected.)"
)
