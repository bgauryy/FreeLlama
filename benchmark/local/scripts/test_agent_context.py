#!/usr/bin/env python3
"""Contract tests for the adapters' context management. Run: python3 test_agent_context.py"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from agent_context import (  # noqa: E402
    CHARS_PER_TOKEN,
    call_signature,
    clip,
    context_budget,
    fit_to_context,
    messages_tokens,
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

# --- pathological case: recent turns alone exceed the window ---------------------------------
huge = build_conversation(turns=2, observation_chars=60_000)
fitted_huge, _ = fit_to_context(huge, num_ctx=8192, num_predict=512)
check(
    "even oversized recent observations are clipped to fit",
    messages_tokens(fitted_huge) <= context_budget(8192, 512),
    f"{messages_tokens(fitted_huge)} tokens",
)
check("the system prompt still survives the pathological case", "SYSTEM_PROMPT_SENTINEL" in fitted_huge[0]["content"])

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

print()
if FAILURES:
    print(f"{len(FAILURES)} failure(s):")
    for failure in FAILURES:
        print(f"  - {failure}")
    raise SystemExit(1)
print("all agent-context contracts passed")
