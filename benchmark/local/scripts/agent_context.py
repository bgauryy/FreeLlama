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
import hashlib
import math
import os
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Callable, Mapping

# Ollama reports no token counts until after a call, so the first budget uses a configurable
# approximation. Real prompt_eval_count values then raise the multiplier whenever this estimate
# was optimistic; it never lowers itself from a sample.
CHARS_PER_TOKEN = 4
# Headroom held back from `num_ctx` on top of `num_predict`: chat templating, tool-call scaffolding
# and the token estimate's own error all land inside the same window.
SAFETY_MARGIN_TOKENS = 256
# Base64 bytes are not text tokens. Multimodal templates turn each image into a bounded token
# sequence; charging a conservative fixed amount avoids treating a 5MB JPEG as 1.25M text tokens.
IMAGE_TOKEN_ESTIMATE = 1024
# Observations kept verbatim. The two most recent are what the model is actually reasoning over;
# older ones have usually already been distilled into the answer it is assembling.
DEFAULT_KEEP_RECENT = 2
# How much of a compacted observation survives as a breadcrumb, so the model still knows the call
# happened and roughly what it returned rather than silently losing the step.
COMPACT_PREVIEW_CHARS = 180
# Emergency clipping keeps this fraction of the current payload on each pass. It is deliberately
# iterative: rounding and per-message metadata make a one-shot subtraction unsafe.
COMPACT_RETAIN_RATIO = 0.8
CLIP_HEAD_RATIO = 2 / 3
# Full observations are kept outside the prompt and served in line-aware pages of this size.
OBSERVATION_PAGE_CHARS = 3000


def _env_int(env: Mapping[str, str], name: str, default: int, *, minimum: int = 0) -> int:
    raw = env.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = int(raw)
    except ValueError as error:
        raise ValueError(f"{name} must be an integer, got {raw!r}") from error
    if value < minimum:
        raise ValueError(f"{name} must be >= {minimum}, got {value}")
    return value


def _env_float(env: Mapping[str, str], name: str, default: float, *, minimum: float = 0.0) -> float:
    raw = env.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = float(raw)
    except ValueError as error:
        raise ValueError(f"{name} must be a number, got {raw!r}") from error
    if not math.isfinite(value) or value < minimum:
        raise ValueError(f"{name} must be finite and >= {minimum}, got {raw!r}")
    return value


def _env_bool(env: Mapping[str, str], name: str, default: bool) -> bool:
    raw = env.get(name)
    if raw is None or raw == "":
        return default
    normalized = raw.strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise ValueError(f"{name} must be true/false, got {raw!r}")


