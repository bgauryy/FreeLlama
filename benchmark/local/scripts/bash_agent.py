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

from agent_context import (
    AgentContextManager,
    AgentRuntimeConfig,
    PARSE_REPAIR_NOTICE,
    REPEAT_NOTICE,
    ObservationStore,
    call_signature,
    paginate,
    page_footer,
    parse_json_action,
    write_failure_result,
)
from agent_transport import chat_request, request_headers, unwrap_chat_response

# Nothing is clipped any more. `calls[].result` is written to result.json on disk and read by the
# MCP layer — it never enters the model's context, so there was never a reason to shorten it. What
# the MODEL sees is paginated instead: one page plus an exact instruction for fetching the next.
# See the pagination section of agent_context.py for why clipping was data loss, not economy.

DENYLIST = re.compile(
    r"\bsudo\b|\brm\s+-rf\s+/(?!\S)|:\(\)\s*\{.*:\|:.*\}|\bcurl\b|\bwget\b|\bnc\b|\bssh\b|>\s*/dev/(sd|nvme|disk)",
    re.IGNORECASE,
)

# Home and well-known absolute prefixes. A relative `grep /pattern/` is not a filesystem path;
# `/etc/passwd` is. `..` as a path component walks out of cwd=workspace.
_HOME_ESCAPE = re.compile(r"(?:^|[\s=\"'])(?:~(?:/|$)|\$HOME\b|\$\{HOME\})")
_ABS_PATH = re.compile(r"(?:^|[\s=\"'])(/(?:[^\s\"']+))")
_DOTDOT_PATH = re.compile(r"(?:^|[\s=\"'/])\.\.(?:/|[\s\"']|$)")
_FS_ABS_PREFIX = re.compile(
    r"^/(?:etc|usr|home|Users|var|tmp|private|opt|root|System|Library|bin|sbin|dev|Applications|Volumes)(?:/|$)"
)


def assert_command_confined(root: Path, command_text: str) -> None:
    """Reject commands that read outside the workspace. The MCP allowlist only constrains
    the workspace *root*; without this, `cat /etc/hosts` from cwd=root succeeds."""
    if _HOME_ESCAPE.search(command_text):
        raise ValueError("command blocked: home-directory path is outside the workspace")
    if _DOTDOT_PATH.search(command_text):
        raise ValueError("command blocked: '..' walks outside the workspace")
    root = root.resolve()
    for match in _ABS_PATH.finditer(command_text):
        raw = match.group(1)
        candidate = Path(raw)
        looks_like_fs = _FS_ABS_PREFIX.match(raw) is not None or candidate.exists()
        if not looks_like_fs:
            continue
        resolved = candidate.resolve()
        if resolved != root and root not in resolved.parents:
            raise ValueError(f"command blocked: path escapes workspace: {raw}")


def request_json(url: str, payload: dict[str, Any], timeout_seconds: float) -> dict[str, Any]:
    request = Request(url, data=json.dumps(payload).encode(), headers=request_headers(), method="POST")
    with urlopen(request, timeout=timeout_seconds) as response:
        return json.loads(response.read())


def parse_action(content: str) -> dict[str, Any]:
    value = parse_json_action(content)
    action = value["action"]
    if action == "shell" and (not isinstance(value.get("command"), str) or not value["command"].strip()):
        raise ValueError("shell action requires a non-empty string command")
    if action == "page" and (
        not isinstance(value.get("step"), int)
        or value["step"] < 1
        or not isinstance(value.get("page"), int)
        or value["page"] < 1
    ):
        raise ValueError("page action requires positive integer step and page")
    if action == "finish" and not isinstance(value.get("answer"), str):
        raise ValueError("finish action requires a string answer")
    if action not in {"shell", "page", "finish"}:
        raise ValueError(f"unsupported action for bash adapter: {action}")
    return value


def run_shell(root: Path, command_text: str, timeout_seconds: float) -> str:
    if not command_text.strip():
        raise ValueError("empty shell command")
    if DENYLIST.search(command_text):
        raise ValueError("command blocked by safety denylist")
    assert_command_confined(root, command_text)
    result = subprocess.run(
        ["/bin/bash", "-c", command_text],
        cwd=root,
        text=True,
        capture_output=True,
        timeout=timeout_seconds,
        check=False,
    )
    output = (result.stdout or "") + (result.stderr or "")
    if not output.strip():
        output = f"(no output, exit code {result.returncode})"
    return output


