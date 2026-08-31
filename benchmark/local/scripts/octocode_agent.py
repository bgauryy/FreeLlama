#!/usr/bin/env python3
"""Agent A adapter: local Ollama model whose only research tool is the `octocode` CLI.

Implements the FreeLlama benchmark adapter contract (benchmark/harness/references/adapters.md):
reads FREELLAMA_BENCH_MODEL/PROMPT/WORKSPACE/AGENT_RESULT, drives its own Ollama chat loop, and
writes agent-result.json. See benchmark/local/docs/02-agent-a-octocode.md for the full spec this
mirrors verbatim.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from agent_context import (
    MAX_PARSE_REPAIRS,
    PARSE_REPAIR_NOTICE,
    REPEAT_NOTICE,
    ObservationStore,
    call_signature,
    fit_to_context,
    paginate,
    page_footer,
)

OCTOCODE_TOOLS = {"localViewStructure", "localFindFiles", "localSearchCode", "localGetFileContent", "lspGetSemantics"}
PATH_KEYS = ("path", "uri")
# Nothing is clipped any more — see the pagination section of agent_context.py. `calls[].result`
# goes to result.json on disk and never into the model's context, so it is stored in full; the model
# is shown one page at a time with an exact instruction for fetching the next.


def request_json(url: str, payload: dict[str, Any]) -> dict[str, Any]:
    request = Request(url, data=json.dumps(payload).encode(), headers={"content-type": "application/json"}, method="POST")
    with urlopen(request, timeout=600) as response:
        return json.loads(response.read())


def safe_resolve(root: Path, value: str) -> Path:
    candidate = Path(value)
    candidate = candidate if candidate.is_absolute() else (root / candidate)
    candidate = candidate.resolve()
    if candidate != root and root not in candidate.parents:
        raise ValueError(f"path escapes benchmark workspace: {value}")
    if ".git" in candidate.parts:
        raise ValueError("git metadata is outside the task surface")
    return candidate


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


def run_octocode(root: Path, tool_name: str, queries: dict[str, Any]) -> str:
    if tool_name not in OCTOCODE_TOOLS:
        raise ValueError(f"unsupported octocode tool: {tool_name}")
    resolved = dict(queries)
    for key in PATH_KEYS:
        if key in resolved and isinstance(resolved[key], str):
            resolved[key] = str(safe_resolve(root, resolved[key]))
    command = ["npx", "octocode", "tools", tool_name, "--queries", json.dumps(resolved), "--compact"]
    result = subprocess.run(command, cwd=root, text=True, capture_output=True, timeout=45, check=False)
    output = (result.stdout or "") + (("\nSTDERR:\n" + result.stderr) if result.returncode != 0 else "")
    if not output.strip():
        output = "(empty result)"
    return output


def system_prompt(workspace: str) -> str:
    return f"""You are a local coding agent in an isolated benchmark workspace at {workspace}. You may not read or search files directly — you can only call the `octocode` local tools below. Return exactly one JSON object per turn.

Tools (call as {{"action":"octocode","tool":"<name>","queries":{{...}}}}):

localViewStructure - browse a directory tree, no content loaded; cheapest first orientation step.
  queries: path (string, required, absolute), maxDepth (int), recursive (bool), filesOnly (bool),
  directoriesOnly (bool), pattern (glob/substring filter).

localFindFiles - find files/dirs by name, glob, regex, or type; returns paths only, not content.
  queries: path (string, required, absolute), names (array of globs), pathPattern (glob over full
  path), regex (basename regex), entryType ("f"|"d"), excludeDir (array, e.g. ["node_modules",".git"]).

localSearchCode - search file contents for text/regex; returns file+line matches. Your main tool.
  queries: path (string, required, absolute), keywords (string; literal or regex search term),
  mode ("paginated"|"discovery"|"detailed"), include/exclude (glob arrays),
  caseInsensitive (bool), maxFiles (int).
  Do NOT pass mode "structural" with keywords — structural (AST) search takes `pattern`/`rule`
  instead and hard-errors on keywords, costing you a turn for nothing.

