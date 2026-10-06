#!/usr/bin/env python3
"""Generate a `freellama serve` routing policy from a harness quality aggregate.

`route_evidence` returns `medium` confidence only when a task has both a configured policy and
benchmark data. This script writes that policy from a harness `aggregate.json`, which carries
`pass_at_1`, a correctness measure. It deliberately does not read `bench-all` output: throughput
is not quality evidence.

The caller names the task the suite measures; the script never infers it.

Aggregate model ids are lossy slugs (`:` replaced with `-`), so qualification matches forward from
the tags installed on the Ollama endpoint (`/api/tags`).

Usage:
  python3 benchmark/harness/scripts/policy_from_eval.py \
    --aggregate results/aggregate.json --task coding [--min-pass 0.8] [--out platform.toml] \
    [--allow-smoke] [--ollama-endpoint http://127.0.0.1:11434]
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

# Minimum trials before a run is evidence rather than a smoke check (the harness's own rule).
PUBLISHABLE_TRIALS = 3
DEFAULT_OLLAMA_ENDPOINT = "http://127.0.0.1:11434"
CONTROL_TIMEOUT_SECONDS = 10
# Router task kinds (serde snake_case wire names).
TASKS = ("completion", "coding", "code_repair", "tools", "browser", "vision", "embedding", "long_context")


class PolicyError(Exception):
    """The aggregate cannot honestly back a quality contract."""


@dataclass(frozen=True)
class Qualified:
    model: str
    pass_at_1: float
    agent: str | None
    trials: int


def slug(model: str) -> str:
    """The harness run-id slug for a model tag (`qwen3.8:27b-mlx` -> `qwen3.8-27b-mlx`)."""
    return model.replace(":", "-")


def _require_str(value: Any, field: str) -> str:
    if not isinstance(value, str):
        raise PolicyError(f"aggregate field {field} must be a string")
    return value


def qualify(aggregate: dict[str, Any], installed: list[str], min_pass: float, allow_smoke: bool) -> tuple[list[Qualified], str]:
    """Return the installed models that cleared `min_pass`, best first, and the benchmark date."""
    suite = aggregate.get("suite")
    models = aggregate.get("models")
    if not isinstance(suite, dict) or not isinstance(models, list):
        raise PolicyError("aggregate must contain a `suite` object and a `models` list")
    suite_id = _require_str(suite.get("id"), "suite.id")
    benchmark_date = _require_str(suite.get("benchmark_date"), "suite.benchmark_date")
    review = suite.get("review") or {}
    if not review.get("fresh", False):
        raise PolicyError(
            f"aggregate {suite_id} is past its review window (review_due_at "
            f"{review.get('review_due_at')!r}); re-review the suite before generating a quality "
            "contract from it"
        )

    qualified: list[Qualified] = []
    for entry in models:
        entry_id = _require_str(entry.get("id"), "models[].id")
        passed = entry.get("pass_at_1")
        if passed is None or passed < min_pass:
            continue
        trials = int(entry.get("trial_budget") or 0)
        if trials < PUBLISHABLE_TRIALS and not allow_smoke:
            raise PolicyError(
                f"`{entry_id}` cleared {min_pass:.2f} with pass_at_1 {passed:.3f}, but the run used "
                f"{trials} trial(s). The harness treats fewer than {PUBLISHABLE_TRIALS} as a smoke "
                "result, not evidence. Re-run with more trials, or pass --allow-smoke to write a "
                "policy marked smoke-only."
            )
        # Match forward from installed tags; the aggregate id is a lossy slug.
        matching = [m for m in installed if entry_id == slug(m) or entry_id.startswith(f"{slug(m)}-")]
        if not matching:
            continue
        longest = max(len(slug(m)) for m in matching)
        finalists = [m for m in matching if len(slug(m)) == longest]
        if len(finalists) != 1:
            raise PolicyError(
                f"aggregate id `{entry_id}` ambiguously matches installed models {', '.join(finalists)}; "
                "their lossy slugs are indistinguishable, so refusing rather than crediting evidence "
                "to the wrong model"
            )
        qualified.append(Qualified(finalists[0], float(passed), entry.get("agent"), trials))

    # Best first (stable): the router prefers the leading candidate for objective=quality.
    qualified.sort(key=lambda q: q.pass_at_1, reverse=True)
    seen: set[str] = set()
    unique = []
    for q in qualified:
        if q.model not in seen:
            seen.add(q.model)
            unique.append(q)
    if not unique:
        raise PolicyError(
            f"no model in {suite_id} cleared pass_at_1 >= {min_pass:.2f} while also being installed "
            "here. Lower --min-pass, run the suite against an installed model, or install a "
            "qualifying one."
        )
    return unique, benchmark_date


def render_policy(entries: dict[str, list[Qualified]], source: str, benchmark_date: str, min_pass: float, smoke: bool) -> str:
    """Render a policy TOML file with its provenance written into comments."""
    out = ["schema_version = 1\n"]
    if smoke:
        out.append("smoke = true\n")
    out.append("\n")
    out.append("# GENERATED by benchmark/harness/scripts/policy_from_eval.py — do not hand-edit; regenerate.\n")
    out.append(f"# source        = {source}\n")
    out.append(f"# benchmarked   = {benchmark_date}\n")
    out.append(f"# threshold     = pass_at_1 >= {min_pass:.2f}\n")
    if smoke:
        out.append(
            "#\n# SMOKE-ONLY: generated from a run with fewer than three trials. The harness treats\n"
            "# that as a smoke result, not publishable evidence. Routes qualified by this file will\n"
            "# report medium confidence on thin evidence — re-run with more trials before relying\n"
            "# on it for anything quality-sensitive.\n"
        )
    out.append("\n")
    for task in sorted(entries, key=TASKS.index):
        models = entries[task]
        out.append(f"[policies.{task}]\n")
        out.append("qualified_models = [" + ", ".join(json.dumps(q.model) for q in models) + "]\n")
        for q in models:
            agent = f", agent {q.agent}" if q.agent else ""
            out.append(f"# {q.model} — pass_at_1 {q.pass_at_1:.3f} over {q.trials} trial(s){agent}\n")
        out.append("\n")
    return "".join(out)


def installed_models(endpoint: str) -> list[str]:
    """List installed Ollama tags from `<endpoint>/api/tags`."""
    url = endpoint.rstrip("/") + "/api/tags"
    with urllib.request.urlopen(url, timeout=CONTROL_TIMEOUT_SECONDS) as response:
        tags = json.load(response)
    return [model["name"] for model in tags["models"]]


def task_kind(value: str) -> str:
    task = value.replace("-", "_")
    if task not in TASKS:
        raise argparse.ArgumentTypeError(f"unknown task {value!r}; choose one of {', '.join(TASKS)}")
    return task


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--aggregate", required=True, type=Path, help="harness aggregate.json")
    parser.add_argument("--task", required=True, type=task_kind, help="the task this suite measures; never inferred")
    parser.add_argument("--min-pass", type=float, default=0.8, help="minimum pass_at_1 to qualify (default 0.8)")
    parser.add_argument("--out", type=Path, help="write here instead of stdout")
    parser.add_argument("--allow-smoke", action="store_true", help="accept fewer than three trials; marks the output smoke-only")
    parser.add_argument(
        "--ollama-endpoint",
        default=os.environ.get("FREELLAMA_OLLAMA_ENDPOINT") or DEFAULT_OLLAMA_ENDPOINT,
        help="endpoint used to list installed models (aggregate ids are lossy slugs)",
    )
    args = parser.parse_args(argv)
    try:
        try:
            aggregate = json.loads(args.aggregate.read_text(encoding="utf-8"))
        except ValueError as error:
            raise PolicyError(f"parse aggregate {args.aggregate}: {error}") from error
        qualified, benchmark_date = qualify(aggregate, installed_models(args.ollama_endpoint), args.min_pass, args.allow_smoke)
    except (OSError, ValueError, KeyError, PolicyError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    smoke = any(q.trials < PUBLISHABLE_TRIALS for q in qualified)
    rendered = render_policy({args.task: qualified}, str(args.aggregate), benchmark_date, args.min_pass, smoke)
    if args.out:
        args.out.write_text(rendered, encoding="utf-8")
        print(f"wrote {args.out}")
    else:
        sys.stdout.write(rendered)
    return 0


if __name__ == "__main__":
    sys.exit(main())
