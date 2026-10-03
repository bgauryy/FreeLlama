"""Regression checks for measured, rather than inferred, CPU/GPU overlap."""

import itertools
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import run_validation


class ThroughputQualificationTests(unittest.TestCase):
    def run_fixture(self, clock, fail_early=False, embeddings=None, gpu_text="HARDWARE_GPU_OK"):
        health = {
            "contracts": {
                "authentication": "optional_bearer_all_routes",
                "placement_feedback_persistence": "versioned_atomic_snapshot_v1",
                "placement_observation": "ollama_api_ps_after_execution",
            },
            "feedback": {"persistence": {"enabled": True}},
        }

        calls = 0

        def task(_url, body, _token):
            nonlocal calls
            calls += 1
            processor = "cpu" if body["task"] == "embedding" else "gpu"
            response = {"done": True, "message": {"content": gpu_text}}
            if fail_early and calls == 3:
                response["message"]["content"] = "WRONG"
            if processor == "cpu":
                response = {"embeddings": embeddings if embeddings is not None else [[1.0], [2.0]]}
            return {"wall_seconds": 1.0, "payload": {
                "execution": {"observation": {"status": "verified", "processor": processor}},
                "admission": {"queue_wait_ms": 0}, "response": response,
            }}

        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "receipt.json"
            argv = ["run_validation.py", "--gpu-model", "gpu:fixture", "--cpu-model",
                    "cpu:fixture", "--output", str(output)]
            with patch.object(sys, "argv", argv), patch.object(run_validation, "call", return_value=health), \
                    patch.object(run_validation, "timed_call", side_effect=task), \
                    patch.object(run_validation.time, "monotonic", side_effect=clock):
                result = run_validation.main()
            return result, json.loads(output.read_text())

    def test_overlapping_client_times_do_not_prove_throughput_gain(self):
        # Both separate sequential and parallel runs take one second. Summing two overlapping
        # one-second client durations would incorrectly report a twofold gain.
        result, report = self.run_fixture(itertools.cycle([0.0, 1.0]))
        self.assertEqual(result, 1)
        self.assertEqual(report["verdict"], "reject")
        self.assertEqual(report["performance"]["speedup"], 1.0)
        self.assertEqual(len(report["performance"]["trials"]), 3)

    def test_comparable_repeated_runs_measure_real_gain(self):
        # Alternating sequential/parallel pair order: (2s,1s), (1s,2s), (2s,1s).
        clock = iter([0, 2, 2, 3, 3, 4, 4, 6, 6, 8, 8, 9])
        result, report = self.run_fixture(clock)
        self.assertEqual(result, 0)
        self.assertEqual(report["performance"]["sequential_median_seconds"], 2.0)
        self.assertEqual(report["performance"]["parallel_median_seconds"], 1.0)
        self.assertEqual(report["performance"]["speedup"], 2.0)
        self.assertEqual(report["performance"]["measured_hardware_utilization"], "unknown")

    def test_later_success_cannot_hide_earlier_failed_work(self):
        clock = iter([0, 2, 2, 3, 3, 4, 4, 6, 6, 8, 8, 9])
        result, report = self.run_fixture(clock, fail_early=True)
        self.assertEqual(result, 1)
        self.assertEqual(report["performance"]["speedup"], 2.0)
        self.assertIsNone(report["performance"]["parallel_requests_per_minute"])
        self.assertTrue(any("trial1/sequential/gpu_coding" in failure for failure in report["failures"]))

    def test_fast_invalid_embeddings_cannot_qualify(self):
        for invalid in ([[], []], [[1], [2, 3]], [[float("nan")], [1]], [[True], [1]]):
            with self.subTest(embeddings=invalid):
                clock = iter([0, 2, 2, 3, 3, 4, 4, 6, 6, 8, 8, 9])
                result, report = self.run_fixture(clock, embeddings=invalid)
                self.assertEqual(result, 1)
                self.assertIsNone(report["performance"]["parallel_requests_per_minute"])
                self.assertTrue(any("finite numeric vectors" in failure for failure in report["failures"]))

    def test_marker_inside_unrequested_prose_is_not_correct_output(self):
        clock = iter([0, 2, 2, 3, 3, 4, 4, 6, 6, 8, 8, 9])
        result, report = self.run_fixture(clock, gpu_text="I ignored your request: HARDWARE_GPU_OK")
        self.assertEqual(result, 1)
        self.assertIsNone(report["performance"]["parallel_requests_per_minute"])


if __name__ == "__main__":
    unittest.main()