def system_prompt() -> str:
    return """You are a local coding agent in an isolated benchmark workspace containing a pinned repository, rooted at the current directory. You solve tasks using ONLY raw POSIX shell commands — no editors, no special tools, no network access. Return exactly one JSON object per turn:

{"action":"shell","command":"one shell command, e.g. grep -n \\"class Group\\" click/src/click/core.py"}
{"action":"page","step":2,"page":2}
{"action":"finish","answer":"concise final answer with repository-relative evidence"}

These are the complete action schemas. Use only `shell`, `page`, or `finish`; every shown field is
required with the shown type. An invalid shape is rejected and costs one bounded repair turn.

Long output is PAGINATED, never truncated: you are shown page 1 and told the total. Nothing is
discarded, so "not found" is only a real answer once you have seen every page you need. Send the
page action to read another page of a previous step — it re-reads stored output and does not re-run
the command, so it is cheaper than repeating the search.

Use standard Unix utilities: ls, find, cat, grep, sed, awk, head, tail, wc, tree (if present). Chain
with pipes if needed, but keep each turn to a single shell invocation. Never edit files. Be decisive:
most tasks need 2-6 commands. Call finish as soon as the requested facts are established.

SCOPE YOUR SEARCHES. A real workspace holds far more than its source, and an unscoped grep drowns
the answer in vendored and generated files. Always exclude them:
  grep -rn "PATTERN" . --exclude-dir={node_modules,target,.venv,venv,dist,build,.git,vendor,__pycache__,.octocode,site-packages}
Matches under fixtures/, test/fixtures/, mocks/ or examples/ are usually scaffolding, NOT the real
implementation — a mock named like the thing you are looking for is a trap, not an answer. Prefer
src/, packages/*/src/, lib/ and the repo root, and name the file you took the answer from.
If a search returns nothing, widen the pattern before concluding the thing does not exist: absence
of a grep hit is weak evidence, and "not found" is only a real answer once you have looked in the
source directories.
ASKED FOR A DEFAULT? Find where it is DECLARED, not where it appears. A value like a port or a
timeout is scattered across tests, docs and examples that merely pass it; those are occurrences, not
the default. The declaration is an attribute or initializer — `default_value = `, `unwrap_or(`,
`const `, `static `, a settings schema, a clap/argparse arg. Grep for the declaration form, and if
you can only find occurrences, say which file you took it from and that you did not find a
declaration. Test files (`tests/`, `*_test.*`, `*_contract.*`) define nothing — they consume it."""


