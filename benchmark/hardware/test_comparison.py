"""Outcome contracts for a matched direct/managed comparison sensor."""
import copy
import json
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

import run_comparison
from comparison_contract import load_comparison, new_comparison_report, summarize_comparison, profile_errors, digest_json
from test_workload import manifest, health


def snapshot(config):
    resident = {"name": config["model"], "digest": "sha256:fixture", "context_length": 4096,
                "size": 100, "size_vram": 100}
    return {"observed_at_unix_seconds": time.time(), "health": health(), "status": {
        "host": {"holding": False, "sample_age_ms": 0, "memory_pressure": "normal"},
        "raw_proxy": {"active": 0, "waiting": 0},
        "backends": {"gpu": {"upstream": "http://direct", "circuit": {"state": "closed"},
                      "admission": {"active_units": 0, "queue_depth": 0, "resource_waiters": 0}}},
        "ollama": {"config": {"process_inspection": "observed: ollama serve pid 42 for http://direct",
             "observed_at": time.time(), "observation_scope": "process_snapshot",
             "effective": {"num_parallel": 2}, "settings": {"OLLAMA_NUM_PARALLEL": {
                 "source": "process", "value": "2"}, "OLLAMA_KV_CACHE_TYPE": {
                 "source": "ollama_default", "value": None}}}}},
        "runtime": {"settings": {"task_costs": {"value": {"coding": 2}}}},
        "version": {"version": "fixture-v1"}, "tags": {"models": [resident]}, "ps": {"models": [resident]}}


def result(case, config, mode):
    response = {"model": config["model"], "done": True, "message": {"content": case["expected_text"]},
                "eval_count": 8, "prompt_eval_count": 10, "prompt_eval_cached_count": 4,
                "total_duration": 1000000000, "load_duration": 1000000,
                "prompt_eval_duration": 2000000, "eval_duration": 3000000}
    payload = response if mode == "direct" else {
        "response": response, "route": {"selected_model": config["model"]},
        "admission": {"queue_wait_ms": 0, "total_wait_ms": 0},
        "execution": {"upstream": "http://direct", "model_digest": "sha256:fixture",
            "runtime_options": run_comparison.chat_body(config, case)["options"],
            "observation": {"status": "verified", "processor": "gpu", "digest": "sha256:fixture",
                            "context_length": 4096}},
        "metrics": {"output_tokens": 8, "prompt_tokens": 10, "cached_prompt_tokens": 4}}
    return {"id": case["id"], "repeat": 0, "status": "success", "wall_seconds": 2, "payload": payload,
            "logical_request_sha256": digest_json(run_comparison.chat_body(config, case)),
            "request_sha256": digest_json(run_comparison.chat_body(config, case)) if mode == "direct" else "managed-actual-wire"}


def mocked_measure(config, endpoint, token, checkpoint, wrong_first=False, hang=False):
    state = snapshot(config)
    answers = {case["prompt"]: case for case in config["cases"]}
    calls = []

    def transport(url, body, auth, timeout):
        if body is None:
            for key, suffix in (("health", "/health"), ("status", "/status"), ("runtime", "/config"),
                                ("version", "/version"), ("tags", "/tags"), ("ps", "/ps")):
                if url.endswith(suffix):
                    state["observed_at_unix_seconds"] = time.time()
                    state["status"]["ollama"]["config"]["observed_at"] = time.time()
                    return copy.deepcopy(state[key])
            raise AssertionError(url)
        calls.append(copy.deepcopy(body))
        direct = url.endswith("/chat")
        prompt = body["messages"][0]["content"] if direct else body["prompt"]
        case = answers[prompt]
        if hang and case["id"] == "product":
            time.sleep(30)
        value = result(case, config, "direct" if direct else "managed")["payload"]
        if wrong_first and len(calls) == 1:
            (value if direct else value["response"])["message"]["content"] = "wrong"
        time.sleep(.003)
        return value

    with patch.object(run_comparison, "call", side_effect=transport):
        run_comparison.measure_comparison(config, endpoint, token, checkpoint)
    return calls


