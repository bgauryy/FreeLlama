#!/usr/bin/env python3
"""Agent B adapter: local Ollama model restricted to raw Linux/bash commands only.

Same adapter contract and decoding settings as octocode_agent.py (see
benchmark/harness/references/adapters.md and benchmark/local/docs/03-agent-b-bash.md) — the only
variable under test is the tool surface: this agent has no structured tool schema, just a shell.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from pathlib import Path
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from agent_context import REPEAT_NOTICE, call_signature, clip, fit_to_context

# Both limits used to be tail-only slices (`observation[-3000:]`), which kept the end of a command's
# output and discarded the beginning — the wrong half for `ls`, `grep -n`, or `head`. `clip` keeps
# both ends; see agent_context.py.
RESULT_CLIP_CHARS = 2000
OBSERVATION_CLIP_CHARS = 3000

DENYLIST = re.compile(
    r"\bsudo\b|\brm\s+-rf\s+/(?!\S)|:\(\)\s*\{.*:\|:.*\}|\bcurl\b|\bwget\b|\bnc\b|\bssh\b|>\s*/dev/(sd|nvme|disk)",
    re.IGNORECASE,
)


def request_json(url: str, payload: dict[str, Any]) -> dict[str, Any]:
    request = Request(url, data=json.dumps(payload).encode(), headers={"content-type": "application/json"}, method="POST")
    with urlopen(request, timeout=600) as response:
        return json.loads(response.read())


def parse_action(content: str) -> dict[str, Any]:
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


def run_shell(root: Path, command_text: str) -> str:
    if not command_text.strip():
        raise ValueError("empty shell command")
    if DENYLIST.search(command_text):
        raise ValueError("command blocked by safety denylist")
    result = subprocess.run(["/bin/bash", "-c", command_text], cwd=root, text=True, capture_output=True, timeout=30, check=False)
    output = (result.stdout or "") + (result.stderr or "")
    if not output.strip():
        output = f"(no output, exit code {result.returncode})"
    return output


def system_prompt() -> str:
    return """You are a local coding agent in an isolated benchmark workspace containing a pinned repository, rooted at the current directory. You solve tasks using ONLY raw POSIX shell commands — no editors, no special tools, no network access. Return exactly one JSON object per turn:

{"action":"shell","command":"one shell command, e.g. grep -n \\"class Group\\" click/src/click/core.py"}
{"action":"finish","answer":"concise final answer with repository-relative evidence"}

