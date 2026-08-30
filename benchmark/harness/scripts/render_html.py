#!/usr/bin/env python3
"""Render a self-contained benchmark dashboard from aggregate JSON."""

from __future__ import annotations

import argparse
import html
import json
import os
from pathlib import Path
from urllib.parse import quote

from _common import load_json
from _schema import require_valid


def percent(value: object) -> str:
    return "—" if not isinstance(value, (int, float)) else f"{100 * value:.1f}%"


def number(value: object, digits: int = 1) -> str:
    return "—" if not isinstance(value, (int, float)) else f"{value:,.{digits}f}"


def model_rows(models: list[dict]) -> str:
    rows = []
    for model in sorted(models, key=lambda item: (item["deterministic_pass_rate"] is not None, item["deterministic_pass_rate"] or -1), reverse=True):
        cache = model["usage"].get("cache_hit_ratio")
        judge = "calibrated" if model["judge"]["calibrated"] else ("advisory" if model["judge"]["scored_trials"] else "not run")
        rows.append(f"""<tr>
<td><strong>{html.escape(model['id'])}</strong><small>run {html.escape(model.get('run_id') or '')}</small></td><td>{html.escape(model.get('agent') or '')}</td><td>{model.get('trial_budget', 0)}</td>
<td>{model['coverage']['applicable_tasks']}/{model['coverage']['total_tasks']}<small>missing: {html.escape(', '.join(model.get('missing_capability_tasks', [])) or 'none')}</small></td><td>{percent(model['deterministic_pass_rate'])}</td>
<td>{percent(model['pass_at_1'])}</td><td>{percent(model['pass_power_3'])}</td><td>{number(model.get('efficiency_score'))}</td>
<td>{number(model['timing'].get('successful_tasks_per_hour'))}</td><td>{number(model['timing'].get('median_wall_ms'))}</td>
<td>{number(model['usage'].get('input_tokens'), 0)}</td><td>{number(model['usage'].get('output_tokens'), 0)}</td><td>{percent(cache)}</td>
<td>{model['usage'].get('tool_calls', 0)}</td><td>{model['usage'].get('failed_tool_calls', 0)}</td><td>{html.escape(', '.join(model['judge'].get('models', [])) or 'none')}<small>{html.escape(judge)}</small></td>
</tr>""")
    return "\n".join(rows)


def raw_trial_href(raw_path: str, output_dir: Path) -> str:
    source = Path(raw_path)
    if not source.is_absolute():
        source = (Path.cwd() / source).resolve()
    relative = Path(os.path.relpath(source, output_dir.resolve())).as_posix()
    return quote(relative, safe="/")


def task_sections(models: list[dict], output_dir: Path) -> str:
    sections = []
    for model in models:
        rows = []
        for task in model["tasks"]:
            if not task["applicable"]:
                state, badge = "N/A", "na"
            elif task["pass_rate"] == 1:
                state, badge = "PASS", "pass"
            elif task["pass_rate"] == 0:
                state, badge = "FAIL", "fail"
            else:
                state, badge = percent(task["pass_rate"]), "mixed"
            raw_links = " ".join(
                f'<a href="{html.escape(raw_trial_href(path, output_dir))}">t{index}</a>'
                for index, path in enumerate(task["raw_trials"], 1)
            )
            rows.append(f"""<tr><td>{task['id']}</td><td>{html.escape(task['title'])}</td><td>{html.escape(task['tier'])}</td><td>{html.escape(task['category'])}</td><td><span class="badge {badge}">{state}</span></td><td>{number(task['deterministic_score'])}</td><td>{number(task['judge_score'])}</td><td>{number(task['efficiency_score'])}</td><td>{number(task['median_wall_ms'])}</td><td>{task['failed_tool_calls']}/{task['retries']}</td><td>{raw_links}</td></tr>""")
        sections.append(f"""<details><summary>{html.escape(model['id'])} · task detail</summary><div class="table-wrap"><table><thead><tr><th>ID</th><th>Task</th><th>Tier</th><th>Category</th><th>Result</th><th>Outcome</th><th>Judge</th><th>Efficiency</th><th>Median ms</th><th>Failures/retries</th><th>Raw</th></tr></thead><tbody>{''.join(rows)}</tbody></table></div></details>""")
    return "\n".join(sections)


