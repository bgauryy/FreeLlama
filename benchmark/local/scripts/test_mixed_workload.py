#!/usr/bin/env python3
"""Fixture and mock-server contracts; no Ollama or live inference required."""
import argparse
import copy
import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

import mixed_workload as benchmark


def health(holding=False):
    return {"status": "ok", "admission": {"resources": {"holding": holding, "observation": {
        "available_memory_bytes": 16 * 1024**3, "swap_out_pages": 10, "load_average_one_minute": .5,
        "logical_cpus": 8, "thermal_throttled": False, "memory_pressure": "normal"}}}}


MODEL = {"name": "fixture:exact", "digest": "sha256:fixture", "resident": False, "execution": {"placement": "gpu"}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send_json(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        if self.path.endswith("/health"):
            self.server.health_calls += 1
            sample = health(self.server.hold_after is not None and self.server.health_calls >= self.server.hold_after)
            sample["admission"]["resources"]["observation"].update(self.server.observation)
            self.send_json(200, sample)
        else:
            self.send_json(200, {"models": [self.server.model]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(body)
        time.sleep(self.server.delay)
        if self.server.refuse:
            self.send_json(503, self.server.refusal_body)
            return
        text = " ".join(message["content"] for message in body["messages"])
        answer = "FL-7319" if "CURRENT" in text else "FL-2086" if "FL-2086" in text else '{"count":7,"owner":"Rina"}' if "owner=Rina" in text else "review"
        self.send_json(200, {"route": {"selected_model": MODEL["name"]},
                            "execution": {"model_digest": MODEL["digest"], "lifecycle": {"unload_requested": True}, "observation": {"status": "verified"}},
                            "response": {"done": self.server.done, "message": {"content": answer}}})


class BenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.server.requests = []
        self.server.health_calls = 0
        self.server.hold_after = None
        self.server.delay = .01
        self.server.refuse = False
        self.server.refusal_body = {"error": "queue admission refused"}
        self.server.done = True
        self.server.observation = {}
        self.server.model = copy.deepcopy(MODEL)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def run_probe(self, **kwargs):
        args = argparse.Namespace(endpoint=f"http://127.0.0.1:{self.server.server_port}", model=MODEL["name"], cpu_model=None,
                                  trials=3, max_wall_seconds=5, request_timeout_seconds=1, max_output_tokens=96,
                                  max_parallel=2, include_compacted=False, smoke=False, context_tokens=4096)
        for key, value in kwargs.items():
            setattr(args, key, value)
        return benchmark.run(args)

    def test_complete_mock_run_and_fixed_payload(self):
        report = self.run_probe(include_compacted=True)
        self.assertTrue(report["complete"])
        self.assertEqual(report["summary"]["correct"], 15)
        self.assertEqual(len(self.server.requests), 15)
        for request in self.server.requests:
            self.assertEqual(request["keep_alive"], "0")
            self.assertEqual(request["model"], MODEL["name"])
            self.assertEqual(request["objective"], "fastest")
            self.assertEqual(request["context_tokens"], 4096)
            self.assertEqual(request["request_options"]["options"]["num_predict"], 96)
        self.assertTrue(any(t["context"]["precompacted"] for t in report["tasks"]))
        self.assertEqual(benchmark.compare(report, report)["verdict"], "NO_MEASURED_IMPROVEMENT")

    def test_refusals_reduce_primary_and_correct_fraction(self):
        self.server.refuse = True
        report = self.run_probe()
        self.assertEqual(report["summary"]["correct_fraction"], 0)
        self.assertEqual(report["summary"]["successful_correct_tasks_per_second"], 0)
        self.assertEqual(report["summary"]["status_counts"], {"admission_refusal": 12})
        self.assertEqual(benchmark.compare(report, report)["verdict"], "INSUFFICIENT_EVIDENCE")

    def test_resource_refusal_stops_dispatch_even_with_healthy_cached_sample(self):
        self.server.refuse = True
        self.server.refusal_body = {"error": "resource unavailable", "code": "resource_admission_unavailable", "resource_admission": {"status": "held"}}
        report = self.run_probe()
        self.assertFalse(report["complete"])
        self.assertLessEqual(len(self.server.requests), 2)
        self.assertIn("resource_admission_unavailable", report["stop_reasons"])
        self.assertGreater(report["summary"]["status_counts"].get("admission_refusal", 0), 0)
        self.assertEqual(report["summary"]["correct_fraction"], 0)

    def test_incomplete_response_stops_new_dispatch(self):
        self.server.done = False
        report = self.run_probe()
        self.assertFalse(report["complete"])
        self.assertLessEqual(len(self.server.requests), 2)
        self.assertIn("request_completion_unknown", report["stop_reasons"])
        self.assertGreater(report["summary"]["status_counts"].get("incomplete_response", 0), 0)

    def test_context_option_is_recorded_sent_and_comparison_frozen(self):
        baseline = self.run_probe(context_tokens=8192, include_compacted=True)
        self.assertEqual(baseline["workload_config"]["context_tokens"], 8192)
        self.assertTrue(all(request["context_tokens"] == 8192 for request in self.server.requests))
        candidate = copy.deepcopy(baseline)
        candidate["workload_config"]["context_tokens"] = 4096
        self.assertEqual(benchmark.compare(baseline, candidate)["verdict"], "NOT_COMPARABLE")

    def test_context_option_rejects_out_of_range_without_requests(self):
        for value in ("2047", "32769"):
            with self.assertRaises(SystemExit) as raised, patch("sys.stderr"):
                benchmark.main(["run", "--endpoint", f"http://127.0.0.1:{self.server.server_port}", "--model", MODEL["name"], "--context-tokens", value, "--output", ".octocode/context-validation-never-written.json"])
            self.assertEqual(raised.exception.code, 2)
        self.assertEqual(self.server.requests, [])

    def test_holding_prevents_all_dispatch(self):
        self.server.hold_after = 1
        report = self.run_probe()
        self.assertFalse(report["complete"])
        self.assertEqual(self.server.requests, [])
        self.assertIn("host_holding", report["stop_reasons"])
        self.assertEqual(report["summary"]["correct_fraction"], 0)

    def test_pressure_after_catalog_prevents_dispatch(self):
        self.server.hold_after = 2
        report = self.run_probe()
        self.assertFalse(report["complete"])
        self.assertEqual(self.server.requests, [])

    def test_wall_deadline_is_absolute(self):
        self.server.delay = .5
        start = time.monotonic()
        report = self.run_probe(max_wall_seconds=.15, request_timeout_seconds=1)
        self.assertLess(time.monotonic() - start, .35)
        self.assertIn("wall_deadline", report["stop_reasons"])
        self.assertLessEqual(len(self.server.requests), 2)
        self.assertFalse(report["complete"])

    def test_request_deadline_stops_new_work(self):
        self.server.delay = .5
        report = self.run_probe(request_timeout_seconds=.08)
        # A transport timeout returned by urllib also stops dispatch, even if it
        # arrives just before the controller's absolute deadline.
        self.assertLessEqual(len(self.server.requests), 2)
        self.assertFalse(report["complete"])

    def test_unknown_thermal_is_honest_and_blocks_comparison(self):
        self.server.observation["thermal_throttled"] = None
        report = self.run_probe()
        self.assertTrue(report["complete"])
        self.assertIn("thermal_throttled", report["summary"]["unknown_metrics"])
        self.assertEqual(benchmark.compare(report, report)["verdict"], "INSUFFICIENT_EVIDENCE")

    def test_missing_or_zero_cpu_denominator_cannot_hide_high_load(self):
        baseline = self.run_probe()
        for cpus in (None, 0):
            candidate = copy.deepcopy(baseline)
            candidate["elapsed_seconds"] *= .8
            for sample in candidate["health_samples"]:
                sample["body"]["admission"]["resources"]["observation"].update(logical_cpus=cpus, load_average_one_minute=100)
            candidate["summary"] = benchmark.summarize(candidate)
            self.assertIn("logical_cpus", candidate["summary"]["unknown_metrics"])
            self.assertEqual(benchmark.compare(baseline, candidate)["verdict"], "INSUFFICIENT_EVIDENCE")

    def test_preexisting_resident_model_is_not_unloaded(self):
        self.server.model["resident"] = True
        report = self.run_probe()
        self.assertFalse(report["complete"])
        self.assertEqual(self.server.requests, [])

    def test_cpu_requires_existing_assignment(self):
        report = self.run_probe(cpu_model=MODEL["name"])
        self.assertFalse(report["complete"])
        self.assertEqual(self.server.requests, [])

    def test_exact_model_and_digest_required(self):
        for change in ({"model": "fixture"}, {}):
            if not change:
                self.server.model["digest"] = None
            report = self.run_probe(**change)
            self.assertFalse(report["complete"])
        self.assertEqual(self.server.requests, [])

    def test_comparison_guards_smoke_partial_forged_and_changed_budget(self):
        baseline = self.run_probe()
        for mutate in (
            lambda r: r["workload_config"].update(smoke=True),
            lambda r: r["tasks"].pop(),
            lambda r: r["tasks"][0].update(correct=False),
            lambda r: r["summary"].update(correct=900),
            lambda r: r.update(case_hash="wrong"),
            lambda r: r["models"]["primary"].update(digest=None),
        ):
            candidate = copy.deepcopy(baseline)
            mutate(candidate)
            self.assertEqual(benchmark.compare(baseline, candidate)["verdict"], "INSUFFICIENT_EVIDENCE")
        candidate = copy.deepcopy(baseline)
        candidate["workload_config"]["max_wall_seconds"] = 6
        self.assertEqual(benchmark.compare(baseline, candidate)["verdict"], "NOT_COMPARABLE")

    def test_comparable_improvement_is_never_accept(self):
        baseline = self.run_probe()
        candidate = copy.deepcopy(baseline)
        candidate["elapsed_seconds"] *= .8
        candidate["summary"] = benchmark.summarize(candidate)
        result = benchmark.compare(baseline, candidate)
        self.assertEqual(result["verdict"], "MEASURED_IMPROVEMENT")
        self.assertIn("No ACCEPT", result["promotion"])

    def test_grader_rejects_wrong_types_and_extra_fields(self):
        case = benchmark.load_suite()["cases"][2]
        self.assertTrue(benchmark.grade(case, '{"count":7,"owner":"Rina"}'))
        for text in ('{"count":"7","owner":"Rina"}', '{"count":7,"owner":"Rina","extra":1}', '```json\n{}\n```', 'I cannot help'):
            self.assertFalse(benchmark.grade(case, text))

    def test_malformed_response_and_health(self):
        case = benchmark.load_suite()["cases"][0]
        for body in (None, [], {"response": []}, {"response": {}, "execution": {}, "route": []}):
            result = benchmark.classify(case, {"http_status": 200, "body": body}, MODEL)
            self.assertEqual(result["status"], "malformed_metadata")
        self.server.observation["available_memory_bytes"] = "lots"
        report = self.run_probe()
        self.assertIn("health_metadata_invalid", report["stop_reasons"])
        self.assertEqual(self.server.requests, [])

    def test_pressure_thresholds(self):
        guards = json.loads(benchmark.KPI.read_text())["guardrails"]
        for patch_values, reason in (({"swap_out_pages": 11}, "swap_out_growth"), ({"available_memory_bytes": 1}, "available_memory_below_guard"), ({"load_average_one_minute": 100}, "load_above_guard"), ({"thermal_throttled": True}, "thermal_throttling")):
            body = health()
            body["admission"]["resources"]["observation"].update(patch_values)
            self.assertIn(reason, benchmark.pressure_reasons({"http_status": 200, "body": body}, 10, guards))

    def test_unavailable_required_memory_prevents_dispatch(self):
        self.server.observation["available_memory_bytes"] = None
        report = self.run_probe()
        self.assertIn("required_available_memory_unavailable", report["stop_reasons"])
        self.assertEqual(self.server.requests, [])


if __name__ == "__main__":
    unittest.main()