Use standard Unix utilities: ls, find, cat, grep, sed, awk, head, tail, wc, tree (if present). Chain
with pipes if needed, but keep each turn to a single shell invocation. Never edit files. Be decisive:
most tasks need 2-6 commands. Call finish as soon as the requested facts are established."""


def main() -> int:
    model = os.environ.get("FREELLAMA_TARGET_MODEL") or os.environ["FREELLAMA_BENCH_MODEL"]
    workspace = Path(os.environ["FREELLAMA_BENCH_WORKSPACE"]).resolve()
    prompt = Path(os.environ["FREELLAMA_BENCH_PROMPT"]).read_text(encoding="utf-8")
    result_path = Path(os.environ["FREELLAMA_AGENT_RESULT"])
    endpoint = os.environ.get("FREELLAMA_OLLAMA_ENDPOINT", "http://127.0.0.1:11434").rstrip("/")
    max_turns = int(os.environ.get("FREELLAMA_AGENT_MAX_TURNS", "10"))
    messages: list[dict[str, str]] = [
        {"role": "system", "content": system_prompt()},
        {"role": "user", "content": prompt},
    ]
    calls: list[dict[str, Any]] = []
    usage = {"input_tokens": 0, "output_tokens": 0, "cache_read_tokens": None, "cache_write_tokens": None}
    metrics = {"load_ms": 0.0, "prompt_eval_ms": 0.0, "eval_ms": 0.0}
    answer = ""
    failure: str | None = None
    context_compactions = 0
    chat_options = {"temperature": 0, "seed": 42, "num_ctx": 8192, "num_predict": 512}

    def call_model() -> dict[str, Any]:
        # The proxy already retries transient upstream 5xx errors (packages/rust-core/src/proxy.rs); this loop is a
        # second, slower layer for outages that outlast the proxy's own retry budget — losing a
        # whole multi-turn conversation to one bad turn would be wasteful.
        last_error: Exception | None = None
        for attempt in range(2):
            try:
                response = request_json(f"{endpoint}/api/chat", {"model": model, "messages": messages, "stream": False, "format": "json", "think": False, "keep_alive": "5m", "options": chat_options})
                break
            except (HTTPError, URLError, TimeoutError) as error:
                last_error = error
                if attempt == 0:
                    time.sleep(5)
        else:
            raise last_error  # type: ignore[misc]
        usage["input_tokens"] += int(response.get("prompt_eval_count", 0))
        usage["output_tokens"] += int(response.get("eval_count", 0))
        metrics["load_ms"] += float(response.get("load_duration", 0)) / 1_000_000
        metrics["prompt_eval_ms"] += float(response.get("prompt_eval_duration", 0)) / 1_000_000
        metrics["eval_ms"] += float(response.get("eval_duration", 0)) / 1_000_000
        return response

    seen_calls: dict[str, int] = {}
    for _ in range(max_turns):
        try:
            response = call_model()
            action = parse_action(str(response.get("message", {}).get("content", "")))
        except (HTTPError, URLError, TimeoutError, ValueError, json.JSONDecodeError) as error:
            failure = f"agent response failed: {type(error).__name__}: {error}"
            break
        if action["action"] == "finish":
            answer = str(action.get("answer", "")).strip()
            break
        started = time.perf_counter()
        command_text = str(action.get("command", ""))
        # An exact repeat costs a turn out of ten AND a second copy of an observation the model has
        # already seen. Answer it from the prior step instead of re-running the command.
        signature = call_signature({"command": command_text})
        if signature in seen_calls:
            observation = REPEAT_NOTICE
            status = "repeat"
        else:
            try:
                observation = run_shell(workspace, command_text)
                status = "ok"
            except (OSError, ValueError, subprocess.TimeoutExpired) as error:
                observation = f"tool error: {type(error).__name__}: {error}"
                status = "error"
            seen_calls[signature] = len(calls) + 1
        calls.append({
            "name": "shell",
            "raw_name": action.get("action", ""),
            "arguments": {"command": command_text},
            "status": status,
            "duration_ms": round((time.perf_counter() - started) * 1000, 3),
            "result": clip(observation, RESULT_CLIP_CHARS),
        })
        messages.append({"role": "assistant", "content": json.dumps(action, ensure_ascii=False)})
        remaining = max_turns - len(calls)
        messages.append({"role": "user", "content": f"Observation:\n{clip(observation, OBSERVATION_CLIP_CHARS)}\n\nCommands remaining: {remaining}. Finish now if the task is answerable; do not repeat prior commands."})
        messages, compacted = fit_to_context(messages, num_ctx=chat_options["num_ctx"], num_predict=chat_options["num_predict"])
        context_compactions += 1 if compacted else 0
    else:
        messages.append({"role": "user", "content": "Command budget is exhausted. Return exactly a finish JSON action now using the evidence collected. Do not request another command."})
        try:
            response = call_model()
            final_action = parse_action(str(response.get("message", {}).get("content", "")))
            if final_action.get("action") != "finish":
                raise ValueError("forced final response was not finish")
            answer = str(final_action.get("answer", "")).strip()
        except (HTTPError, URLError, TimeoutError, ValueError, json.JSONDecodeError) as error:
            failure = f"agent exceeded {max_turns} turns and finalization failed: {type(error).__name__}: {error}"

    if not answer:
        answer = failure or "agent stopped without a final answer"
    result = {
        "final_answer": answer,
        "tool_calls": calls,
        "usage": usage,
        "provider_metrics": metrics,
        "model_metadata": {
            "adapter": "bash_shell_agent_v1",
            "temperature": chat_options["temperature"],
            "seed": chat_options["seed"],
            "num_ctx": chat_options["num_ctx"],
            "num_predict": chat_options["num_predict"],
            "max_turns": max_turns,
            "context_compactions": context_compactions,
            "cache_token_metrics": "not_reported_by_ollama",
        },
    }
    result_path.parent.mkdir(parents=True, exist_ok=True)
    result_path.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(answer)
    return 1 if failure else 0


if __name__ == "__main__":
    raise SystemExit(main())
