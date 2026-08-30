#!/usr/bin/env python3
"""Deterministic golden agent for validating the real-repository benchmark harness."""

from __future__ import annotations

import json
import os
from pathlib import Path


TASK = os.environ["FREELLAMA_BENCH_TASK_ID"]
WORKSPACE = Path(os.environ["FREELLAMA_BENCH_WORKSPACE"])
RESULT = Path(os.environ["FREELLAMA_AGENT_RESULT"])

ANSWERS = {
    "R01": "Serializer is in itsdangerous/src/itsdangerous/serializer.py; Serializer.dumps signs and Serializer.loads unsigns. The public exports are in itsdangerous/src/itsdangerous/__init__.py.",
    "R02": "click/src/click/formatting.py defines HelpFormatter defaults: max_width 80, computed width floor 50, and the FORCED_WIDTH test override.",
    "R05": "In click/src/click/core.py, Command.main creates a Context with make_context, enters Context, calls invoke, and standalone_mode converts Click exceptions and exits into user-facing output and SystemExit behavior.",
    "R07": "itsdangerous/src/itsdangerous/url_safe.py dump_payload may zlib-compress, marks compression, and base64 encodes; load_payload reverses that. itsdangerous/src/itsdangerous/signer.py splits the value and verify_signature checks the MAC. Tampering diverges at verify_signature and raises BadSignature because the signed-byte invariant is violated. An alternate decompression failure is ruled out because signature verification happens before payload loading.",
    "R09": "itsdangerous/src/itsdangerous/signer.py uses key_derivation django-concat and sep b\".\". itsdangerous/src/itsdangerous/url_safe.py uses zlib. click/src/click/formatting.py defaults max_width to 80. click/src/click/core.py uses the call argument to control invocation of callable default_map values.",
}

FIXES = {
    "R03": ("itsdangerous/src/itsdangerous/encoding.py", "return base64.urlsafe_b64encode(string).lstrip(b\"=\")", "return base64.urlsafe_b64encode(string).rstrip(b\"=\")"),
    "R04": ("itsdangerous/src/itsdangerous/signer.py", "            if self.algorithm.verify_signature(key, value, sig):\n                return False", "            if self.algorithm.verify_signature(key, value, sig):\n                return True"),
    "R06": ("click/src/click/formatting.py", "widths[idx] = max(widths.get(idx, 0), len(col))", "widths[idx] = max(widths.get(idx, 0), term_len(col))"),
    "R08": ("itsdangerous/src/itsdangerous/timed.py", "            if age >= max_age:", "            if age > max_age:"),
}

calls = []
if TASK in FIXES:
    relative, old, new = FIXES[TASK]
    path = WORKSPACE / relative
    text = path.read_text(encoding="utf-8")
    if text.count(old) != 1:
        raise SystemExit(f"golden repair expected one match for {TASK}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")
    calls.append({"name": "edit", "arguments": {"path": relative}, "status": "ok"})
elif TASK == "R10":
    path = WORKSPACE / "click/src/click/core.py"
    text = path.read_text(encoding="utf-8")
    replacements = [
        ("            and name in self.default_map\n            and bool(self.default_map[name])", "            and name in self.default_map\n            and self.default_map[name] is not UNSET"),
        ("        value = self.default_map[name]\n\n        if callable(value):\n            return value()", "        value = self.default_map[name]\n\n        if call and callable(value):\n            return value()"),
    ]
    for old, new in replacements:
        if text.count(old) != 1:
            raise SystemExit("golden R10 repair expected one match")
        text = text.replace(old, new, 1)
    path.write_text(text, encoding="utf-8")
    calls.append({"name": "edit", "arguments": {"path": "click/src/click/core.py"}, "status": "ok"})
else:
    calls.extend([
        {"name": "search", "arguments": {"query": TASK}, "status": "ok"},
        {"name": "read", "arguments": {"path": "source evidence"}, "status": "ok"},
    ])

answer = ANSWERS.get(TASK, "Implemented the minimal repair and verified the requested behavior with focused tests.")
RESULT.parent.mkdir(parents=True, exist_ok=True)
RESULT.write_text(json.dumps({"final_answer": answer, "tool_calls": calls, "usage": {"input_tokens": 1, "output_tokens": 1}}, indent=2) + "\n", encoding="utf-8")
print(answer)