def main() -> int:
    model = os.environ.get("FREELLAMA_TARGET_MODEL") or os.environ["FREELLAMA_BENCH_MODEL"]
    workspace = Path(os.environ["FREELLAMA_BENCH_WORKSPACE"]).resolve()
    prompt = Path(os.environ["FREELLAMA_BENCH_PROMPT"]).read_text(encoding="utf-8")
    result_path = Path(os.environ["FREELLAMA_AGENT_RESULT"])
    endpoint = os.environ.get("FREELLAMA_OLLAMA_ENDPOINT", "http://127.0.0.1:11434").rstrip("/")
    managed_endpoint = os.environ.get("FREELLAMA_AGENT_MANAGED_ENDPOINT", "").rstrip("/")
    execution_preference = os.environ.get("FREELLAMA_AGENT_EXECUTION_PREFERENCE", "auto")
    min_placement_evidence = os.environ.get("FREELLAMA_AGENT_MIN_PLACEMENT_EVIDENCE", "configured")
    try:
        runtime = AgentRuntimeConfig.from_env(default_tool_timeout_seconds=30)
    except ValueError as error:
        answer = f"invalid research adapter configuration: {error}"
        write_failure_result(result_path, answer)
        print(answer)
        return 1
    messages: list[dict[str, Any]] = [
        {"role": "system", "content": system_prompt()},
        {"role": "user", "content": prompt},
    ]
    calls: list[dict[str, Any]] = []
    usage = {"input_tokens": 0, "output_tokens": 0, "cache_read_tokens": None, "cache_write_tokens": None}
    metrics = {"load_ms": 0.0, "prompt_eval_ms": 0.0, "eval_ms": 0.0}
    execution_receipts: list[dict[str, Any]] = []
    answer = ""
    failure: str | None = None
    calibration_dir = os.environ.get("FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR", "").strip()
    context_manager = AgentContextManager(
        runtime,
        model=model,
        calibration_dir=Path(calibration_dir) if calibration_dir else None,
    )
    try:
        messages = context_manager.fit(messages)
    except ValueError as error:
        answer = f"research task does not fit the configured context safely: {error}"
        write_failure_result(result_path, answer)
        print(answer)
        return 1
    chat_options = {
        "temperature": runtime.temperature,
        "seed": runtime.seed,
        "num_ctx": runtime.num_ctx,
        "num_predict": runtime.num_predict,
    }

    def call_model() -> dict[str, Any]:
        # The proxy already retries 500/502/504 (packages/rust-core/src/proxy.rs); this loop is a
        # second, slower layer for outages that outlast the proxy's own retry budget — losing a
        # whole multi-turn conversation to one bad turn would be wasteful.
        last_error: Exception | None = None
        for attempt in range(runtime.retry_attempts):
            try:
                transport_endpoint = (
                    f"{managed_endpoint}/_freellama/v1/tasks" if managed_endpoint else endpoint
                )
                url, payload = chat_request(
                    transport_endpoint,
                    model,
                    messages,
                    chat_options,
                    runtime.think,
                    runtime.keep_alive,
                    execution_preference,
                    min_placement_evidence,
                )
                response = unwrap_chat_response(
                    request_json(url, payload, runtime.request_timeout_seconds), execution_receipts
                )
                break
            except (HTTPError, URLError, TimeoutError) as error:
                last_error = error
                if attempt + 1 < runtime.retry_attempts:
                    time.sleep(runtime.retry_backoff_seconds)
        else:
            raise last_error  # type: ignore[misc]
        usage["input_tokens"] += int(response.get("prompt_eval_count", 0))
        usage["output_tokens"] += int(response.get("eval_count", 0))
        metrics["load_ms"] += float(response.get("load_duration", 0)) / 1_000_000
        metrics["prompt_eval_ms"] += float(response.get("prompt_eval_duration", 0)) / 1_000_000
        metrics["eval_ms"] += float(response.get("eval_duration", 0)) / 1_000_000
        context_manager.observe(messages, response.get("prompt_eval_count"))
        return response

    def refit() -> bool:
        nonlocal messages, failure
        try:
            messages = context_manager.fit(messages)
            return True
        except ValueError as error:
            failure = f"research context cannot be fitted safely: {error}"
            return False

    seen_calls: dict[str, int] = {}
    observations = ObservationStore(runtime.context.observation_page_chars)
    parse_failures = 0
    for _ in range(runtime.max_turns):
        # Transport failures are terminal (call_model already retried them). A *parse* failure is
        # not: tell the model what was wrong with its reply and let it correct itself, keeping every
        # tool result gathered so far.
        try:
            response = call_model()
        except (HTTPError, URLError, TimeoutError) as error:
            failure = f"agent response failed: {type(error).__name__}: {error}"
            break
        raw = str(response.get("message", {}).get("content", ""))
        try:
            action = parse_action(raw)
        except (ValueError, json.JSONDecodeError) as error:
            parse_failures += 1
            if parse_failures > runtime.max_parse_repairs:
                failure = f"agent gave {parse_failures} unparseable replies: {type(error).__name__}: {error}"
                break
            messages.append({"role": "assistant", "content": raw[: runtime.parse_repair_echo_chars]})
            messages.append({"role": "user", "content": PARSE_REPAIR_NOTICE})
            if not refit():
                break
            continue
        parse_failures = 0
        # Paging re-serves stored output. It costs a turn but never a subprocess, and no byte of
        # any observation is ever unreachable — which is the whole point of paginating instead of
        # clipping.
        if action["action"] == "page":
            try:
                want_step = int(action.get("step", 0))
                want_page = int(action.get("page", 1))
            except (TypeError, ValueError):
                want_step, want_page = 0, 1
            body, footer = observations.view(want_step, want_page)
            messages.append({"role": "assistant", "content": json.dumps(action, ensure_ascii=False)})
            messages.append({"role": "user", "content": f"Observation (step {want_step}):\n{body}{footer}"})
            if not refit():
                break
            continue
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
                observation = run_shell(workspace, command_text, runtime.tool_timeout_seconds)
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
            "result": observation,
        })
        messages.append({"role": "assistant", "content": json.dumps(action, ensure_ascii=False)})
        remaining = runtime.max_turns - len(calls)
        step = len(calls)
        observations.put(step, observation)
        body, shown_page, total_pages = paginate(
            observation, page_size=runtime.context.observation_page_chars
        )
        footer = page_footer(step, shown_page, total_pages, len(observation))
        messages.append({"role": "user", "content": f"Observation (step {step}):\n{body}{footer}\n\nCommands remaining: {remaining}. Finish now if the task is answerable; do not repeat prior commands."})
        if not refit():
            break
    else:
        messages.append({"role": "user", "content": "Command budget is exhausted. Return exactly a finish JSON action now using the evidence collected. Do not request another command."})
        if refit():
            try:
                response = call_model()
                final_action = parse_action(str(response.get("message", {}).get("content", "")))
                if final_action.get("action") != "finish":
                    raise ValueError("forced final response was not finish")
                answer = str(final_action.get("answer", "")).strip()
            except (HTTPError, URLError, TimeoutError, ValueError, json.JSONDecodeError) as error:
                failure = f"agent exceeded {runtime.max_turns} turns and finalization failed: {type(error).__name__}: {error}"

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
            "max_turns": runtime.max_turns,
            "runtime_config": runtime.metadata(),
            "context_management": context_manager.metadata(),
            "context_compactions": context_manager.compactions,
            "transport": "managed" if managed_endpoint else "direct_ollama",
            "execution_preference": execution_preference,
            "min_placement_evidence": min_placement_evidence,
            "execution_receipts": execution_receipts,
            "cache_token_metrics": "not_reported_by_ollama",
        },
    }
    result_path.parent.mkdir(parents=True, exist_ok=True)
    result_path.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(answer)
    return 1 if failure else 0


if __name__ == "__main__":
    raise SystemExit(main())