def hanging_comparison(config, endpoint, token, checkpoint):
    mocked_measure(config, endpoint, token, checkpoint, hang=True)


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "workload.json").write_text(json.dumps(manifest()))
        (self.root / "comparison.json").write_text(json.dumps({"schema_version": 1,
                  "workload": "workload.json", "repeats_per_phase": 1}))
        self.config = load_comparison(self.root / "comparison.json")
        self.config["direct_endpoint"] = "http://direct"

    def receipt(self, managed_wall=2, direct_wall=2):
        config = self.config
        report = new_comparison_report(config, "http://managed")
        report["profile"] = snapshot(config)
        report["snapshots"] = [copy.deepcopy(report["profile"])]
        for index in range(config["trials"] + 1):
            pair = {"order": ["direct", "managed"] if index % 2 == 0 else ["managed", "direct"]}
            for mode in pair["order"]:
                pair[mode] = {"wall_seconds": direct_wall if mode == "direct" else managed_wall,
                    "cases": [result(case, config, mode) for case in config["cases"]],
                    "before": copy.deepcopy(report["profile"]), "after": copy.deepcopy(report["profile"])}
            if index == 0:
                report["warmup"] = pair
            else:
                # Measured pair numbering starts at zero independently of warmup.
                pair["order"] = ["direct", "managed"] if (index - 1) % 2 == 0 else ["managed", "direct"]
                report["pairs"].append(pair)
        summarize_comparison(report, config)
        return report

    def test_equal_wall_is_no_gain_despite_overlapping_request_latencies(self):
        report = self.receipt()
        self.assertEqual(report["verdict"], "accept")
        self.assertEqual(report["performance"]["managed_over_direct_wall_ratio"], 1)
        self.assertEqual(report["performance"]["direct"]["qualified_requests_per_minute"], 60)
        self.assertEqual(report["performance"]["managed"]["qualified_requests_per_minute"], 60)
        self.assertEqual(report["performance"]["pair_count"], 3)
        self.assertNotIn("engine_ranking", report)

    def test_slower_managed_is_overhead_and_optional_declared_gate_rejects(self):
        report = self.receipt(managed_wall=3)
        self.assertEqual(report["performance"]["managed_over_direct_wall_ratio"], 1.5)
        self.assertEqual(report["verdict"], "accept")
        self.config["maximum_overhead_ratio"] = 1.25
        summarize_comparison(report, self.config)
        self.assertEqual(report["verdict"], "reject")
        self.assertIsNone(report["performance"]["managed"]["qualified_requests_per_minute"])

    def test_unmeasured_ratio_is_unavailable_not_a_measured_overhead_failure(self):
        self.config["maximum_overhead_ratio"] = 1.15
        report = new_comparison_report(self.config, "http://managed")
        report["profile"] = snapshot(self.config)
        report["profile"]["ps"]["models"] = []
        summarize_comparison(report, self.config)
        self.assertEqual(report["verdict"], "reject")
        self.assertEqual(report["completion"]["submitted_requests"], 0)
        self.assertIsNone(report["performance"]["managed_over_direct_wall_ratio"])
        self.assertIn("managed/direct wall ratio unavailable; declared overhead gate unqualified", report["failures"])
        self.assertFalse(any("exceeds declared maximum" in failure for failure in report["failures"]))
        self.assertIn("resident digest unavailable; installed revision comparison unqualified",
                      report["failures"])

    def test_wrong_golden_or_false_done_retains_all_receipts_and_no_goodput(self):
        for field, bad in (("done", False), ("message", {"content": "wrong"})):
            report = self.receipt()
            report["pairs"][0]["direct"]["cases"][0]["payload"][field] = bad
            summarize_comparison(report, self.config)
            self.assertEqual(report["verdict"], "reject")
            self.assertEqual(report["completion"]["completed_requests"], 16)
            self.assertIsNone(report["performance"]["direct"]["qualified_requests_per_minute"])

    def test_profile_change_or_missing_physical_gpu_evidence_invalidates_comparison(self):
        for mutate in (lambda s: s["ps"]["models"][0].update(size_vram=90),
                       lambda s: s["ps"]["models"][0].update(digest="different"),
                       lambda s: s["status"]["ollama"]["config"]["effective"].update(num_parallel=1)):
            report = self.receipt()
            mutate(report["pairs"][0]["direct"]["after"])
            summarize_comparison(report, self.config)
            self.assertEqual(report["verdict"], "reject")
            self.assertIsNone(report["performance"]["managed_over_direct_wall_ratio"])

    def test_missing_model_unobserved_process_or_busy_host_is_no_inference(self):
        for mutate in (lambda s: s.update(ps={"models": []}),
                       lambda s: s["status"]["ollama"]["config"].update(process_inspection="fallback"),
                       lambda s: s["status"]["backends"]["gpu"]["admission"].update(active_units=1),
                       lambda s: s["status"]["host"].update(holding=True)):
            value = snapshot(self.config)
            mutate(value)
            with self.subTest(mutate=mutate):
                self.assertTrue(profile_errors(value, self.config))

    def test_repeat_or_order_replay_cannot_replace_the_planned_case_set(self):
        report = self.receipt()
        report["pairs"][1]["order"] = ["direct", "managed"]
        report["pairs"][0]["direct"]["cases"][1] = copy.deepcopy(report["pairs"][0]["direct"]["cases"][0])
        summarize_comparison(report, self.config)
        self.assertEqual(report["verdict"], "reject")
        self.assertFalse(report["completion"]["complete"])

    def test_duration_gate_cannot_turn_a_short_smoke_into_sustained_evidence(self):
        self.config["minimum_measured_seconds"] = 20
        report = self.receipt()
        self.assertEqual(report["performance"]["measured_seconds"], 12)
        self.assertFalse(report["performance"]["duration_target_met"])
        self.assertEqual(report["verdict"], "reject")

    def test_direct_queue_and_energy_unknown_cache_is_reported_count_only(self):
        report = self.receipt()
        self.assertEqual(report["performance"]["direct"]["queue_wait_ms"]["sample_count"], 0)
        self.assertIsNone(report["performance"]["direct"]["queue_wait_ms"]["p95"])
        self.assertEqual(report["performance"]["direct"]["cached_prompt_tokens"]["reported_total"], 24)
        self.assertIsNone(report["performance"]["energy_joules"])
        self.assertIn("shared", report["performance"]["cache_policy"])

    def test_historical_freshness_uses_collection_clock_and_stale_capture_fails(self):
        old = snapshot(self.config)
        old["observed_at_unix_seconds"] -= 120
        old["status"]["ollama"]["config"]["observed_at"] -= 120
        self.assertEqual(profile_errors(old, self.config), [])
        old["status"]["ollama"]["config"]["observed_at"] -= 20
        self.assertTrue(any("stale" in error for error in profile_errors(old, self.config)))

    def test_wrong_wire_identity_cannot_qualify_with_a_correct_answer(self):
        report = self.receipt()
        report["pairs"][0]["direct"]["cases"][0]["request_sha256"] = "wrong-prompt"
        summarize_comparison(report, self.config)
        self.assertEqual(report["verdict"], "reject")
        self.assertIsNone(report["performance"]["direct"]["qualified_requests_per_minute"])

    def test_late_correct_outputs_are_quality_successes_and_slo_misses(self):
        self.config["latency_slo_ms"] = 500
        report = self.receipt()
        direct = report["performance"]["direct"]
        self.assertEqual(report["verdict"], "accept")
        self.assertEqual(direct["correct_requests"], 6)
        self.assertEqual(direct["slo_misses"], 6)
        self.assertEqual(direct["slo_pass_rate"], 0)
        self.assertEqual(direct["slo_qualified_requests_per_minute"], 0)
        self.assertEqual(direct["qualified_requests_per_minute"], 60)

    def test_production_mocked_paths_match_payload_and_stop_after_quality_failure(self):
        output = self.root / "production.json"
        calls = mocked_measure(self.config, "http://managed", None, output)
        report = run_comparison.read_receipt(output)
        self.assertEqual(report["verdict"], "accept", report["failures"])
        self.assertEqual(len(calls), 16)
        self.assertEqual(report["completion"]["successful_requests"], 16)
        self.assertTrue(all(body.get("options", body.get("request_options", {}).get("options"))["num_predict"] == 256 for body in calls))
        for case in self.config["cases"]:
            self.assertEqual(report["requests"][case["id"]]["direct"], run_comparison.chat_body(self.config, case))
        calls = mocked_measure(self.config, "http://managed", None, output, wrong_first=True)
        report = run_comparison.read_receipt(output)
        self.assertEqual(report["verdict"], "reject")
        self.assertLessEqual(len(calls), 2)
        self.assertTrue(any("golden" in error for error in report["failures"]))

    def test_hung_mocked_transport_preserves_incremental_receipts_with_external_deadline(self):
        self.config["run_budget_seconds"] = .6
        started = time.monotonic()
        report = run_comparison.run_bounded(self.config, "http://managed", None, self.root / "hung.json",
            worker=hanging_comparison, report_factory=new_comparison_report,
            report_summary=summarize_comparison, receipt_reader=run_comparison.read_receipt)
        self.assertLess(time.monotonic() - started, 2)
        self.assertEqual(report["completion"]["submitted_requests"], 2)
        self.assertEqual(report["completion"]["successful_requests"], 1)
        self.assertEqual(report["warmup"]["direct"]["cases"][1]["status"], "submitted")
        self.assertEqual(report["verdict"], "reject")
        self.assertEqual(report["upstream_cancellation"], "unknown")

    def test_captured_profile_remains_valid_after_sustained_time_and_output_is_fixed(self):
        report = self.receipt(managed_wall=22, direct_wall=21)
        self.config["minimum_measured_seconds"] = 120
        for pair in [report["warmup"], *report["pairs"]]:
            for mode in ("direct", "managed"):
                for boundary in ("before", "after"):
                    snapshot = pair[mode][boundary]
                    snapshot["observed_at_unix_seconds"] -= 240
                    snapshot["status"]["ollama"]["config"]["observed_at"] -= 240
        report["profile"]["observed_at_unix_seconds"] -= 240
        report["profile"]["status"]["ollama"]["config"]["observed_at"] -= 240
        summarize_comparison(report, self.config)
        self.assertEqual(report["verdict"], "accept", report["failures"])
        self.assertEqual(report["performance"]["measured_seconds"], 129)

    def test_missing_resident_guard_in_production_never_submits_generation(self):
        bad = snapshot(self.config)
        bad["ps"]["models"] = None
        with patch.object(run_comparison, "observe", return_value=bad), patch.object(run_comparison, "call") as http:
            output = self.root / "blocked.json"
            run_comparison.measure_comparison(self.config, "http://managed", None, output)
            report = run_comparison.read_receipt(output)
            http.assert_not_called()
        self.assertEqual(report["verdict"], "reject")
        self.assertEqual(report["completion"]["submitted_requests"], 0)
        self.assertIn("resident", " ".join(report["failures"]))

    def test_slo_counts_use_each_completion_latency_and_keep_partial_correctness(self):
        self.config["latency_slo_ms"] = 500
        report = self.receipt()
        for pair in report["pairs"]:
            pair["direct"]["cases"][0]["wall_seconds"] = .1
        summarize_comparison(report, self.config)
        summary = report["performance"]["direct"]
        self.assertEqual(summary["correct_requests"], 6)
        self.assertEqual(summary["slo_qualified_requests"], 3)
        self.assertEqual(summary["slo_misses"], 3)
        self.assertEqual(summary["slo_pass_rate"], .5)
        self.assertEqual(summary["slo_qualified_requests_per_minute"], 30)
        report["pairs"][0]["direct"]["cases"][1]["payload"]["done"] = False
        summarize_comparison(report, self.config)
        self.assertEqual(report["performance"]["direct"]["correct_requests"], 5)
        self.assertEqual(report["verdict"], "reject")
        self.assertIsNone(report["performance"]["direct"]["slo_qualified_requests_per_minute"])

    def test_partial_journal_tail_cannot_hide_prior_success(self):
        output = self.root / "tail.json"
        mocked_measure(self.config, "http://managed", None, output)
        with output.with_suffix(".events").open("a") as journal:
            journal.write('{"partial":')
        report = run_comparison.read_receipt(output)
        summarize_comparison(report, self.config)
        self.assertEqual(report["completion"]["successful_requests"], 16)
        self.assertEqual(report["verdict"], "reject")
        self.assertIn("incomplete event", " ".join(report["failures"]))

    def test_invalid_budget_or_unbounded_repeats_reject_before_http(self):
        for change in ({"repeats_per_phase": 0}, {"repeats_per_phase": 1000000},
                       {"minimum_measured_seconds": 11}, {"extra": 1}):
            (self.root / "comparison.json").write_text(json.dumps({"schema_version": 1,
                "workload": "workload.json", **change}))
            with self.subTest(change=change), patch.object(run_comparison, "call") as http:
                with self.assertRaises(ValueError):
                    load_comparison(self.root / "comparison.json")
                http.assert_not_called()


if __name__ == "__main__":
    unittest.main()
