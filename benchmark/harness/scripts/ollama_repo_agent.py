#!/usr/bin/env python3
"""Small, isolated repository agent adapter for local Ollama models."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import time
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


def request_json(url: str, payload: dict[str, Any]) -> dict[str, Any]:
    request = Request(
        url,
        data=json.dumps(payload).encode(),
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urlopen(request, timeout=600) as response:
        return json.loads(response.read())


def safe_path(root: Path, value: str) -> Path:
    candidate = (root / value).resolve()
    if candidate != root and root not in candidate.parents:
        raise ValueError("path escapes benchmark workspace")
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
        start, end = text.find("{"), text.rfind("}")
        if start < 0 or end <= start:
            raise ValueError("model did not return a JSON action") from None
        value = json.loads(text[start : end + 1])
    if not isinstance(value, dict) or not isinstance(value.get("action"), str):
        raise ValueError("model action must be a JSON object with an action")
    return value


def list_files(root: Path, action: dict[str, Any]) -> str:
    path = safe_path(root, str(action.get("path", ".")))
    if not path.is_dir():
        raise ValueError("list path is not a directory")
    files = [
        item.relative_to(root).as_posix()
        for item in sorted(path.rglob("*"))
        if item.is_file() and ".git" not in item.parts and "__pycache__" not in item.parts
    ]
    return "\n".join(files[:80]) + ("\n...truncated" if len(files) > 80 else "")


def search(root: Path, action: dict[str, Any]) -> str:
    query = str(action.get("query", ""))
    if not query:
        raise ValueError("search query is empty")
    path = safe_path(root, str(action.get("path", ".")))
    matches: list[str] = []
    candidates = [path] if path.is_file() else sorted(path.rglob("*"))
    for candidate in candidates:
        if not candidate.is_file() or ".git" in candidate.parts or candidate.stat().st_size > 1_000_000:
            continue
        try:
            lines = candidate.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        for number, line in enumerate(lines, start=1):
            if query.lower() in line.lower():
                relative = candidate.relative_to(root).as_posix()
                matches.append(f"{relative}:{number}:{line[:300]}")
                if len(matches) >= 40:
                    return "\n".join(matches) + "\n...truncated"
    return "\n".join(matches) or "no matches"


def read_file(root: Path, action: dict[str, Any]) -> str:
    path = safe_path(root, str(action.get("path", "")))
    if not path.is_file() or path.stat().st_size > 1_000_000:
        raise ValueError("read path is not a small regular file")
    lines = path.read_text(encoding="utf-8").splitlines()
    start = max(1, int(action.get("start_line", 1)))
    end = min(len(lines), int(action.get("end_line", start + 159)), start + 159)
    return "\n".join(f"{number}: {lines[number - 1]}" for number in range(start, end + 1))


def edit_file(root: Path, action: dict[str, Any]) -> str:
    path = safe_path(root, str(action.get("path", "")))
    old, new = str(action.get("old", "")), str(action.get("new", ""))
    if not path.is_file() or not old or len(new) > 50_000:
        raise ValueError("edit requires a file, non-empty old text, and bounded replacement")
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise ValueError(f"edit old text must match exactly once, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")
    return f"updated {path.relative_to(root).as_posix()}"


def write_file(root: Path, action: dict[str, Any]) -> str:
    path = safe_path(root, str(action.get("path", "")))
    content = str(action.get("content", ""))
    if path.exists() or len(content) > 50_000:
        raise ValueError("write creates one new bounded file only")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return f"created {path.relative_to(root).as_posix()}"


def run_tests(root: Path, action: dict[str, Any]) -> str:
    repo_name = str(action.get("repo", ""))
    if repo_name not in {"click", "itsdangerous"}:
        raise ValueError("test repo must be click or itsdangerous")
    repo = safe_path(root, repo_name)
    target = str(action.get("target", "")).strip()
    if ".." in target or target.startswith("/"):
        raise ValueError("unsafe test target")
    command = [sys.executable, "-m", "pytest", "-q"]
    if target:
        command.append(target)
    environment = os.environ.copy()
    environment["PYTHONPATH"] = str(repo / "src")
    started = time.perf_counter()
    result = subprocess.run(
        command,
        cwd=repo,
        env=environment,
        text=True,
        capture_output=True,
        timeout=120,
        check=False,
    )
    output = (result.stdout + result.stderr)[-6000:]
    return json.dumps(
        {
            "exit_code": result.returncode,
            "duration_ms": round((time.perf_counter() - started) * 1000, 3),
            "output": output,
        }
    )


def tool(action: dict[str, Any], root: Path) -> tuple[str, str]:
    name = action["action"]
    handlers = {
        "list": list_files,
        "search": search,
        "read": read_file,
        "edit": edit_file,
        "write": write_file,
        "test": run_tests,
    }
    if name not in handlers:
        raise ValueError(f"unsupported action: {name}")
    return name, handlers[name](root, action)


def system_prompt() -> str:
    return """You are a local coding agent in an isolated benchmark workspace containing pinned `click/` and `itsdangerous/` repositories. Work only through JSON actions. Return exactly one JSON object per turn.
