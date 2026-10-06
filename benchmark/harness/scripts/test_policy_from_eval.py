#!/usr/bin/env python3
"""policy_from_eval must refuse to manufacture quality evidence it does not have."""

import io
import json
import tempfile
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

from policy_from_eval import PolicyError, main, qualify, render_policy, slug

try:
    from tomllib import loads as toml_loads
except ImportError:  # Python < 3.11: parse the flat subset render_policy emits.

    def toml_loads(text):
        root = {}
        table = root
        for raw in text.splitlines():
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            if line.startswith("["):
                table = root
                for part in line.strip("[]").split("."):
                    table = table.setdefault(part, {})
                continue
            key, value = (part.strip() for part in line.split("=", 1))
            table[key] = json.loads(value)
        return root

INSTALLED = ["qwen3.8:27b-mlx"]


def aggregate(trials: int, passed: float, fresh: bool) -> dict:
    return {
        "suite": {"id": "s", "benchmark_date": "2026-08-27", "review": {"fresh": fresh, "review_due_at": "2026-09-27"}},
        "models": [{"id": "qwen3.8-27b-mlx-bash", "agent": "bash", "pass_at_1": passed, "trial_budget": trials}],
    }


class QualifyTests(unittest.TestCase):
    def test_slug_matches_the_harness_convention(self):
        self.assertEqual(slug("qwen3.8:27b-mlx"), "qwen3.8-27b-mlx")

    def test_refuses_a_smoke_run_as_evidence(self):
        with self.assertRaisesRegex(PolicyError, "smoke result"):
            qualify(aggregate(1, 0.9, True), INSTALLED, 0.8, False)

    def test_accepts_a_smoke_run_only_when_asked(self):
        qualified, date = qualify(aggregate(1, 0.9, True), INSTALLED, 0.8, True)
        self.assertEqual([q.model for q in qualified], ["qwen3.8:27b-mlx"])
        self.assertEqual(date, "2026-08-27")

    def test_refuses_an_aggregate_past_its_review_window(self):
        with self.assertRaisesRegex(PolicyError, "review window"):
            qualify(aggregate(3, 0.9, False), INSTALLED, 0.8, False)

    def test_refuses_when_nothing_clears_the_bar(self):
        with self.assertRaisesRegex(PolicyError, "cleared"):
            qualify(aggregate(3, 0.5, True), INSTALLED, 0.8, False)

    def test_ignores_models_that_are_not_installed(self):
        with self.assertRaisesRegex(PolicyError, "installed here"):
            qualify(aggregate(3, 0.9, True), ["some-other:7b"], 0.8, False)

    def test_prefers_the_longest_installed_slug_and_refuses_ambiguity(self):
        qualified, _ = qualify(aggregate(3, 0.9, True), ["qwen3.8:27b", "qwen3.8:27b-mlx"], 0.8, False)
        self.assertEqual([q.model for q in qualified], ["qwen3.8:27b-mlx"])
        with self.assertRaisesRegex(PolicyError, "ambiguously"):
            qualify(aggregate(3, 0.9, True), ["qwen3.8:27b-mlx", "qwen3.8-27b:mlx"], 0.8, False)

    def test_ranks_best_first_and_deduplicates(self):
        data = aggregate(3, 0.85, True)
        data["models"].append({"id": "qwen3.8-27b-mlx-octocode", "agent": "octocode", "pass_at_1": 0.95, "trial_budget": 3})
        data["models"].append({"id": "gemma4-e4b", "pass_at_1": 0.9, "trial_budget": 3})
        qualified, _ = qualify(data, INSTALLED + ["gemma4:e4b"], 0.8, False)
        self.assertEqual([(q.model, q.agent) for q in qualified], [("qwen3.8:27b-mlx", "octocode"), ("gemma4:e4b", None)])

    def test_a_model_qualified_by_two_runs_appears_once_even_when_scores_are_not_adjacent(self):
        data = aggregate(3, 0.9, True)
        data["models"] = [
            {"id": "alpha-1b-agent-x", "pass_at_1": 0.95, "trial_budget": 3},
            {"id": "beta-1b", "pass_at_1": 0.90, "trial_budget": 3},
            {"id": "alpha-1b-agent-y", "pass_at_1": 0.85, "trial_budget": 3},
        ]
        qualified, _ = qualify(data, ["alpha:1b", "beta:1b"], 0.8, False)
        self.assertEqual([q.model for q in qualified], ["alpha:1b", "beta:1b"])
        self.assertEqual(qualified[0].pass_at_1, 0.95, "each model keeps its best score")


class RenderTests(unittest.TestCase):
    def test_render_policy_writes_a_machine_readable_smoke_flag(self):
        qualified, _ = qualify(aggregate(1, 0.9, True), INSTALLED, 0.8, True)
        rendered = render_policy({"coding": qualified}, "agg.json", "2026-08-27", 0.8, True)
        parsed = toml_loads(rendered)
        self.assertIs(parsed["smoke"], True, "runtime loader keys on this field, not the comment")
        self.assertEqual(parsed["schema_version"], 1)
        self.assertEqual(parsed["policies"]["coding"]["qualified_models"], ["qwen3.8:27b-mlx"])
        self.assertIn("# qwen3.8:27b-mlx — pass_at_1 0.900 over 1 trial(s), agent bash", rendered)

    def test_publishable_run_has_no_smoke_flag(self):
        qualified, _ = qualify(aggregate(3, 0.9, True), INSTALLED, 0.8, False)
        parsed = toml_loads(render_policy({"code_repair": qualified}, "agg.json", "2026-08-27", 0.8, False))
        self.assertNotIn("smoke", parsed)
        self.assertIn("code_repair", parsed["policies"])


class _Tags(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - http.server API
        body = json.dumps({"models": [{"name": name} for name in INSTALLED]}).encode()
        self.send_response(200 if self.path == "/api/tags" else 404)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


class MainTests(unittest.TestCase):
    def test_main_lists_installed_tags_and_writes_the_policy(self):
        server = HTTPServer(("127.0.0.1", 0), _Tags)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory, "aggregate.json")
            source.write_text(json.dumps(aggregate(3, 0.9, True)), encoding="utf-8")
            out = Path(directory, "platform.toml")
            with redirect_stdout(io.StringIO()):
                code = main(["--aggregate", str(source), "--task", "code-repair", "--out", str(out), "--ollama-endpoint", f"http://127.0.0.1:{server.server_port}"])
            self.assertEqual(code, 0)
            parsed = toml_loads(out.read_text(encoding="utf-8"))
        self.assertEqual(parsed["policies"]["code_repair"]["qualified_models"], INSTALLED)

    def test_a_malformed_aggregate_reports_a_parse_error_not_a_traceback(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory, "aggregate.json")
            source.write_text("{ not json at all", encoding="utf-8")
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                code = main(["--aggregate", str(source), "--task", "coding", "--ollama-endpoint", "http://127.0.0.1:9"])
        self.assertEqual(code, 1)
        self.assertIn("parse aggregate", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