localGetFileContent - read one file or a line range/matched slice of it.
  queries: path (string, required, absolute), fullContent (bool; small files only), startLine +
  endLine (ints, both required together), matchString (anchor text/regex), minify
  ("none"|"standard"|"symbols" — "symbols" gives a cheap outline first).

lspGetSemantics - LSP semantic queries: definitions, references, callers/callees, symbol outline.
  queries: uri (string, required, absolute path), type ("definition"|"references"|"callers"|
  "callees"|"documentSymbols"|"hover"|...), symbolName (exact identifier), lineHint (int; get this
  from a prior search/documentSymbols call, never guess it).
  An EMPTY result is ambiguous, and it will not tell you which case you hit: it reports
  serverAvailable=true whether the language server is still indexing the project or genuinely
  cannot analyse the file. Measured here on the same files: a cold server returned
  totalSymbols=0 for Rust, and once warm the identical call returned 35. Python returned 139-430,
  TypeScript 45. So totalSymbols=0 means "ask again or search instead", NOT "no such symbol" and
  NOT "unsupported". Do not spend more than one retry on it — prefer localSearchCode, which needs
  no index and cannot be cold.

Read another page of an earlier step: {{"action":"page","step":2,"page":2}}
Long output is PAGINATED, never truncated — you see page 1 and the total, and nothing is discarded.
"Not found" is only a real answer once you have seen every page you need. Paging re-reads stored
output and does NOT re-run the tool, so it is cheaper than repeating a search.

Finish with: {{"action":"finish","answer":"concise final answer with repository-relative evidence"}}

