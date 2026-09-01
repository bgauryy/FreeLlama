#!/usr/bin/env python3
"""Managed/direct transport contracts for both research adapters."""

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from agent_transport import chat_request, request_headers, unwrap_chat_response


class AgentTransportTests(unittest.TestCase):
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
