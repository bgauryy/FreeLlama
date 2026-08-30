#!/usr/bin/env python3
"""Full end-to-end acceptance suite for the benchmark skill."""

from __future__ import annotations

from datetime import date, timedelta
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote, unquote

from _schema import require_valid


def run(command: list[str], expected: int = 0, timeout: int = 300) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(command, text=True, capture_output=True, timeout=timeout, check=False)
    if result.returncode != expected:
        raise RuntimeError(f"command failed ({result.returncode}, expected {expected}): {command}\n{result.stdout}\n{result.stderr}")
    return result


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    scripts, schemas = root / "scripts", root / "schemas"
    suite, fixture = root / "tasks/suite.json", root / "fixtures/atlas"
    suite_value = json.loads(suite.read_text(encoding="utf-8"))
    require_valid(suite_value, json.loads((schemas / "suite.schema.json").read_text(encoding="utf-8")), "public suite")
    run([sys.executable, str(scripts / "validate.py"), "--suite", str(suite)])
    with tempfile.TemporaryDirectory(prefix="freellama-bench-selftest-") as temporary:
        temp = Path(temporary)

        private_suite = temp / "private" / "suite.json"
        run([sys.executable, str(scripts / "build_private_suite.py"), "--suite", str(suite), "--output", str(private_suite), "--seed", "self-test-seed"])
        private_value = json.loads(private_suite.read_text(encoding="utf-8"))
        require_valid(private_value, json.loads((schemas / "suite.schema.json").read_text(encoding="utf-8")), "private suite")
        assert private_value["visibility"] == "private_held_out" and private_value["variant_seed_hash"]
        run([sys.executable, str(scripts / "validate.py"), "--suite", str(private_suite)])
        reference_command = f"{sys.executable} {scripts / 'reference_agent.py'}"
        private_results = temp / "private-results"
        private_command = [sys.executable, str(scripts / "run.py"), "--suite", str(private_suite), "--model", "private-reference", "--agent", "golden-reference", "--agent-command", reference_command, "--capability", "filesystem", "--capability", "shell", "--capability", "tools", "--capability", "skills", "--capability", "mcp", "--trials", "1", "--results", str(private_results)]
        run(private_command, timeout=600)
        private_aggregate = private_results / "aggregate.json"
        run([sys.executable, str(scripts / "aggregate.py"), "--results", str(private_results), "--output", str(private_aggregate)])
        assert json.loads(private_aggregate.read_text(encoding="utf-8"))["models"][0]["deterministic_pass_rate"] == 1.0

        labels = temp / "judge-labels.json"
        labels.write_text(json.dumps({"cases": [{"id": f"c{index}", "human_winner": "A" if index % 2 else "B", "judge_winner": "A" if index % 2 else "B", "swapped_judge_winner": "A" if index % 2 else "B", "human_score": index % 6, "judge_score": index % 6} for index in range(20)]}), encoding="utf-8")
        calibration = temp / "calibration.json"
        run([sys.executable, str(scripts / "calibrate_judge.py"), "--input", str(labels), "--judge-model", "judge-fixture", "--output", str(calibration)])
        run([sys.executable, str(scripts / "validate.py"), "--suite", str(suite), "--calibration", str(calibration)])

        stale_suite = temp / "stale-suite.json"
        stale = dict(suite_value); stale["last_reviewed_at"] = (date.today() - timedelta(days=10)).isoformat(); stale["review_due_at"] = (date.today() - timedelta(days=1)).isoformat(); stale["fixture"] = str(fixture)
        stale_suite.write_text(json.dumps(stale), encoding="utf-8")
        run([sys.executable, str(scripts / "validate.py"), "--suite", str(stale_suite)], expected=1)
        run([sys.executable, str(scripts / "validate.py"), "--suite", str(stale_suite), "--allow-stale-suite"])

        results = temp / "results"
        base_command = [sys.executable, str(scripts / "run.py"), "--suite", str(suite), "--agent", "golden-reference", "--agent-command", reference_command, "--capability", "filesystem", "--capability", "shell", "--capability", "tools", "--capability", "skills", "--capability", "mcp", "--results", str(results)]
        run(base_command + ["--model", "reference-a", "--trials", "3"], timeout=600)
        run(base_command + ["--model", "reference-b", "--trials", "1"], timeout=600)

        aggregate, dashboard = results / "aggregate.json", results / "index.html"
        run([sys.executable, str(scripts / "aggregate.py"), "--results", str(results), "--output", str(aggregate)])
        run([sys.executable, str(scripts / "render_html.py"), "--aggregate", str(aggregate), "--output", str(dashboard)])
        run([sys.executable, str(scripts / "validate.py"), "--suite", str(suite), "--results", str(results)])
        value = json.loads(aggregate.read_text(encoding="utf-8"))
        by_model = {model["id"]: model for model in value["models"]}
        assert by_model["reference-a"]["deterministic_pass_rate"] == 1.0
        assert by_model["reference-a"]["pass_power_3"] == 1.0
        assert by_model["reference-a"]["judge"] == {"models": [], "scored_trials": 0, "calibrated": False}
        assert len(value["common_tasks"]) == 20 and len(value["pairwise_comparisons"]) == 1
        assert value["pairwise_comparisons"][0]["right_over_left_time_geomean"] is not None
        page = dashboard.read_text(encoding="utf-8")
        assert "Common-task comparison" in page and "Paired geometric comparisons" in page and "Review due" in page
        first_raw = Path(by_model["reference-a"]["tasks"][0]["raw_trials"][0])
        raw_href = quote(Path(os.path.relpath(first_raw, dashboard.parent.resolve())).as_posix(), safe="/")
        assert f'href="{raw_href}"' in page, raw_href
        assert (dashboard.parent / unquote(raw_href)).exists(), raw_href

        invalid_results = temp / "invalid-results"
        invalid_results.mkdir()
        sample = json.loads(next(results.rglob("trial-1.json")).read_text(encoding="utf-8")); sample.pop("benchmark_date")
        (invalid_results / "trial-invalid.json").write_text(json.dumps(sample), encoding="utf-8")
        run([sys.executable, str(scripts / "validate.py"), "--suite", str(suite), "--results", str(invalid_results)], expected=1)

        timeout_suite = temp / "timeout-suite.json"
        timeout_value = json.loads(suite.read_text(encoding="utf-8")); timeout_value["fixture"] = str(fixture); timeout_value["tasks"][0]["timeout_seconds"] = 1
        timeout_suite.write_text(json.dumps(timeout_value), encoding="utf-8")
        timeout_results = temp / "timeout-results"
        run([sys.executable, str(scripts / "run.py"), "--suite", str(timeout_suite), "--model", "timeout-fixture", "--agent-command", f"{sys.executable} {scripts / 'timeout_agent.py'}", "--task", "Q01", "--trials", "1", "--results", str(timeout_results)], expected=2)
        timeout_trial = json.loads(next(timeout_results.rglob("trial-1.json")).read_text(encoding="utf-8"))
        assert timeout_trial["status"] == "timeout" and timeout_trial["timing"]["peak_rss_kb"] is not None
        child_pid = int((Path(timeout_trial["artifacts"]["workspace"]) / "child.pid").read_text(encoding="utf-8"))
        time.sleep(0.1)
        try:
            os.kill(child_pid, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError(f"timed-out descendant still alive: {child_pid}")

        red = temp / "red"; shutil.copytree(fixture, red)
        run([sys.executable, str(scripts / "task_verifier.py"), "--task", "Q04", "--workspace", str(red)], expected=1)
        math_file = red / "src/atlas/math_utils.py"; math_file.write_text(math_file.read_text(encoding="utf-8").replace("range(start, end)", "range(start, end + 1)"), encoding="utf-8")
        run([sys.executable, str(scripts / "task_verifier.py"), "--task", "Q04", "--workspace", str(red)])
    print(json.dumps({"self_test": "passed", "checks": ["schema enforcement", "review dates", "private 20-task execution", "judge calibration", "public 20-task golden x3", "common-task pairs", "geometric ratios", "HTML", "negative schema", "process-tree timeout", "red-green verifier"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
