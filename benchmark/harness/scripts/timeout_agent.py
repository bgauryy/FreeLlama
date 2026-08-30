#!/usr/bin/env python3
"""Spawn a descendant and hang; self_test verifies process-group cleanup."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import subprocess
import sys
import time

argparse.ArgumentParser(description="Self-test helper for timeout process-tree cleanup.").parse_args()
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
Path(os.environ["FREELLAMA_BENCH_WORKSPACE"], "child.pid").write_text(str(child.pid), encoding="utf-8")
time.sleep(60)
