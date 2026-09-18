#!/usr/bin/env python3
"""Managed/direct transport contracts for both research adapters."""

import os
import io
import json
from contextlib import redirect_stdout
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError, URLError

from agent_transport import chat_request, request_headers, unwrap_chat_response, PromptCacheUsage, retryable_chat_error
from agent_context import ObservationStore
import bash_agent
import octocode_agent


class AgentTransportTests(unittest.TestCase):
    def test_unverified_model_identity_disables_persistent_calibration_in_both_adapters(self):
        for adapter in [bash_agent, octocode_agent]:
            with self.subTest(adapter=adapter.__name__), tempfile.TemporaryDirectory() as root:
                prompt = Path(root) / "prompt.txt"
                prompt.write_text("Find one fact.", encoding="utf-8")
                result_path = Path(root) / "result.json"
                cache_path = Path(root) / "calibration"
                settings = {"FREELLAMA_TARGET_MODEL": "test:latest", "FREELLAMA_BENCH_WORKSPACE": root,
                            "FREELLAMA_BENCH_PROMPT": str(prompt), "FREELLAMA_AGENT_RESULT": str(result_path),
                            "FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR": str(cache_path)}
                response = {"message": {"content": '{"action":"finish","answer":"done"}'}, "prompt_eval_count": 6000}
                with patch.dict(os.environ, settings, clear=True), patch.object(adapter, "request_json", return_value=response), \
                     patch.object(adapter, "resolve_model_identity", return_value={"identity": "", "verified": False, "scope": "current_process_only"}) as identity, \
                     patch("builtins.print"):
                    self.assertEqual(adapter.main(), 0)
                identity.assert_called_once()
                self.assertFalse(cache_path.exists())
                decoded = json.loads(result_path.read_text(encoding="utf-8"))
                self.assertFalse(decoded["model_metadata"]["calibration_model_identity"]["verified"])
                self.assertEqual(decoded["model_metadata"]["context_management"]["calibration_samples"], 1)

    def test_both_adapters_spool_results_page_and_cleanup_without_retaining_full_outputs(self):
        payload = "FIRST\n" + "🦙\\\"\r\n" * 20_000 + "LAST"
        for adapter, runner_name, action in [
            (bash_agent, "run_shell", {"action": "shell", "command": "read src/router.py"}),
            (octocode_agent, "run_octocode", {"action": "octocode", "tool": "localGetFileContent", "queries": {"path": "src/router.py"}}),
        ]:
            with self.subTest(adapter=adapter.__name__), tempfile.TemporaryDirectory() as root:
                prompt = Path(root) / "prompt.txt"
                prompt.write_text("Find the router.", encoding="utf-8")
                result_path = Path(root) / "result.json"
                requests, stores = [], []
                actions = iter([action, {"action": "page", "step": 1, "page": 2}, {"action": "finish", "answer": "done"}])

                def respond(request, **_):
                    requests.append(json.loads(request.data))
                    return io.BytesIO(json.dumps({"message": {"content": json.dumps(next(actions))}, "prompt_eval_count": 100, "eval_count": 10}).encode())

                def make_store(*args, **kwargs):
                    store = ObservationStore(*args, **kwargs)
                    stores.append(store)
                    return store

                env = {"FREELLAMA_TARGET_MODEL": "test:latest", "FREELLAMA_BENCH_WORKSPACE": root,
                       "FREELLAMA_BENCH_PROMPT": str(prompt), "FREELLAMA_AGENT_RESULT": str(result_path)}
                with patch.dict(os.environ, env, clear=True), patch.object(adapter, "urlopen", side_effect=respond), \
                     patch.object(adapter, runner_name, return_value=payload) as runner, \
                     patch.object(adapter, "ObservationStore", side_effect=make_store), \
                     patch.object(adapter, "resolve_model_identity") as identity, patch("builtins.print"):
                    self.assertEqual(adapter.main(), 0)
                self.assertEqual(runner.call_count, 1)
                identity.assert_not_called()
                decoded = json.loads(result_path.read_text(encoding="utf-8"))
                self.assertEqual(decoded["tool_calls"][0]["result"], payload)
                self.assertEqual(decoded["final_answer"], "done")
                self.assertEqual(decoded["tool_calls"][0]["status"], "ok")
                self.assertIn('"page":2', requests[-1]["messages"][-1]["content"])
                self.assertTrue(all(len(message.get("content", "")) < 10_000 for request in requests for message in request["messages"]))
                self.assertTrue(all(not store.directory.exists() for store in stores))

    def test_both_adapter_loops_do_not_replay_timeouts_and_report_cache_reads(self):
        import bash_agent
        import octocode_agent
        for adapter in [bash_agent, octocode_agent]:
            with self.subTest(adapter=adapter.__name__), tempfile.TemporaryDirectory() as root:
                prompt = Path(root) / "prompt.txt"
                prompt.write_text("Find one fact.", encoding="utf-8")
                result = Path(root) / "result.json"
                settings = {
                    "FREELLAMA_TARGET_MODEL": "test:latest",
                    "FREELLAMA_BENCH_WORKSPACE": root,
                    "FREELLAMA_BENCH_PROMPT": str(prompt),
                    "FREELLAMA_AGENT_RESULT": str(result),
                    "FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS": "0",
                }
                with patch.dict(os.environ, settings, clear=True), redirect_stdout(io.StringIO()):
                    with patch.object(adapter, "request_json", side_effect=TimeoutError()) as request:
                        self.assertEqual(adapter.main(), 1)
                        self.assertEqual(request.call_count, 1)
                    response = {"message":{"content":'{"action":"finish","answer":"done"}'},
                                "prompt_eval_count":100, "prompt_eval_cached_count":40, "eval_count":5}
                    with patch.object(adapter, "request_json", return_value=response) as request:
                        self.assertEqual(adapter.main(), 0)
                        self.assertEqual(request.call_count, 1)
                    record = json.loads(result.read_text())
                    self.assertEqual(record["usage"]["input_tokens"], 100)
                    self.assertEqual(record["usage"]["cache_read_tokens"], 40)
                    self.assertEqual(record["model_metadata"]["cache_token_metrics"]["status"], "reported")

    def test_retry_only_when_inference_was_refused(self):
        busy = HTTPError("url", 503, "busy", {}, None)
        self.assertTrue(retryable_chat_error(busy))
        busy.close()
        self.assertTrue(retryable_chat_error(URLError(ConnectionRefusedError())))
        for error in [TimeoutError(), URLError(TimeoutError()),
                      HTTPError("url", 504, "timeout", {}, None),
                      HTTPError("url", 400, "invalid", {}, None),
                      URLError(ConnectionResetError())]:
            with self.subTest(error=error):
                self.assertFalse(retryable_chat_error(error))
            if isinstance(error, HTTPError):
                error.close()

    def test_cache_usage_distinguishes_missing_zero_and_partial_reporting(self):
        cache = PromptCacheUsage()
        cache.observe({"prompt_eval_count": 10})
        self.assertIsNone(cache.tokens)
        cache.observe({"prompt_eval_count": 10, "prompt_eval_cached_count": 0})
        self.assertIsNone(cache.tokens)  # A missing turn must not become an invented zero.
        self.assertEqual(cache.metadata()["reported_tokens"], 0)
        self.assertEqual(cache.metadata()["status"], "partially_reported")

    def test_cache_usage_counts_cached_subset_without_double_counting_prompt(self):
        cache = PromptCacheUsage()
        cache.observe({"prompt_eval_count": 10, "prompt_eval_cached_count": 4})
        cache.observe({"prompt_eval_count": 20, "prompt_eval_cached_count": 8})
        self.assertEqual(cache.tokens, 12)
        self.assertEqual(cache.metadata()["status"], "reported")
        self.assertAlmostEqual(cache.metadata()["hit_ratio"], 0.4)

    def test_invalid_cache_counts_remain_unknown(self):
        for value in [-1, 11, True, "4"]:
            with self.subTest(value=value):
                cache = PromptCacheUsage()
                cache.observe({"prompt_eval_count": 10, "prompt_eval_cached_count": value})
                self.assertIsNone(cache.tokens)

    def test_managed_request_uses_coding_route_and_keeps_num_ctx_routing_owned(self):
        url, body = chat_request(
            "http://127.0.0.1:11435/_freellama/v1/tasks",
            "qwen:latest",
            [{"role": "user", "content": "inspect"}],
            {"num_ctx": 8192, "num_predict": 512, "temperature": 0, "seed": 42},
            False,
            "5m",
            "prefer_cpu",
            "observed",
        )
        self.assertEqual(url, "http://127.0.0.1:11435/_freellama/v1/tasks")
        self.assertEqual(body["task"], "coding")
        self.assertEqual(body["context_tokens"], 8192)
        self.assertEqual(body["execution_preference"], "prefer_cpu")
        self.assertEqual(body["min_placement_evidence"], "observed")
        self.assertNotIn("num_ctx", body["request_options"]["options"])

    def test_direct_request_remains_available_for_benchmark_comparisons(self):
        url, body = chat_request(
            "http://127.0.0.1:11434",
            "qwen:latest",
            [],
            {"num_ctx": 4096},
            False,
            "0",
        )
        self.assertEqual(url, "http://127.0.0.1:11434/api/chat")
        self.assertEqual(body["options"]["num_ctx"], 4096)
        self.assertIs(body["truncate"], False)
        self.assertIs(body["shift"], False)

    def test_managed_wrapper_is_unwrapped_and_receipt_is_retained(self):
        receipts = []
        response = unwrap_chat_response(
            {
                "route": {"selected_model": "qwen:latest"},
                "execution": {"backend": "cpu", "observation": {"processor": "cpu"}},
                "admission": {"mode": "resident_shared"},
                "feedback": {"accepted": True},
                "response": {"message": {"content": "{}"}},
            },
            receipts,
        )
        self.assertEqual(response["message"]["content"], "{}")
        self.assertEqual(receipts[0]["execution"]["backend"], "cpu")

    def test_bearer_token_is_read_from_a_file(self):
        with tempfile.TemporaryDirectory() as root:
            token_file = Path(root) / "token"
            token_file.write_text("a" * 32 + "\n", encoding="utf-8")
            with patch.dict(os.environ, {"FREELLAMA_AUTH_TOKEN_FILE": str(token_file)}):
                self.assertEqual(request_headers()["authorization"], f"Bearer {'a' * 32}")


if __name__ == "__main__":
    unittest.main()
