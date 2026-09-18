#!/usr/bin/env python3
"""Contract tests for the adapters' context management. Run: python3 test_agent_context.py"""

from __future__ import annotations

import sys
import json
import tracemalloc
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from agent_context import (  # noqa: E402
    AgentContextManager,
    AgentRuntimeConfig,
    CHARS_PER_TOKEN,
    ContextBudgeter,
    ContextPolicy,
    IMAGE_TOKEN_ESTIMATE,
    ObservationStore,
    paginate,
    page_footer,
    call_signature,
    clip,
    context_budget,
    fit_to_context,
    messages_tokens,
    _compact,
)

FAILURES: list[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    if condition:
        print(f"ok   {name}")
    else:
        FAILURES.append(f"{name}{f': {detail}' if detail else ''}")
        print(f"FAIL {name} {detail}")


def build_conversation(turns: int, observation_chars: int) -> list[dict[str, str]]:
    messages = [
        {"role": "system", "content": "SYSTEM_PROMPT_SENTINEL " + "x" * 4000},
        {"role": "user", "content": "QUESTION_SENTINEL where is the router defined?"},
    ]
    for turn in range(turns):
        messages.append({"role": "assistant", "content": f'{{"action":"octocode","tool":"t{turn}"}}'})
        messages.append({"role": "user", "content": f"Observation {turn}:\n" + "y" * observation_chars})
    return messages


# --- clip keeps the head, which tail-only slicing threw away -------------------------------
head_marker = "IMPORTANT_FIRST_LINE"
body = head_marker + "\n" + ("filler\n" * 2000) + "LAST_LINE"
clipped = clip(body, 3000)
check("clip keeps the head of a long tool result", head_marker in clipped)
check("clip keeps the tail too", "LAST_LINE" in clipped)
check("clip respects the character limit", len(clipped) <= 3000, f"got {len(clipped)}")
check("clip leaves short text untouched", clip("short", 3000) == "short")
tail_heavy = clip("H" * 1000 + "T" * 1000, 200, head_ratio=0.25)
check("head/tail clipping ratio is configurable", tail_heavy.count("T") > tail_heavy.count("H"))
try:
    clip("short", 3000, head_ratio=0)
except ValueError:
    invalid_clip_ratio_rejected = True
else:
    invalid_clip_ratio_rejected = False
check("invalid clipping ratios fail even when no clipping is needed", invalid_clip_ratio_rejected)

# --- the real bug: a 10-turn run at num_ctx=8192 overflowed silently -------------------------
messages = build_conversation(turns=10, observation_chars=3000)
budget = context_budget(8192, 512)
check(
    "a 10-turn run genuinely overflows num_ctx=8192 before the fix",
    messages_tokens(messages) > budget,
    f"{messages_tokens(messages)} tokens vs {budget} budget",
)

fitted, compacted = fit_to_context(messages, num_ctx=8192, num_predict=512)
check("fit_to_context reports that it compacted", compacted)
check(
    "fit_to_context brings the conversation inside the budget",
    messages_tokens(fitted) <= budget,
    f"{messages_tokens(fitted)} tokens vs {budget} budget",
)
check(
    "the system prompt survives compaction",
    "SYSTEM_PROMPT_SENTINEL" in fitted[0]["content"],
)
check("the original question survives compaction", "QUESTION_SENTINEL" in fitted[1]["content"])
check("no messages are dropped, only shrunk", len(fitted) == len(messages))
check(
    "the two most recent observations stay verbatim",
    fitted[-1]["content"] == messages[-1]["content"]
    and fitted[-3]["content"] == messages[-3]["content"],
)
check(
    "older observations leave a breadcrumb rather than vanishing",
    "compacted" in fitted[3]["content"],
)

# --- a conversation already inside the budget is returned untouched --------------------------
small = build_conversation(turns=1, observation_chars=100)
fitted_small, compacted_small = fit_to_context(small, num_ctx=8192, num_predict=512)
check("a small conversation is not compacted", not compacted_small and fitted_small == small)
check("fitting within budget preserves the original prefix object", fitted_small is small)

recoverable = "Observation (step 7):\nFIRST_EVIDENCE\n" + "middle " * 2000 + "\nLAST_EVIDENCE"
recoverable += page_footer(7, 2, 4, 20_000)
breadcrumb = _compact(recoverable, ContextPolicy())
check("compaction retains an exact stored-page recovery action", '{"action":"page","step":7,"page":2}' in breadcrumb)
check("compaction retains completeness state", "page 2 of 4" in breadcrumb)
check("compaction retains evidence at both ends", "FIRST_EVIDENCE" in breadcrumb and "LAST_EVIDENCE" in breadcrumb)
check("compacting a breadcrumb again is byte-stable", _compact(breadcrumb, ContextPolicy()) == breadcrumb)
single_page_observation = "Observation (step 8):\nFIRST_EVIDENCE\n" + "middle " * 500 + "\nLAST_EVIDENCE"
single_page_observation += "\n\nCommands remaining: 2. Finish now if the task is answerable; do not repeat prior commands."
check("breadcrumb tail is evidence rather than an obsolete turn reminder", "LAST_EVIDENCE" in _compact(single_page_observation, ContextPolicy()))
check("overflow compaction reclaims modest headroom when old observations suffice", messages_tokens(fitted) <= int(budget * 0.9))
extended = fitted + [{"role": "assistant", "content": '{"action":"finish","answer":"done"}'}]
extended_fit, extended_compacted = fit_to_context(extended, num_ctx=8192, num_predict=512)
check("next fitting append leaves the compacted prefix unchanged", not extended_compacted and extended_fit is extended)

# --- pathological case: recent turns alone exceed the window ---------------------------------
huge = build_conversation(turns=2, observation_chars=60_000)
fitted_huge, _ = fit_to_context(huge, num_ctx=8192, num_predict=512)
check(
    "even oversized recent observations are clipped to fit",
    messages_tokens(fitted_huge) <= context_budget(8192, 512),
    f"{messages_tokens(fitted_huge)} tokens",
)
check("the system prompt still survives the pathological case", "SYSTEM_PROMPT_SENTINEL" in fitted_huge[0]["content"])
check("pathological observations never alter the pinned system prompt", fitted_huge[0] == huge[0])
check("pathological observations never alter the pinned question", fitted_huge[1] == huge[1])

# --- pinned overflow is fail-closed by default and clipping is explicit ------------------------
pinned_only = [
    {"role": "system", "content": "SYSTEM_PINNED " + "s" * 40_000},
    {"role": "user", "content": "QUESTION_PINNED " + "q" * 40_000},
]
try:
    fit_to_context(pinned_only, num_ctx=4096, num_predict=512)
except ValueError as error:
    pinned_error = str(error)
else:
    pinned_error = ""
check("oversized pinned content fails closed by default", "FREELLAMA_AGENT_PINNED_OVERFLOW=clip" in pinned_error)

clip_policy = ContextPolicy(pinned_overflow="clip")
fitted_pinned, clipped_pinned = fit_to_context(
    pinned_only,
    num_ctx=4096,
    num_predict=512,
    policy=clip_policy,
)
check("pinned clipping requires and honors explicit opt-in", clipped_pinned)
check("opt-in pinned clipping fits", messages_tokens(fitted_pinned, clip_policy) <= context_budget(4096, 512))
check("opt-in pinned clipping preserves leading contract markers", "SYSTEM_PINNED" in fitted_pinned[0]["content"] and "QUESTION_PINNED" in fitted_pinned[1]["content"])

# --- repeat detection -------------------------------------------------------------------------
check(
    "identical calls share a signature regardless of key order",
    call_signature({"tool": "a", "queries": {"x": 1, "y": 2}})
    == call_signature({"queries": {"y": 2, "x": 1}, "tool": "a"}),
)
check(
    "different calls do not collide",
    call_signature({"command": "ls"}) != call_signature({"command": "ls -la"}),
)

# --- budgeting sanity --------------------------------------------------------------------------
check("token estimate scales with length", messages_tokens([{"role": "user", "content": "z" * (CHARS_PER_TOKEN * 100)}]) >= 100)
check("budget never goes negative", context_budget(256, 512) == 0)

custom_policy = ContextPolicy(chars_per_token=2, safety_margin_tokens=400, image_token_estimate=77)
check("characters-per-token estimate is configurable", messages_tokens(small, custom_policy) > messages_tokens(small))
check("context safety margin is configurable", context_budget(8192, 512, custom_policy.safety_margin_tokens) == 7280)
check(
    "image token charge is configurable",
    messages_tokens([{"role": "user", "content": "", "images": ["bytes"]}], custom_policy) >= 77,
)

budgeter = ContextBudgeter()
calibration_messages = [{"role": "user", "content": "calibrate " * 100}]
uncalibrated = budgeter.estimate(calibration_messages)
budgeter.observe(calibration_messages, uncalibrated * 2)
check("Ollama prompt counts calibrate future estimates", budgeter.estimate(calibration_messages) == uncalibrated * 2)
budgeter.observe(calibration_messages, 1)
check("calibration never makes budgeting less conservative", budgeter.estimate(calibration_messages) == uncalibrated * 2)

runtime_env = {
    "FREELLAMA_AGENT_NUM_CTX": "16384",
    "FREELLAMA_AGENT_NUM_PREDICT": "768",
    "FREELLAMA_AGENT_TEMPERATURE": "0.2",
    "FREELLAMA_AGENT_SEED": "7",
    "FREELLAMA_AGENT_REQUEST_TIMEOUT_SECONDS": "90",
    "FREELLAMA_AGENT_RETRY_ATTEMPTS": "3",
    "FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS": "1.5",
    "FREELLAMA_AGENT_TOOL_TIMEOUT_SECONDS": "12",
    "FREELLAMA_AGENT_KEEP_ALIVE": "0",
    "FREELLAMA_AGENT_THINK": "true",
    "FREELLAMA_AGENT_MAX_PARSE_REPAIRS": "4",
    "FREELLAMA_AGENT_PARSE_REPAIR_ECHO_CHARS": "240",
    "FREELLAMA_AGENT_CHARS_PER_TOKEN": "3.5",
    "FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS": "300",
    "FREELLAMA_AGENT_IMAGE_TOKEN_ESTIMATE": "900",
    "FREELLAMA_AGENT_KEEP_RECENT": "3",
    "FREELLAMA_AGENT_COMPACT_PREVIEW_CHARS": "120",
    "FREELLAMA_AGENT_COMPACT_RETAIN_RATIO": "0.7",
    "FREELLAMA_AGENT_CLIP_HEAD_RATIO": "0.6",
    "FREELLAMA_AGENT_OBSERVATION_PAGE_CHARS": "2048",
    "FREELLAMA_AGENT_PINNED_OVERFLOW": "clip",
}
runtime = AgentRuntimeConfig.from_env(default_tool_timeout_seconds=30, env=runtime_env)
check("runtime schema applies model-call configuration", runtime.num_ctx == 16384 and runtime.num_predict == 768 and runtime.think and runtime.parse_repair_echo_chars == 240)
check("runtime schema applies context configuration", runtime.context.observation_page_chars == 2048 and runtime.context.pinned_overflow == "clip" and runtime.context.clip_head_ratio == 0.6)
manager = AgentContextManager(runtime)
manager.observe(small, messages_tokens(small, runtime.context) * 2)
check("context manager reports resolved policy and calibration", manager.metadata()["calibration_samples"] == 1 and manager.metadata()["estimated_input_budget"] == 15316)

with tempfile.TemporaryDirectory() as calibration_root:
    calibration_dir = Path(calibration_root)
    first_manager = AgentContextManager(runtime, model="model-a", calibration_dir=calibration_dir)
    baseline_estimate = first_manager.budgeter.estimate(small)
    first_manager.observe(small, baseline_estimate * 2)
    restarted_manager = AgentContextManager(runtime, model="model-a", calibration_dir=calibration_dir)
    check(
        "model calibration survives a fresh coding-agent process",
        restarted_manager.budgeter.estimate(small) >= baseline_estimate * 2
        and restarted_manager.metadata()["calibration_source"] == "persistent_model_cache",
    )
    other_model = AgentContextManager(runtime, model="model-b", calibration_dir=calibration_dir)
    check(
        "persistent token calibration never crosses model templates",
        other_model.metadata()["calibration_source"] == "current_process"
        and other_model.budgeter.scale == 1.0,
    )
    changed_policy_runtime = AgentRuntimeConfig.from_env(
        default_tool_timeout_seconds=30,
        env={**runtime_env, "FREELLAMA_AGENT_CHARS_PER_TOKEN": "8"},
    )
    changed_policy = AgentContextManager(changed_policy_runtime, model="model-a", calibration_dir=calibration_dir)
    check("calibration is isolated when the estimator policy changes", changed_policy.budgeter.scale == 1.0)
    identified = AgentContextManager(runtime, model="model-a", calibration_dir=calibration_dir, calibration_identity="digest-a/template-a")
    identified.observe(small, baseline_estimate * 3)
    same_identity = AgentContextManager(runtime, model="model-a", calibration_dir=calibration_dir, calibration_identity="digest-a/template-a")
    changed_identity = AgentContextManager(runtime, model="model-a", calibration_dir=calibration_dir, calibration_identity="digest-b/template-a")
    check("matching immutable calibration identity reloads", same_identity.budgeter.scale >= 3.0)
    check("changed calibration identity starts fresh", changed_identity.budgeter.scale == 1.0)
    check("tag-only calibration reports its limited scope", restarted_manager.metadata()["calibration_scope"] == "model_tag_and_estimator")
    invalid_cache_path = calibration_dir / f"{first_manager._model_key()}-invalid.json"
    invalid_cache_path.write_text("[]", encoding="utf-8")
    invalid_record_manager = AgentContextManager(runtime, model="model-a", calibration_dir=calibration_dir)
    check("malformed calibration records do not prevent loading valid samples", invalid_record_manager.budgeter.scale >= 2.0)
    invalid_cache_path.write_text(json.dumps({**first_manager._calibration_contract(), "scale": 2, "samples": float("inf")}), encoding="utf-8")
    invalid_sample_manager = AgentContextManager(runtime, model="model-a", calibration_dir=calibration_dir)
    check("non-finite cached sample counts do not break startup", invalid_sample_manager.budgeter.scale >= 2.0)

try:
    ContextPolicy.from_env({"FREELLAMA_AGENT_COMPACT_RETAIN_RATIO": "1.0"})
except ValueError:
    invalid_policy_rejected = True
else:
    invalid_policy_rejected = False
check("invalid context configuration fails fast", invalid_policy_rejected)

try:
    AgentRuntimeConfig.from_env(
        default_tool_timeout_seconds=30,
        env={"FREELLAMA_AGENT_KEEP_ALIVE": ""},
    )
except ValueError:
    empty_keep_alive_rejected = True
else:
    empty_keep_alive_rejected = False
check("empty keep-alive configuration fails fast", empty_keep_alive_rejected)

# --- typed coding-agent history ---------------------------------------------------------------
typed = [
    {"role": "system", "content": "SYSTEM_TYPED"},
    {"role": "user", "content": "TASK_TYPED", "images": ["base64-is-not-counted-as-text"]},
    {
        "role": "assistant",
        "content": "",
        "tool_calls": [{"function": {"name": "read", "arguments": {"path": "src/a.py"}}}],
    },
    {"role": "tool", "tool_name": "read", "content": "z" * 40_000},
]
check(
    "images use a bounded multimodal estimate rather than base64 character count",
    messages_tokens(typed[:2]) >= IMAGE_TOKEN_ESTIMATE,
)
fitted_typed, compacted_typed = fit_to_context(typed, num_ctx=4096, num_predict=512, keep_recent=0)
check("typed tool history compacts", compacted_typed)
check("tool role survives compaction", fitted_typed[3]["role"] == "tool")
check("tool name survives compaction", fitted_typed[3]["tool_name"] == "read")
check("assistant tool_calls survive compaction", fitted_typed[2]["tool_calls"] == typed[2]["tool_calls"])
check(
    "typed history fits after compaction",
    messages_tokens(fitted_typed) <= context_budget(4096, 512),
)

forced = build_conversation(turns=10, observation_chars=3000)
forced.append({"role": "user", "content": "Return the final answer now."})
fitted_forced, _ = fit_to_context(forced, num_ctx=8192, num_predict=512)
check(
    "forced finalization is fitted after its instruction is appended",
    messages_tokens(fitted_forced) <= context_budget(8192, 512),
)


# --- pagination: the point is that NOTHING is lost -------------------------------------------
big = "\n".join(f"src/mod_{i}.py:{i}: def handler_{i}(request, ctx):" for i in range(900))
pages = []
p, total = 1, None
while True:
    chunk, got, tot = paginate(big, p)
    total = tot
    pages.append(chunk)
    if got >= tot:
        break
    p += 1
rejoined = "".join(pages)
check("pagination covers a large observation in several pages", total > 1, f"total={total}")
check(
    "REASSEMBLED PAGES ARE BYTE-IDENTICAL TO THE INPUT — no data loss",
    rejoined == big,
    f"{len(rejoined)} chars vs {len(big)} original",
)
check("no page exceeds the page budget", all(len(c) <= 3000 for c in pages))
check(
    "pages split on line boundaries, never mid-line",
    all(c.endswith("\n") for c in pages[:-1]),
)
check("a short observation is a single page", paginate("one line")[2] == 1)
check("empty input is one empty page", paginate("") == ("", 1, 1))
check("page numbers clamp instead of erroring", paginate(big, 9999)[1] == total)
long_line = "prefix\n" + "界" * 10_000 + "\r\ntail"
long_line_total = paginate(long_line, page_size=128)[2]
long_line_pages = [paginate(long_line, p, 128)[0] for p in range(1, long_line_total + 1)]
check("oversized Unicode lines obey the hard page limit", all(len(c) <= 128 for c in long_line_pages))
check("oversized-line pagination remains lossless", "".join(long_line_pages) == long_line)
try:
    paginate("", page_size=0)
except ValueError:
    invalid_page_size_rejected = True
else:
    invalid_page_size_rejected = False
check("pagination rejects a non-positive page size even for empty input", invalid_page_size_rejected)

# --- the footer must tell the model how to get the rest --------------------------------------
foot = page_footer(step=3, page=1, total=4, total_chars=len(big))
check("footer names the page and the total", "page 1 of 4" in foot)
check("footer gives an exact next-page action", '"action":"page","step":3,"page":2' in foot)
check("footer states nothing was discarded", "nothing discarded" in foot)
check("footer says paging does not re-run the command", "does NOT re-run" in foot)
check("a single page gets no footer at all", page_footer(1, 1, 1, 10) == "")
final_foot = page_footer(3, 4, 4, len(big))
check("final page explicitly ends pagination", "End of stored output" in final_foot)
check("final page never instructs a wraparound", '"action":"page"' not in final_foot and "next page" not in final_foot)

# --- the store keeps everything retrievable -------------------------------------------------
store = ObservationStore()
store.put(1, big)
store.put(2, "small")
body, footer = store.view(1, 2)
check("store serves an arbitrary page of a stored observation", body in pages and body == pages[1])
check("store's footer advertises page 3 next", '"page":3' in footer)
check("store returns the full short observation with no footer", store.view(2) == ("small", ""))
check(
    "store round-trips the original exactly",
    store.get(1) == big,
    "the full text must remain retrievable, that is what makes paging lossless",
)
missing_body, _ = store.view(99)
check("an unknown step explains itself rather than returning empty", "no stored output" in missing_body)
check("an unknown step lists the steps that do exist", "1, 2" in missing_body)

small_pages = ObservationStore(page_size=128)
small_pages.put(1, big)
_, small_page_footer = small_pages.view(1)
check("observation page size is configurable", "page 1 of" in small_page_footer and "page 1 of 1" not in small_page_footer)
store.close()
small_pages.close()

# The hot loop must retain page descriptors, not every full observation or page offset.
disk_payload = "開始\n" + "🦙\"\\\r\n" * 100_000 + "END"
with ObservationStore(page_size=128) as disk_store:
    spool_directory = disk_store.directory
    tracemalloc.start()
    disk_store.put(1, disk_payload, metadata={"action": "shell", "status": "ok", "source": "src/router.py"})
    _, peak_bytes = tracemalloc.get_traced_memory()
    tracemalloc.stop()
    check("spooling uses bounded additional memory", peak_bytes < 1_000_000, str(peak_bytes))
    check("disk store keeps full Unicode output exact", disk_store.get(1) == disk_payload)
    recovered = []
    total_pages = disk_store.page_count(1)
    for page in range(total_pages, 0, -1):
        recovered.append(disk_store.view(1, page)[0])
    check("disk store supports arbitrary page seeks losslessly", "".join(reversed(recovered)) == disk_payload)
    ledger_message = disk_store.message(1, 2)
    ledger_compact = _compact(ledger_message, ContextPolicy())
    check("structured compaction preserves action status source and page", all(part in ledger_compact for part in ['"action":"shell"', '"status":"ok"', '"source":"src/router.py"', '"page":2']))
    with tempfile.TemporaryDirectory() as result_root:
        result_path = Path(result_root) / "result.json"
        result = {"final_answer": "ok", "tool_calls": [{"name": "shell", "status": "ok"}], "usage": {"input_tokens": 1}}
        tracemalloc.start()
        disk_store.write_result(result_path, result)
        _, write_peak = tracemalloc.get_traced_memory()
        tracemalloc.stop()
        check("final audit JSON streams with bounded memory", write_peak < 2_000_000, str(write_peak))
        decoded = json.loads(result_path.read_text(encoding="utf-8"))
        check("streamed final result preserves the public JSON shape", decoded == {**result, "tool_calls": [{"name": "shell", "status": "ok", "result": disk_payload}]})
        previous_result = result_path.read_bytes()
        try:
            disk_store.write_result(result_path, {"tool_calls": [{"name": "shell"}, {"name": "missing"}]})
        except KeyError:
            pass
        check("failed finalization leaves the previous result intact", result_path.read_bytes() == previous_result)
        check("failed finalization removes its partial output", not list(Path(result_root).glob("*.tmp")))
check("closing the store removes its temporary spool", not spool_directory.exists())
try:
    with ObservationStore() as failing_store:
        failed_directory = failing_store.directory
        raise RuntimeError("test cleanup")
except RuntimeError:
    pass
check("exceptional exit cleans the observation spool", not failed_directory.exists())

print()
if FAILURES:
    print(f"{len(FAILURES)} failure(s):")
    for failure in FAILURES:
        print(f"  - {failure}")
    raise SystemExit(1)
print("all agent-context contracts passed")
