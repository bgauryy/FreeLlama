#!/usr/bin/env python3
"""Fail once, then reveal a value; used to test recovery."""

from pathlib import Path

state = Path(".flaky-read-state")
if not state.exists():
    state.write_text("failed-once\n", encoding="utf-8")
    raise SystemExit("temporary read failure; retry is safe")
print("RECOVERED-731")