SCOPE YOUR SEARCHES. Pass excludeDir/exclude on every search in a real workspace —
["node_modules","target",".venv","dist","build",".git","vendor","__pycache__",".octocode"] — or
vendored and generated files will bury the answer. Matches under fixtures/, mocks/ or examples/ are
scaffolding, NOT the real implementation. Prefer src/, packages/*/src/ and lib/, and name the file
you took the answer from. Absence of a match is weak evidence: widen the pattern before concluding
something does not exist.
ASKED FOR A DEFAULT? Find where it is DECLARED, not where it appears. A value like a port or a
timeout is scattered across tests, docs and examples that merely pass it; those are occurrences, not
the default. The declaration is an attribute or initializer — `default_value = `, `unwrap_or(`,
`const `, `static `, a settings schema, a clap/argparse arg. Grep for the declaration form, and if
you can only find occurrences, say which file you took it from and that you did not find a
declaration. Test files (`tests/`, `*_test.*`, `*_contract.*`) define nothing — they consume it.

All paths you pass must resolve inside the workspace; relative paths are resolved against the
workspace root automatically. Orient cheap (localViewStructure/documentSymbols) before reading in
full. Be decisive: most tasks need 2-6 tool calls. Never edit or write files — you only have
read-only tools. Call finish as soon as the requested facts are established."""


def main() -> int:
    model = os.environ.get("FREELLAMA_TARGET_MODEL") or os.environ["FREELLAMA_BENCH_MODEL"]
    workspace = Path(os.environ["FREELLAMA_BENCH_WORKSPACE"]).resolve()
    prompt = Path(os.environ["FREELLAMA_BENCH_PROMPT"]).read_text(encoding="utf-8")
    result_path = Path(os.environ["FREELLAMA_AGENT_RESULT"])
    endpoint = os.environ.get("FREELLAMA_OLLAMA_ENDPOINT", "http://127.0.0.1:11434").rstrip("/")
    max_turns = int(os.environ.get("FREELLAMA_AGENT_MAX_TURNS", "10"))
    # Tunable because the right value is machine-dependent: prefix KV-cache reuse is real (a warm
    # prefix re-serves in ~0.3s vs ~19s cold), so a LARGER window is cheaper than it looks — and it
    # avoids fit_to_context compaction, which edits the byte prefix and invalidates the cache from
    # that point. With OLLAMA_KV_CACHE_TYPE=q8_0, 16384 costs the same KV memory as 8192 at f16.
    num_ctx = int(os.environ.get("FREELLAMA_AGENT_NUM_CTX", "8192"))
    messages: list[dict[str, str]] = [
        {"role": "system", "content": system_prompt(str(workspace))},
        {"role": "user", "content": prompt},
    ]
    calls: list[dict[str, Any]] = []
    usage = {"input_tokens": 0, "output_tokens": 0, "cache_read_tokens": None, "cache_write_tokens": None}
    metrics = {"load_ms": 0.0, "prompt_eval_ms": 0.0, "eval_ms": 0.0}
    answer = ""
    failure: str | None = None
    context_compactions = 0
    chat_options = {"temperature": 0, "seed": 42, "num_ctx": num_ctx, "num_predict": 512}

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
    observations = ObservationStore()
    parse_failures = 0
    for _ in range(max_turns):
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
            if parse_failures > MAX_PARSE_REPAIRS:
                failure = f"agent gave {parse_failures} unparseable replies: {type(error).__name__}: {error}"
                break
            messages.append({"role": "assistant", "content": raw[:500]})
            messages.append({"role": "user", "content": PARSE_REPAIR_NOTICE})
            messages, compacted = fit_to_context(messages, num_ctx=chat_options["num_ctx"], num_predict=chat_options["num_predict"])
            context_compactions += 1 if compacted else 0
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
            messages, compacted = fit_to_context(messages, num_ctx=chat_options["num_ctx"], num_predict=chat_options["num_predict"])
            context_compactions += 1 if compacted else 0
            continue
        if action["action"] == "finish":
            answer = str(action.get("answer", "")).strip()
            break
        started = time.perf_counter()
        tool_name = str(action.get("tool", ""))
        queries = action.get("queries", {}) if isinstance(action.get("queries"), dict) else {}
        # An exact repeat costs a turn out of ten AND a second copy of an observation the model
        # has already seen. Answer it from the prior step instead of re-running the subprocess.
        signature = call_signature({"tool": tool_name, "queries": queries})
        repeated = signature in seen_calls
        if repeated:
            observation = REPEAT_NOTICE
            status = "repeat"
        else:
            try:
                observation = run_octocode(workspace, tool_name, queries)
                status = "ok"
            except (OSError, ValueError, subprocess.TimeoutExpired) as error:
                observation = f"tool error: {type(error).__name__}: {error}"
                status = "error"
            seen_calls[signature] = len(calls) + 1
        calls.append({
            "name": f"mcp.octocode.{tool_name}" if tool_name else "mcp.octocode.unknown",
            "raw_name": action.get("action", ""),
            "arguments": {"tool": tool_name, "queries": queries},
            "status": status,
            "duration_ms": round((time.perf_counter() - started) * 1000, 3),
            "result": observation,
        })
        messages.append({"role": "assistant", "content": json.dumps(action, ensure_ascii=False)})
        remaining = max_turns - len(calls)
        step = len(calls)
        observations.put(step, observation)
        body, shown_page, total_pages = paginate(observation)
        footer = page_footer(step, shown_page, total_pages, len(observation))
        messages.append({"role": "user", "content": f"Observation (step {step}):\n{body}{footer}\n\nTool calls remaining: {remaining}. Finish now if the task is answerable; do not repeat prior calls."})
        messages, compacted = fit_to_context(messages, num_ctx=chat_options["num_ctx"], num_predict=chat_options["num_predict"])
        context_compactions += 1 if compacted else 0
    else:
        messages.append({"role": "user", "content": "Tool budget is exhausted. Return exactly a finish JSON action now using the evidence collected. Do not request another tool."})
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
            "adapter": "octocode_cli_agent_v1",
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