def common_rows(models: list[dict]) -> str:
    return "".join(f"<tr><td><strong>{html.escape(model['id'])}</strong></td><td>{model['common_task_metrics']['task_count']}</td><td>{percent(model['common_task_metrics']['pass_at_1'])}</td><td>{percent(model['common_task_metrics']['pass_power_3'])}</td><td>{number(model['common_task_metrics']['deterministic_score'])}</td><td>{number(model['common_task_metrics']['median_wall_ms'])}</td><td>{number(model['common_task_metrics']['median_context_chars'], 0)}</td></tr>" for model in models)


def pairwise_rows(comparisons: list[dict]) -> str:
    rows = []
    for item in comparisons:
        wins = item["correctness"]
        rows.append(f"<tr><td>{html.escape(item['left'])}</td><td>{html.escape(item['right'])}</td><td>{item['common_tasks']}</td><td>{wins['left_wins']} / {wins['ties']} / {wins['right_wins']}</td><td>{number(item['right_over_left_time_geomean'], 2)}</td><td>{number(item['right_over_left_context_geomean'], 2)}</td></tr>")
    return "".join(rows)


def question_sections(tasks: list[dict]) -> str:
    cards = []
    for task in tasks:
        requirements = ", ".join(task.get("requirements", [])) or "none"
        rubric = "".join(f"<li>{html.escape(item)}</li>" for item in task.get("rubric", []))
        cards.append(f"""<article class="question">
<div class="question-head"><span class="badge mixed">{html.escape(task['id'])}</span><strong>{html.escape(task['title'])}</strong></div>
<div class="question-meta">{html.escape(task['tier'])} · {html.escape(task['category'])} · {html.escape(requirements)} · {task['timeout_seconds']}s limit</div>
<p class="prompt">{html.escape(task['prompt'])}</p>
<details class="rubric"><summary>Scoring intent</summary><ul>{rubric}</ul></details>
</article>""")
    return "".join(cards)


