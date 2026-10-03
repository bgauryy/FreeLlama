"""Outcome contracts for bounded, quality-qualified hardware workload receipts."""

import json
import copy
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

import run_workload
from workload_contract import load_workload, new_report, summarize, write_report


def manifest():
    return {
        "schema_version": 1, "model": "gpu:fixture", "task": "coding",
        "trials": 3, "concurrency": 2, "num_predict": 256, "num_ctx": 4096,
        "request_timeout_seconds": 2, "run_budget_seconds": 10,
        "cases": [
            {"id": "sum", "prompt": "Calculate 17 + 28. Return the integer.", "expected_text": "45"},
            {"id": "product", "prompt": "Calculate 7 * 9. Return the integer.", "expected_text": "63"},
        ],
    }


def health():
    return {"status": "ok", "admission": {"resources": {"holding": False}}, "contracts": {
        "authentication": "optional_bearer_all_routes",
        "placement_feedback_persistence": "versioned_atomic_snapshot_v1",
        "placement_observation": "ollama_api_ps_after_execution",
    }, "feedback": {"persistence": {"enabled": True}}}


def hang_after_checkpoint(config, endpoint, token, checkpoint):
    report = new_report(config, endpoint)
    report["health"] = health()
    report["warmup"] = {"wall_seconds": 0.1, "cases": [{"id": "retained", "status": "error",
                       "error": {"type": "FixtureError", "message": "retained before hang"}}]}
    write_report(checkpoint, report)
    time.sleep(30)


def hang_in_transport(config, endpoint, token, checkpoint):
    def transport(url, body, auth, timeout):
        if body is None:
            return health()
        if body["prompt"].startswith("Calculate 17"):
            return {"route": {"selected_model": "gpu:fixture"},
                    "execution": {"observation": {"status": "verified", "processor": "gpu"}},
                    "admission": {"queue_wait_ms": 0},
                    "response": {"done": True, "message": {"content": "45"}}}
        time.sleep(30)
    with patch.object(run_workload, "call", side_effect=transport):
        run_workload.measure(config, endpoint, token, checkpoint)


class WorkloadReceiptTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def config(self, value=None):
        path = self.root / "workload.json"
        path.write_text(json.dumps(value or manifest()))
        return load_workload(path)

    def measure(self, fail_call=None, failure="quality", minimum_speedup=None, health_payload=None):
        value = manifest()
        if minimum_speedup is not None:
            value["minimum_speedup"] = minimum_speedup
        config = self.config(value)
        calls = []

        def transport(url, body, token, timeout):
            if body is None:
                return health_payload or health()
            calls.append(body)
            if len(calls) == fail_call and failure == "transport":
                raise OSError("connection fixture failed")
            answer = "45" if body["prompt"].startswith("Calculate 17") else "63"
            if len(calls) == fail_call:
                answer = "wrong"
            return {"route": {"selected_model": "gpu:fixture"},
                    "execution": {"observation": {"status": "verified", "processor": "gpu"}},
                    "admission": {"queue_wait_ms": 3, "total_wait_ms": 5},
                    "metrics": {"load_duration_ns": 1000000, "prompt_duration_ns": 2000000,
                                "output_duration_ns": 3000000, "output_tokens": 8,
                                "prompt_tokens": 10, "cached_prompt_tokens": 4},
                    "response": {"done": True, "message": {"content": answer}}}

        checkpoint = self.root / "receipt.json"
        with patch.object(run_workload, "call", side_effect=transport):
            run_workload.measure(config, "http://fixture", None, checkpoint)
        return json.loads(checkpoint.read_text()), calls

    def test_distinct_golden_workload_needs_no_embedding_model_or_gain_gate(self):
        report, calls = self.measure()
        self.assertEqual(report["verdict"], "accept")
        self.assertEqual(len(calls), 14)
        self.assertTrue(all(body["task"] == "coding" for body in calls))
        self.assertTrue(all(body["timeout_seconds"] == 2 for body in calls))
        self.assertTrue(all(body["context_tokens"] == 4096 for body in calls))
        self.assertEqual(report["completion"]["successful_requests"], 14)
        self.assertEqual(len(report["performance"]["trials"]), 3)
        self.assertIsNone(report["performance"]["minimum_speedup"])
        self.assertGreater(report["performance"]["parallel"]["qualified_requests_per_minute"], 0)

    def test_earlier_bad_golden_output_cannot_be_hidden_by_later_success(self):
        report, _ = self.measure(fail_call=3)
        self.assertEqual(report["verdict"], "reject")
        self.assertIsNone(report["performance"]["parallel"]["qualified_requests_per_minute"])
        self.assertTrue(any("trial1/sequential/sum: expected" in failure for failure in report["failures"]))

    def test_transport_failure_persists_partial_receipt_and_retains_prior_success(self):
        report, calls = self.measure(fail_call=3, failure="transport")
        self.assertEqual(report["verdict"], "reject")
        self.assertFalse(report["completion"]["complete"])
        self.assertEqual(len(calls), 3)
        self.assertEqual(report["warmup"]["cases"][0]["status"], "success")
        self.assertEqual(report["performance"]["trials"][0]["sequential"]["cases"][0]["error"]["type"], "OSError")
        self.assertIsNone(report["performance"]["speedup"])

    def test_reported_telemetry_has_counts_and_unknowns_stay_unknown(self):
        report, _ = self.measure()
        parallel = report["performance"]["parallel"]
        self.assertEqual(parallel["latency_seconds"]["sample_count"], 6)
        self.assertEqual(parallel["queue_wait_ms"]["p95"], 3)
        self.assertEqual(parallel["load_seconds"]["p95"], .001)
        self.assertEqual(parallel["cached_prompt_tokens"]["reported_total"], 24)
        self.assertIsNone(report["performance"]["cache_hit_rate"])
        self.assertEqual(report["performance"]["measured_hardware_utilization"], "unknown")

    def test_optional_gain_target_is_enforced_without_inventing_a_baseline(self):
        report, _ = self.measure(minimum_speedup=1000000)
        self.assertEqual(report["verdict"], "reject")
        self.assertTrue(any("speedup" in value for value in report["failures"]))

    def test_clone_or_missing_golden_is_rejected_before_http(self):
        for mutate, expected in (
                (lambda value: value["cases"][1].update(prompt=value["cases"][0]["prompt"]), "distinct"),
                (lambda value: value["cases"][0].pop("expected_text"), "expected_text"),
                (lambda value: value.update(num_predict=-1), "num_predict"),
                (lambda value: value.update(request_timeout_seconds=float("nan")), "request_timeout_seconds"),
                (lambda value: value.update(extra="typo"), "unknown")):
            value = manifest()
            mutate(value)
            with self.subTest(expected=expected), patch.object(run_workload, "call") as transport:
                with self.assertRaisesRegex(ValueError, expected):
                    self.config(value)
                transport.assert_not_called()

    def test_hung_transport_is_killed_at_whole_run_deadline_with_checkpoint_retained(self):
        config = self.config()
        config["run_budget_seconds"] = .3
        started = time.monotonic()
        report = run_workload.run_bounded(config, "http://fixture", None, self.root / "bounded.json",
                                          worker=hang_after_checkpoint)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(report["verdict"], "reject")
        self.assertFalse(report["completion"]["complete"])
        self.assertEqual(report["warmup"]["cases"][0]["id"], "retained")
        self.assertTrue(any("whole-run deadline" in failure for failure in report["failures"]))
        self.assertEqual(report["upstream_cancellation"], "unknown")

    def test_false_done_and_unverified_placement_never_count_as_success(self):
        config = self.config()
        report = new_report(config, "http://fixture")
        report["warmup"] = {"wall_seconds": 1, "cases": [{
            "id": "sum", "wall_seconds": 1, "status": "success", "payload": {
                "route": {"selected_model": "gpu:fixture"},
                "execution": {"observation": {"status": "unknown", "processor": "gpu"}},
                "admission": {"queue_wait_ms": 0},
                "response": {"done": False, "message": {"content": "45"}},
            }}]}
        summarize(report, config)
        self.assertEqual(report["completion"]["successful_requests"], 0)
        self.assertTrue(any("verified gpu" in failure for failure in report["failures"]))
        self.assertTrue(any("done" in failure for failure in report["failures"]))

    def test_hung_production_transport_retains_completed_and_inflight_requests(self):
        config = self.config()
        config["run_budget_seconds"] = .5
        started = time.monotonic()
        report = run_workload.run_bounded(config, "http://fixture", None, self.root / "hung.json",
                                          worker=hang_in_transport)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(report["warmup"]["cases"][0]["status"], "success")
        self.assertEqual(report["warmup"]["cases"][1]["status"], "submitted")
        self.assertEqual(report["completion"]["successful_requests"], 1)
        self.assertEqual(report["completion"]["submitted_requests"], 2)
        self.assertEqual(report["verdict"], "reject")

    def test_invalid_nested_payload_is_rejected_without_losing_the_receipt(self):
        config = self.config()
        report = new_report(config, "http://fixture")
        report["warmup"] = {"wall_seconds": 1, "cases": [{"id": "sum", "status": "success",
                            "wall_seconds": 1, "payload": {"response": [], "admission": False,
                            "execution": "invalid", "route": None}}]}
        summarize(report, config)
        self.assertEqual(report["verdict"], "reject")
        self.assertEqual(report["completion"]["successful_requests"], 0)
        self.assertTrue(any("exact requested tag" in value for value in report["failures"]))

    def test_worker_start_failure_still_writes_a_reject_receipt(self):
        config = self.config()
        context = Mock()
        context.Process.return_value.start.side_effect = OSError("worker unavailable")
        output = self.root / "start-failed.json"
        with patch.object(run_workload.multiprocessing, "get_context", return_value=context):
            report = run_workload.run_bounded(config, "http://fixture", None, output)
        self.assertEqual(report["verdict"], "reject")
        self.assertEqual(json.loads(output.read_text())["completion"]["completed_requests"], 0)
        self.assertTrue(any("worker unavailable" in value for value in report["failures"]))

    def test_duplicate_case_cannot_replace_missing_case_in_one_phase(self):
        report, _ = self.measure()
        results = report["performance"]["trials"][0]["parallel"]["cases"]
        results[1] = copy.deepcopy(results[0])
        summarize(report, self.config())
        self.assertEqual(report["verdict"], "reject")
        self.assertTrue(any("case set" in value for value in report["failures"]))
        self.assertIsNone(report["performance"]["parallel"]["qualified_requests_per_minute"])

    def test_replayed_trial_order_cannot_supply_a_matched_alternating_pair(self):
        report, _ = self.measure()
        report["performance"]["trials"][1] = copy.deepcopy(report["performance"]["trials"][0])
        summarize(report, self.config())
        self.assertEqual(report["verdict"], "reject")
        self.assertTrue(any("order" in value for value in report["failures"]))

    def test_zero_batch_clock_cannot_qualify_without_a_measured_baseline(self):
        report, _ = self.measure()
        report["performance"]["trials"][0]["sequential"]["wall_seconds"] = 0
        summarize(report, self.config())
        self.assertEqual(report["verdict"], "reject")
        self.assertTrue(any("wall_seconds" in value for value in report["failures"]))

    def test_initial_host_hold_or_invalid_health_stops_before_warmup(self):
        for holding, bad_contract in ((True, False), (False, True)):
            payload = health()
            payload["admission"]["resources"]["holding"] = holding
            if bad_contract:
                payload["contracts"]["placement_observation"] = "unsupported"
            with self.subTest(holding=holding, bad_contract=bad_contract):
                report, calls = self.measure(health_payload=payload)
                self.assertEqual(report["verdict"], "reject")
                self.assertEqual(calls, [])
                self.assertIsNone(report["warmup"])


if __name__ == "__main__":
    unittest.main()