@dataclass(frozen=True)
class ContextPolicy:
    """Validated context-management policy shared by every research adapter."""

    chars_per_token: float = CHARS_PER_TOKEN
    safety_margin_tokens: int = SAFETY_MARGIN_TOKENS
    image_token_estimate: int = IMAGE_TOKEN_ESTIMATE
    keep_recent: int = DEFAULT_KEEP_RECENT
    compact_preview_chars: int = COMPACT_PREVIEW_CHARS
    compact_retain_ratio: float = COMPACT_RETAIN_RATIO
    clip_head_ratio: float = CLIP_HEAD_RATIO
    observation_page_chars: int = OBSERVATION_PAGE_CHARS
    pinned_overflow: str = "error"

    def __post_init__(self) -> None:
        if not math.isfinite(self.chars_per_token) or self.chars_per_token <= 0:
            raise ValueError("chars_per_token must be finite and > 0")
        if self.safety_margin_tokens < 0 or self.image_token_estimate < 0 or self.keep_recent < 0:
            raise ValueError("context token estimates and keep_recent must be >= 0")
        if self.compact_preview_chars <= 0 or self.observation_page_chars <= 0:
            raise ValueError("preview and page sizes must be > 0")
        if not 0 < self.compact_retain_ratio < 1:
            raise ValueError("compact_retain_ratio must be between 0 and 1")
        if not 0 < self.clip_head_ratio < 1:
            raise ValueError("clip_head_ratio must be between 0 and 1")
        if self.pinned_overflow not in {"error", "clip"}:
            raise ValueError("pinned_overflow must be 'error' or 'clip'")

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> "ContextPolicy":
        values = os.environ if env is None else env
        pinned_overflow = values.get("FREELLAMA_AGENT_PINNED_OVERFLOW", "error").strip().lower()
        return cls(
            chars_per_token=_env_float(values, "FREELLAMA_AGENT_CHARS_PER_TOKEN", CHARS_PER_TOKEN, minimum=0.000001),
            safety_margin_tokens=_env_int(values, "FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS", SAFETY_MARGIN_TOKENS),
            image_token_estimate=_env_int(values, "FREELLAMA_AGENT_IMAGE_TOKEN_ESTIMATE", IMAGE_TOKEN_ESTIMATE),
            keep_recent=_env_int(values, "FREELLAMA_AGENT_KEEP_RECENT", DEFAULT_KEEP_RECENT),
            compact_preview_chars=_env_int(values, "FREELLAMA_AGENT_COMPACT_PREVIEW_CHARS", COMPACT_PREVIEW_CHARS, minimum=1),
            compact_retain_ratio=_env_float(values, "FREELLAMA_AGENT_COMPACT_RETAIN_RATIO", COMPACT_RETAIN_RATIO, minimum=0.000001),
            clip_head_ratio=_env_float(values, "FREELLAMA_AGENT_CLIP_HEAD_RATIO", CLIP_HEAD_RATIO, minimum=0.000001),
            observation_page_chars=_env_int(values, "FREELLAMA_AGENT_OBSERVATION_PAGE_CHARS", OBSERVATION_PAGE_CHARS, minimum=1),
            pinned_overflow=pinned_overflow,
        )

    def metadata(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class AgentRuntimeConfig:
    """Typed operational settings; safety confinement remains an invariant, not a knob."""

    max_turns: int
    num_ctx: int
    num_predict: int
    temperature: float
    seed: int
    request_timeout_seconds: float
    retry_attempts: int
    retry_backoff_seconds: float
    tool_timeout_seconds: float
    keep_alive: str
    think: bool
    max_parse_repairs: int
    parse_repair_echo_chars: int
    context: ContextPolicy

    @classmethod
    def from_env(
        cls,
        *,
        default_tool_timeout_seconds: float,
        env: Mapping[str, str] | None = None,
    ) -> "AgentRuntimeConfig":
        values = os.environ if env is None else env
        context = ContextPolicy.from_env(values)
        config = cls(
            max_turns=_env_int(values, "FREELLAMA_AGENT_MAX_TURNS", 10, minimum=1),
            num_ctx=_env_int(values, "FREELLAMA_AGENT_NUM_CTX", 8192, minimum=1),
            num_predict=_env_int(values, "FREELLAMA_AGENT_NUM_PREDICT", 512, minimum=1),
            temperature=_env_float(values, "FREELLAMA_AGENT_TEMPERATURE", 0.0),
            seed=_env_int(values, "FREELLAMA_AGENT_SEED", 42),
            request_timeout_seconds=_env_float(values, "FREELLAMA_AGENT_REQUEST_TIMEOUT_SECONDS", 600.0, minimum=0.001),
            retry_attempts=_env_int(values, "FREELLAMA_AGENT_RETRY_ATTEMPTS", 2, minimum=1),
            retry_backoff_seconds=_env_float(values, "FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS", 5.0),
            tool_timeout_seconds=_env_float(values, "FREELLAMA_AGENT_TOOL_TIMEOUT_SECONDS", default_tool_timeout_seconds, minimum=0.001),
            keep_alive=values.get("FREELLAMA_AGENT_KEEP_ALIVE", "5m").strip(),
            think=_env_bool(values, "FREELLAMA_AGENT_THINK", False),
            max_parse_repairs=_env_int(values, "FREELLAMA_AGENT_MAX_PARSE_REPAIRS", MAX_PARSE_REPAIRS, minimum=0),
            parse_repair_echo_chars=_env_int(values, "FREELLAMA_AGENT_PARSE_REPAIR_ECHO_CHARS", 500, minimum=1),
            context=context,
        )
        if context_budget(config.num_ctx, config.num_predict, context.safety_margin_tokens) <= 0:
            raise ValueError(
                "FREELLAMA_AGENT_NUM_CTX must exceed FREELLAMA_AGENT_NUM_PREDICT plus "
                "FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS"
            )
        if not config.keep_alive:
            raise ValueError("FREELLAMA_AGENT_KEEP_ALIVE must not be empty")
        return config

    def metadata(self) -> dict[str, Any]:
        result = asdict(self)
        result.pop("context", None)
        return result


def estimate_tokens(text: str, chars_per_token: float = CHARS_PER_TOKEN) -> int:
    """Approximate a first-call token count; later model counts calibrate the multiplier."""
    return math.floor(len(text) / chars_per_token) + 1


def messages_tokens(messages: list[dict[str, Any]], policy: ContextPolicy | None = None) -> int:
    """Estimate typed Ollama messages without flattening images/tool calls into text."""
    policy = policy or ContextPolicy()
    total = 0
    for message in messages:
        content = message.get("content", "")
        total += estimate_tokens(
            content if isinstance(content, str) else json.dumps(content, ensure_ascii=False),
            policy.chars_per_token,
        )
        images = message.get("images", [])
        if isinstance(images, list):
            total += len(images) * policy.image_token_estimate
        metadata = {key: value for key, value in message.items() if key not in {"content", "images"}}
        total += estimate_tokens(
            json.dumps(metadata, ensure_ascii=False, default=str), policy.chars_per_token
        )
    return total


class ContextBudgeter:
    """Conservative estimate calibrated by Ollama's real post-call prompt count.

    Ollama currently has no stable preflight tokenizer endpoint. The first call therefore uses the
    configured character estimate. After every successful call, `prompt_eval_count` raises (never
    lowers) the estimate multiplier for this exact model/chat template.
    """

    def __init__(
        self,
        policy: ContextPolicy | None = None,
        *,
        initial_scale: float = 1.0,
        calibration_samples: int = 0,
    ) -> None:
        self.policy = policy or ContextPolicy()
        self.scale = max(float(initial_scale), 1.0)
        self.calibration_samples = max(int(calibration_samples), 0)

    def estimate(self, messages: list[dict[str, Any]]) -> int:
        return math.ceil(messages_tokens(messages, self.policy) * self.scale)

    def observe(self, messages: list[dict[str, Any]], prompt_eval_count: Any) -> bool:
        try:
            actual = int(prompt_eval_count)
        except (TypeError, ValueError):
            return False
        if actual <= 0:
            return False
        baseline = max(messages_tokens(messages, self.policy), 1)
        previous = self.scale
        self.scale = max(self.scale, actual / baseline)
        self.calibration_samples += 1
        return self.scale > previous

    def metadata(self) -> dict[str, Any]:
        return {
            "token_counting": "model_calibrated_estimate" if self.calibration_samples else "configured_estimate",
            "estimate_scale": round(self.scale, 6),
            "calibration_samples": self.calibration_samples,
        }


def clip(text: str, limit: int, head_ratio: float = CLIP_HEAD_RATIO) -> str:
    """Truncate to `limit` characters keeping both ends.

    The adapters previously did `text[-limit:]`, keeping only the tail. That is the wrong half for
    almost every tool used here: a directory listing, a grep result and a file read all put their
    most identifying content first, so tail-only truncation threw away the part the model needed
    and kept the trailing noise. Keeping a majority head plus a minority tail preserves the
    beginning of the result while still showing where it ended.
    """
    if not 0 < head_ratio < 1:
        raise ValueError("head_ratio must be between 0 and 1")
    if limit <= 0 or len(text) <= limit:
        return text
    marker_template = "\n… [{dropped} characters elided] …\n"
    # Reserve room for the marker itself so the result never exceeds `limit`.
    marker_budget = len(marker_template.format(dropped=len(text)))
    usable = max(limit - marker_budget, 0)
    if usable == 0:
        return text[:limit]
    head_chars = int(usable * head_ratio)
    tail_chars = usable - head_chars
    dropped = len(text) - head_chars - tail_chars
    tail = text[-tail_chars:] if tail_chars else ""
    return text[:head_chars] + marker_template.format(dropped=dropped) + tail


def context_budget(
    num_ctx: int,
    num_predict: int,
    safety_margin_tokens: int = SAFETY_MARGIN_TOKENS,
) -> int:
    """Tokens available for the conversation itself, once generation and slack are reserved."""
    return max(num_ctx - num_predict - safety_margin_tokens, 0)


def _compact(content: str, policy: ContextPolicy) -> str:
    preview = " ".join(content[: policy.compact_preview_chars].split())
    return f"[earlier observation compacted to save context] {preview}…"


def fit_to_context(
    messages: list[dict[str, Any]],
    *,
    num_ctx: int,
    num_predict: int,
    keep_recent: int | None = None,
    policy: ContextPolicy | None = None,
    token_counter: Callable[[list[dict[str, Any]]], int] | None = None,
) -> tuple[list[dict[str, Any]], bool]:
    """Trim a conversation to fit `num_ctx`, protecting the parts that must never be dropped.

    Returns `(messages, compacted)`. The system prompt (index 0) and the original question
    (index 1) are pinned: they are the two messages Ollama's own front-truncation would delete
    first, and losing either is what turns a working agent into one that emits prose instead of
    JSON. Compaction walks oldest-first through the observation turns, so the model keeps a
    breadcrumb of every step it took even when the full text of the early ones is gone.
    """
    policy = policy or ContextPolicy()
    keep_recent = policy.keep_recent if keep_recent is None else keep_recent
    budget = context_budget(num_ctx, num_predict, policy.safety_margin_tokens)
    count = token_counter or (lambda value: messages_tokens(value, policy))
    if count(messages) <= budget:
        return messages, False

    pinned = min(2, len(messages))
    trimmed = [dict(message) for message in messages]
    # Tool-result messages are first-class observations in native Ollama agent loops. Preserve their
    # role and tool_name/tool_call linkage; compact only their textual payload.
    observations = [
        index
        for index in range(pinned, len(trimmed))
        if trimmed[index].get("role") in {"user", "tool"}
        and isinstance(trimmed[index].get("content"), str)
    ]
    protected = set(observations[-keep_recent:]) if keep_recent else set()
    compacted = False
    for index in observations:
        if count(trimmed) <= budget:
            break
        if index in protected:
            continue
        content = trimmed[index].get("content", "")
        replacement = _compact(content, policy)
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
    while count(trimmed) > budget and guard < 200:
        guard += 1
        widest = max(
            observations,
            key=lambda index: len(trimmed[index].get("content", "")),
            default=None,
        )
        if widest is None:
            break
        content = trimmed[widest].get("content", "")
        keep = max(int(len(content) * policy.compact_retain_ratio), policy.compact_preview_chars)
        if keep >= len(content):
            break
        trimmed[widest]["content"] = clip(content, keep, policy.clip_head_ratio)
        compacted = True
    # Old reasoning traces can dwarf the answer and are not required to preserve a tool-call pair.
    # Keep the field typed, but reduce its payload after observations have already been compacted.
    for message in trimmed[pinned:-2]:
        thinking = message.get("thinking")
        if count(trimmed) <= budget:
            break
        if isinstance(thinking, str) and len(thinking) > policy.compact_preview_chars:
            message["thinking"] = _compact(thinking, policy)
            compacted = True

    # Compact any remaining non-pinned textual history before considering the pinned contract.
    guard = 0
    while count(trimmed) > budget and guard < 200:
        guard += 1
        candidates = [
            index
            for index, message in enumerate(trimmed[pinned:], start=pinned)
            if isinstance(message.get("content"), str)
            and len(message.get("content", "")) > policy.compact_preview_chars
        ]
        if not candidates:
            break
        widest = max(candidates, key=lambda index: len(trimmed[index].get("content", "")))
        content = trimmed[widest]["content"]
        keep = max(int(len(content) * policy.compact_retain_ratio), policy.compact_preview_chars)
        if keep >= len(content):
            break
        trimmed[widest]["content"] = clip(content, keep, policy.clip_head_ratio)
        compacted = True

    # Pinned text is byte-preserved by default. An operator may explicitly choose `clip`, but the
    # safe default fails before Ollama can silently front-truncate the system contract or question.
    guard = 0
    while policy.pinned_overflow == "clip" and count(trimmed) > budget and guard < 200:
        guard += 1
        candidates = [
            index
            for index, message in enumerate(trimmed[:pinned])
            if isinstance(message.get("content"), str)
            and len(message.get("content", "")) > policy.compact_preview_chars
        ]
        if not candidates:
            break
        widest = max(candidates, key=lambda index: len(trimmed[index].get("content", "")))
        content = trimmed[widest]["content"]
        keep = max(int(len(content) * policy.compact_retain_ratio), policy.compact_preview_chars)
        if keep >= len(content):
            break
        trimmed[widest]["content"] = clip(content, keep, policy.clip_head_ratio)
        compacted = True
    if count(trimmed) > budget:
        raise ValueError(
            "pinned system/question content and minimum history exceed context budget: "
            f"{count(trimmed)} > {budget} estimated tokens. Increase FREELLAMA_AGENT_NUM_CTX, "
            "reduce the task/instructions, or explicitly set FREELLAMA_AGENT_PINNED_OVERFLOW=clip."
        )
    return trimmed, compacted


class AgentContextManager:
    """Stateful adapter facade: fit, calibrate, count compactions, and report policy."""

    def __init__(
        self,
        runtime: AgentRuntimeConfig,
        *,
        model: str = "",
        calibration_dir: Path | None = None,
    ) -> None:
        self.runtime = runtime
        self.model = model
        self.calibration_dir = calibration_dir
        loaded = self._load_calibration()
        self.budgeter = ContextBudgeter(
            runtime.context,
            initial_scale=loaded.get("scale", 1.0),
            calibration_samples=loaded.get("samples", 0),
        )
        self.calibration_source = "persistent_model_cache" if loaded else "current_process"
        self.compactions = 0

    def _model_key(self) -> str:
        return hashlib.sha256(self.model.encode("utf-8")).hexdigest()[:24]

    def _load_calibration(self) -> dict[str, Any]:
        if not self.model or self.calibration_dir is None or not self.calibration_dir.exists():
            return {}
        best_scale = 1.0
        samples = 0
        for path in self.calibration_dir.glob(f"{self._model_key()}-*.json"):
            try:
                record = json.loads(path.read_text(encoding="utf-8"))
                if record.get("schema_version") != 1 or record.get("model") != self.model:
                    continue
                best_scale = max(best_scale, float(record.get("scale", 1.0)))
                samples = max(samples, int(record.get("samples", 0)))
            except (OSError, ValueError, TypeError, json.JSONDecodeError):
                continue
        return {"scale": best_scale, "samples": samples} if samples else {}

    def _persist_calibration(self) -> None:
        if not self.model or self.calibration_dir is None:
            return
        self.calibration_dir.mkdir(parents=True, exist_ok=True)
        key = self._model_key()
        record = {
            "schema_version": 1,
            "model": self.model,
            "scale": self.budgeter.scale,
            "samples": self.budgeter.calibration_samples,
        }
        final = self.calibration_dir / f"{key}-{time.time_ns()}-{os.getpid()}.json"
        temporary = final.with_suffix(".tmp")
        temporary.write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(temporary, final)
        # Bounded, race-tolerant history: independent records make concurrent writers monotonic;
        # retaining the newest 32 avoids an unbounded cache without a cross-platform file lock.
        records = sorted(self.calibration_dir.glob(f"{key}-*.json"), key=lambda path: path.name)
        for stale in records[:-32]:
            try:
                stale.unlink()
            except FileNotFoundError:
                pass

    def fit(self, messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
        fitted, compacted = fit_to_context(
            messages,
            num_ctx=self.runtime.num_ctx,
            num_predict=self.runtime.num_predict,
            policy=self.runtime.context,
            token_counter=self.budgeter.estimate,
        )
        self.compactions += int(compacted)
        return fitted

    def observe(self, messages: list[dict[str, Any]], prompt_eval_count: Any) -> None:
        if self.budgeter.observe(messages, prompt_eval_count):
            self._persist_calibration()

    def metadata(self) -> dict[str, Any]:
        return {
            **self.runtime.context.metadata(),
            **self.budgeter.metadata(),
            "calibration_source": self.calibration_source,
            "estimated_input_budget": context_budget(
                self.runtime.num_ctx,
                self.runtime.num_predict,
                self.runtime.context.safety_margin_tokens,
            ),
            "compactions": self.compactions,
        }


def write_failure_result(result_path: Path, answer: str) -> None:
    """Write the minimum valid adapter result for a failure before the normal result path."""
    result_path.parent.mkdir(parents=True, exist_ok=True)
    result_path.write_text(
        json.dumps({"final_answer": answer, "tool_calls": [], "usage": {}}, indent=2) + "\n",
        encoding="utf-8",
    )


def parse_json_action(content: str) -> dict[str, Any]:
    """Recover one JSON object while leaving adapter-specific action validation to the caller."""
    text = content.strip()
    if text.startswith("```"):
        lines = text.splitlines()
        text = "\n".join(lines[1:-1]).strip()
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        decoder = json.JSONDecoder()
        start = text.find("{")
        if start < 0:
            raise ValueError("model did not return a JSON action") from None
        try:
            value, _ = decoder.raw_decode(text, start)
        except json.JSONDecodeError:
            raise ValueError("model did not return a valid JSON action") from None
    if not isinstance(value, dict) or not isinstance(value.get("action"), str):
        raise ValueError("model action must be a JSON object with an action")
    return value


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


# How many unparseable turns to try to repair before giving up on the run. A model that emits prose
# instead of JSON once will usually correct when told so explicitly; aborting on the first one threw
# away an entire delegation — including every tool result already gathered — and surfaced as
# "model did not return a valid JSON action", which reads like model weakness rather than a loop
# with no error recovery.
MAX_PARSE_REPAIRS = 2

PARSE_REPAIR_NOTICE = (
    "Your last reply was not a valid JSON action, so it was discarded. Reply with EXACTLY one JSON "
    "object and nothing else — no prose before or after, no markdown fence. Use the action shapes "
    "given in your instructions. If you already have enough evidence, send the finish action now."
)

# ---------------------------------------------------------------------------------------------
# Pagination — the alternative to throwing data away
#
# Clipping an observation is silent data loss the model can never recover: a routine
# `grep -rn "def " .` over jinja produces ~27,000 characters, and a 3,000-character clip discarded
# 89% of it. The model cannot ask for the rest, cannot tell how much it is missing, and will
# happily conclude "not found" from a window that never contained the answer.
#
# Pagination keeps every byte. The full observation is retained, the model is shown one page plus
# an exact instruction for requesting the next, and asking for page 2 re-serves stored text rather
# than re-running the command — so paging costs a turn, never a duplicate subprocess.
# ---------------------------------------------------------------------------------------------

def paginate(text: str, page: int = 1, page_size: int = OBSERVATION_PAGE_CHARS) -> tuple[str, int, int]:
    """Return `(chunk, page, total_pages)`, splitting on line boundaries.

    Line-aware on purpose: these observations are grep hits, directory listings and file slices,
    where a chunk cut mid-line yields a truncated path or a half identifier — exactly the kind of
    fragment that gets misread as data rather than as damage.
    """
    if not text:
        return "", 1, 1
    lines = text.splitlines(keepends=True)
    pages: list[list[str]] = [[]]
    size = 0
    for line in lines:
        # A single line longer than a page gets its own page rather than being split.
        if size and size + len(line) > page_size:
            pages.append([])
            size = 0
        pages[-1].append(line)
        size += len(line)
    total = max(1, len(pages))
    page = max(1, min(page, total))
    return "".join(pages[page - 1]), page, total


def page_footer(step: int, page: int, total: int, total_chars: int) -> str:
    """Tell the model exactly how to get the rest — or that there is no rest."""
    if total <= 1:
        return ""
    remaining = total - page
    nxt = page + 1 if remaining else 1
    return (
        f'\n\n[page {page} of {total} — {total_chars} characters total, nothing discarded. '
        f'For the next page send exactly: {{"action":"page","step":{step},"page":{nxt}}} '
        f"— that re-reads stored output and does NOT re-run the command.]"
    )


class ObservationStore:
    """Keeps every observation in full so any page can be served later.

    This is what makes pagination lossless rather than a nicer-looking clip: the text stays here
    whole, and the conversation only ever holds the page the model asked for.
    """

    def __init__(self, page_size: int = OBSERVATION_PAGE_CHARS) -> None:
        if page_size <= 0:
            raise ValueError("observation page size must be > 0")
        self._by_step: dict[int, str] = {}
        self.page_size = page_size

    def put(self, step: int, text: str) -> None:
        self._by_step[step] = text

    def get(self, step: int) -> str | None:
        return self._by_step.get(step)

    def view(self, step: int, page: int = 1) -> tuple[str, str]:
        """Return `(body, footer)` for one page of a stored observation."""
        text = self._by_step.get(step)
        if text is None:
            known = ", ".join(str(k) for k in sorted(self._by_step)) or "none yet"
            return (f"no stored output for step {step} (known steps: {known})", "")
        chunk, page, total = paginate(text, page, self.page_size)
        return chunk, page_footer(step, page, total, len(text))