def main() -> int:
    parser = argparse.ArgumentParser(description="Render a self-contained FreeLlama benchmark HTML dashboard.")
    parser.add_argument("--aggregate", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--suite", type=Path, help="Frozen suite used to show the exact benchmark questions and rubrics.")
    parser.add_argument("--model", action="append", default=[], help="Include only this model id; repeat to build a focused dashboard.")
    parser.add_argument("--title", default="FreeLlama agent benchmark", help="Dashboard heading.")
    args = parser.parse_args()
    aggregate = load_json(args.aggregate)
    require_valid(aggregate, load_json(Path(__file__).resolve().parent.parent / "schemas/aggregate.schema.json"), "aggregate")
    if args.model:
        requested = set(args.model)
        available = {model["id"] for model in aggregate["models"]}
        missing = sorted(requested - available)
        if missing:
            raise SystemExit(f"unknown model ids: {', '.join(missing)}")
        aggregate = {
            **aggregate,
            "models": [model for model in aggregate["models"] if model["id"] in requested],
            "pairwise_comparisons": [
                comparison
                for comparison in aggregate.get("pairwise_comparisons", [])
                if comparison["left"] in requested and comparison["right"] in requested
            ],
            "pareto": {
                key: [model for model in values if model in requested]
                for key, values in aggregate.get("pareto", {}).items()
            },
            "runs_discovered": {
                model: runs
                for model, runs in aggregate.get("runs_discovered", {}).items()
                if model in requested
            },
        }
        require_valid(aggregate, load_json(Path(__file__).resolve().parent.parent / "schemas/aggregate.schema.json"), "filtered aggregate")
    suite = None
    if args.suite:
        suite = load_json(args.suite)
        require_valid(suite, load_json(Path(__file__).resolve().parent.parent / "schemas/suite.schema.json"), "suite")
        if suite["suite_id"] != aggregate["suite"]["id"] or suite["suite_version"] != aggregate["suite"]["version"]:
            raise SystemExit("suite id/version does not match aggregate")
    models = aggregate["models"]
    total_tasks = max((model["coverage"]["total_tasks"] for model in models), default=0)
    pareto_data = aggregate.get("pareto", {})
    embedded = json.dumps(aggregate, ensure_ascii=False).replace("</", "<\\/")
    embedded_suite = json.dumps(suite, ensure_ascii=False).replace("</", "<\\/") if suite else ""
    questions = question_sections(suite["tasks"]) if suite else '<p class="lede">Pass <code>--suite</code> to include the exact questions.</p>'
    page = f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>FreeLlama benchmark · {html.escape(aggregate['suite']['version'])}</title>
<style>
:root{{--bg:#0b1020;--panel:#141b2d;--line:#2a3550;--text:#e8eefc;--muted:#9dadca;--good:#2dd4a0;--bad:#fb7185;--warn:#fbbf24;--accent:#7dd3fc}}*{{box-sizing:border-box}}body{{margin:0;background:linear-gradient(145deg,#08101f,#11182a 50%,#0d1727);color:var(--text);font:14px/1.45 ui-sans-serif,system-ui,sans-serif}}main{{max-width:1500px;margin:auto;padding:32px 24px 72px}}h1{{font-size:clamp(28px,4vw,48px);margin:0 0 6px;letter-spacing:-.04em}}.lede{{color:var(--muted);max-width:900px}}.meta{{display:flex;gap:10px;flex-wrap:wrap;margin:20px 0}}.chip{{background:#18233b;border:1px solid var(--line);border-radius:999px;padding:7px 11px}}.cards{{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:12px;margin:24px 0}}.card,details{{background:rgba(20,27,45,.92);border:1px solid var(--line);border-radius:14px;box-shadow:0 16px 50px #0003}}.card{{padding:17px}}.card strong{{display:block;font-size:25px}}.card span,small{{display:block;color:var(--muted);font-size:12px}}h2{{margin-top:34px}}.table-wrap{{overflow:auto}}table{{width:100%;border-collapse:collapse;background:rgba(20,27,45,.86);border:1px solid var(--line)}}th,td{{padding:10px 12px;text-align:left;border-bottom:1px solid var(--line);white-space:nowrap}}th{{position:sticky;top:0;background:#18233b;color:#b9c8e6;font-size:11px;text-transform:uppercase;letter-spacing:.06em}}tr:hover td{{background:#172139}}.badge{{padding:3px 8px;border-radius:999px;font-weight:700;font-size:11px}}.pass{{color:var(--good);background:#0d3c35}}.fail{{color:var(--bad);background:#481a29}}.mixed{{color:var(--warn);background:#463613}}.na{{color:var(--muted);background:#273149}}details{{margin:12px 0;overflow:hidden}}summary{{cursor:pointer;padding:15px 17px;font-weight:700}}a{{color:var(--accent)}}.note{{border-left:3px solid var(--warn);padding:10px 14px;background:#342a13;color:#fde7a6;border-radius:4px}}.question-grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(360px,1fr));gap:12px}}.question{{background:rgba(20,27,45,.92);border:1px solid var(--line);border-radius:14px;padding:17px;box-shadow:0 16px 50px #0003}}.question-head{{display:flex;align-items:center;gap:10px}}.question-meta{{color:var(--muted);font-size:12px;margin-top:8px}}.prompt{{font-size:15px;line-height:1.6;margin:14px 0;white-space:pre-wrap}}.rubric{{box-shadow:none;margin:0;background:#10182a}}.rubric summary{{padding:9px 11px}}.rubric ul{{margin:0;padding:0 30px 13px}}footer{{color:var(--muted);margin-top:30px}}
</style></head><body><main>
<h1>{html.escape(args.title)}</h1><p class="lede">Quality-gated comparison of local model-agent systems across task understanding, repository orientation, reasoning, bug fixing, tool use, efficiency, and end-to-end execution.</p>
<div class="meta"><span class="chip">Suite {html.escape(aggregate['suite']['id'])}@{html.escape(aggregate['suite']['version'])}</span><span class="chip">{html.escape(str(aggregate['suite'].get('visibility') or 'unknown'))}</span><span class="chip">Benchmark date {html.escape(str(aggregate['suite'].get('benchmark_date') or 'unknown'))}</span><span class="chip">Review due {html.escape(str((aggregate['suite'].get('review') or {}).get('review_due_at') or 'unknown'))}</span><span class="chip">Runner {html.escape(str(aggregate['methodology'].get('runner_version') or 'unknown'))}</span><span class="chip">Generated {html.escape(aggregate['generated_at'])}</span><span class="chip">{len(models)} model(s)</span><span class="chip">{len(aggregate['common_tasks'])} common task(s)</span></div>
<p class="note">Deterministic outcomes are the promotion gate. Judge scores are advisory until calibrated; efficiency is awarded only to quality-passing work. Compare unlike capability sets through the common-task view.</p>
<div class="cards"><div class="card"><strong>{len(models)}</strong><span>systems compared</span></div><div class="card"><strong>{len(aggregate['common_tasks'])}/{total_tasks}</strong><span>common applicable tasks</span></div><div class="card"><strong>70 / 20 / 10</strong><span>outcome / judge / efficiency</span></div><div class="card"><strong>{aggregate['methodology'].get('trials_required_for_publishable_reliability', 3)}</strong><span>trials required for pass³</span></div></div>
<h2>Pareto reading</h2><div class="cards"><div class="card"><strong>{html.escape(', '.join(pareto_data.get('quality_leaders', [])) or '—')}</strong><span>quality leader(s)</span></div><div class="card"><strong>{html.escape(', '.join(pareto_data.get('efficiency_leaders', [])) or '—')}</strong><span>efficiency leader(s)</span></div><div class="card"><strong>{html.escape(', '.join(pareto_data.get('dominated', [])) or 'none')}</strong><span>dominated systems</span></div></div>
<h2>Model summary</h2><div class="table-wrap"><table><thead><tr><th>Model/run</th><th>Agent</th><th>Trials</th><th>Coverage</th><th>Trial pass</th><th>Pass@1</th><th>Pass³</th><th>Efficiency</th><th>Success/hour</th><th>Median ms</th><th>Input tok</th><th>Output tok</th><th>Cache hit</th><th>Tools</th><th>Tool failures</th><th>Judge</th></tr></thead><tbody>{model_rows(models)}</tbody></table></div>
<h2>Common-task comparison</h2><div class="table-wrap"><table><thead><tr><th>Model</th><th>Common tasks</th><th>Pass@1</th><th>Pass³</th><th>Outcome</th><th>Median ms</th><th>Context chars</th></tr></thead><tbody>{common_rows(models)}</tbody></table></div>
<h2>Paired geometric comparisons</h2><p class="lede">Ratios are right ÷ left. Above 1 means the left model used less time or context. Correctness is left wins / ties / right wins and always decides first.</p><div class="table-wrap"><table><thead><tr><th>Left</th><th>Right</th><th>N</th><th>Correctness W/T/L</th><th>Time ratio</th><th>Context ratio</th></tr></thead><tbody>{pairwise_rows(aggregate.get('pairwise_comparisons', []))}</tbody></table></div>
<h2>Questions and tasks</h2><p class="lede">These are the exact prompts presented to every model. Expand scoring intent to see the human-readable rubric; deterministic checks remain authoritative.</p><div class="question-grid">{questions}</div>
<h2>Task evidence by model</h2>{task_sections(models, args.output.parent)}
<footer>Raw trials remain the source of truth. Rebuild this page with <code>scripts/render_html.py</code>; do not edit it by hand.</footer>
<script type="application/json" id="benchmark-data">{embedded}</script>
<script type="application/json" id="benchmark-suite">{embedded_suite}</script>
</main></body></html>"""
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(page, encoding="utf-8")
    print(json.dumps({"output": str(args.output), "models": len(models)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