Actions:
{"action":"list","path":"relative/dir"}
{"action":"search","query":"literal","path":"relative/path"}
{"action":"read","path":"relative/file","start_line":1,"end_line":200}
{"action":"edit","path":"relative/file","old":"exact text","new":"replacement"}
{"action":"write","path":"new relative file","content":"text"}
{"action":"test","repo":"click|itsdangerous","target":"optional pytest target"}
{"action":"finish","answer":"concise final answer with repository-relative evidence and tests"}
Inspect before editing. Make the smallest correct change. Never edit tests merely to hide a failure. Use test after changes. Be decisive: most tasks need 2-6 tool actions. Do not reread overlapping ranges or explore unrelated files. Call finish as soon as the requested facts or verification are sufficient."""


def main() -> int:
    model = os.environ["FREELLAMA_BENCH_MODEL"]
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
    for _ in range(max_turns):
        try:
            response = request_json(
                f"{endpoint}/api/chat",
                {
                    "model": model,
                    "messages": messages,
                    "stream": False,
                    "format": "json",
                    "think": False,
                    "keep_alive": "5m",
                    "options": {"temperature": 0, "seed": 42, "num_ctx": 8192, "num_predict": 512},
                },
            )
            content = str(response.get("message", {}).get("content", ""))
            usage["input_tokens"] += int(response.get("prompt_eval_count", 0))
            usage["output_tokens"] += int(response.get("eval_count", 0))
            metrics["load_ms"] += float(response.get("load_duration", 0)) / 1_000_000
            metrics["prompt_eval_ms"] += float(response.get("prompt_eval_duration", 0)) / 1_000_000
            metrics["eval_ms"] += float(response.get("eval_duration", 0)) / 1_000_000
            action = parse_action(content)
        except (HTTPError, URLError, TimeoutError, ValueError, json.JSONDecodeError) as error:
            failure = f"agent response failed: {type(error).__name__}: {error}"
            break
        if action["action"] == "finish":
            answer = str(action.get("answer", "")).strip()
            break
        started = time.perf_counter()
        normalized = "test" if action["action"] == "test" else action["action"]
        try:
            _, observation = tool(action, workspace)
            status = "ok"
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            observation = f"tool error: {type(error).__name__}: {error}"
            status = "error"
        calls.append(
            {
                "name": normalized,
                "raw_name": action["action"],
                "arguments": {key: value for key, value in action.items() if key not in {"action", "content", "new"}},
                "status": status,
                "duration_ms": round((time.perf_counter() - started) * 1000, 3),
                "result": observation[-2000:],
            }
        )
        messages.append({"role": "assistant", "content": json.dumps(action, ensure_ascii=False)})
        remaining = max_turns - len(calls)
        messages.append({"role": "user", "content": f"Observation:\n{observation[-3000:]}\n\nTool actions remaining: {remaining}. Finish now if the task is answerable; do not repeat prior reads."})
    else:
        messages.append({"role": "user", "content": "Tool budget is exhausted. Return exactly a finish JSON action now using the evidence collected. Do not request another tool."})
        try:
            response = request_json(
                f"{endpoint}/api/chat",
                {
                    "model": model,
                    "messages": messages,
                    "stream": False,
                    "format": "json",
                    "think": False,
                    "keep_alive": "5m",
                    "options": {"temperature": 0, "seed": 42, "num_ctx": 8192, "num_predict": 512},
                },
            )
            usage["input_tokens"] += int(response.get("prompt_eval_count", 0))
            usage["output_tokens"] += int(response.get("eval_count", 0))
            metrics["load_ms"] += float(response.get("load_duration", 0)) / 1_000_000
            metrics["prompt_eval_ms"] += float(response.get("prompt_eval_duration", 0)) / 1_000_000
            metrics["eval_ms"] += float(response.get("eval_duration", 0)) / 1_000_000
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
            "adapter": "ollama_repo_agent_v1",
            "temperature": 0,
            "seed": 42,
            "num_ctx": 8192,
            "num_predict": 512,
            "max_turns": max_turns,
            "cache_token_metrics": "not_reported_by_ollama",
        },
    }
    result_path.parent.mkdir(parents=True, exist_ok=True)
    result_path.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(answer)
    return 1 if failure else 0


if __name__ == "__main__":
    raise SystemExit(main())
