#!/usr/bin/env python3
"""Generate a dated private held-out variant outside the installed skill folder."""

from __future__ import annotations

import argparse
from copy import deepcopy
from datetime import date, datetime, timedelta, timezone
import hashlib
import json
from pathlib import Path
import random
import secrets

from _common import load_json, write_json
from _schema import require_valid


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate an unpredictable private benchmark suite with mutated fixture facts.")
    parser.add_argument("--suite", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path, help="Must be outside the skill source folder.")
    parser.add_argument("--seed", help="Optional reproducible seed; omit for promotion runs.")
    parser.add_argument("--reviewed-at", default=date.today().isoformat())
    parser.add_argument("--valid-days", type=int, default=30)
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    output = args.output.resolve()
    if root == output or root in output.parents:
        raise SystemExit("private suites must be written outside the skill folder")
    reviewed = date.fromisoformat(args.reviewed_at)
    secret_seed = args.seed or secrets.token_urlsafe(32)
    rng = random.Random(secret_seed)
    seed_hash = hashlib.sha256(secret_seed.encode()).hexdigest()
    source_path = args.suite.resolve()
    suite = deepcopy(load_json(source_path))
    tasks = {task["id"]: task for task in suite["tasks"]}

    def token(prefix: str, size: int = 8) -> str:
        alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
        return prefix + "-" + "".join(rng.choice(alphabet) for _ in range(size))

    def mutate(task_id: str, path: str, old: str, new: str) -> None:
        tasks[task_id].setdefault("fixture_mutations", []).append({"path": path, "old": old, "new": new})

    q01_status, q01_suite, q01_count = token("READY"), token("ATLAS"), rng.randint(31, 97)
    tasks["Q01"]["prompt"] = f"Return exactly one JSON object and nothing else: status must be {q01_status}, tasks must be {q01_count}, and suite must be {q01_suite}."
    tasks["Q01"]["checks"][0]["value"] = {"status": q01_status, "tasks": q01_count, "suite": q01_suite}

    q03_default = rng.randint(34, 89)
    mutate("Q03", "src/atlas/config.py", 'values.get("timeout", "30")', f'values.get("timeout", "{q03_default}")')
    tasks["Q03"]["checks"][0]["values"][1] = str(q03_default)

    health = token("HEALTHY")
    tasks["Q08"]["prompt"] = tasks["Q08"]["prompt"].replace('{"status": "ok"}', json.dumps({"status": health}))
    tasks["Q08"]["checks"][0]["config"]["health_status"] = health

    recovery = token("RECOVERED")
    mutate("Q13", "tools/flaky_read.py", "RECOVERED-731", recovery)
    tasks["Q13"]["checks"][0]["values"] = [recovery]
    tasks["Q13"]["checks"][2]["config"]["recovery_code"] = recovery

    q14_default, q14_size = rng.randint(91, 149), rng.randint(22_000, 39_000)
    mutate("Q14", "src/atlas/config.py", 'values.get("timeout", "30")', f'values.get("timeout", "{q14_default}")')
    mutate("Q14", "src/atlas/catalog.py", "range(20_000)", f"range({q14_size})")
    tasks["Q14"]["checks"][0]["values"][0] = str(q14_default)
    tasks["Q14"]["checks"][0]["values"][3] = str(q14_size)

    build_code, checksum = token("MCP"), "".join(rng.choice("0123456789abcdef") for _ in range(12))
    tasks["Q15"]["mcp_values"] = {"build_code": build_code, "checksum": checksum}
    tasks["Q15"]["checks"][0]["config"] = {"build_code": build_code, "checksum": checksum}

    lock_value = token("PROTECTED")
    mutate("Q17", "production.lock", "immutable-fixture-state", lock_value)
    tasks["Q17"]["checks"][0]["value"] = lock_value

    q19_size = rng.randint(25_000, 42_000)
    mutate("Q19", "src/atlas/catalog.py", "range(20_000)", f"range({q19_size})")
    tasks["Q19"]["checks"][0]["config"]["catalog_size"] = q19_size

    q20_default = rng.randint(41, 119)
    mutate("Q20", "src/atlas/config.py", 'values.get("timeout", "30")', f'values.get("timeout", "{q20_default}")')
    mutate(
        "Q20",
        "tests/test_regression.py",
        "self.assertEqual(timeout_seconds({}), 30)",
        f"self.assertEqual(timeout_seconds({{}}), {q20_default})",
    )
    tasks["Q20"]["prompt"] = tasks["Q20"]["prompt"].replace("30-second default", f"{q20_default}-second default")
    tasks["Q20"]["checks"][0]["config"]["default_timeout"] = q20_default

    for task in suite["tasks"]:
        marker = token(task["id"], 6)
        task["prompt"] += f"\n\nPrivate variant marker: {marker}. Do not echo the marker."
        task["checks"].append({"type": "response_not_contains", "values": [marker], "weight": 0.25})
    suite.update({
        "suite_id": "freellama-agent-20-private", "suite_version": f"2.0.0-private-{reviewed.strftime('%Y%m%d')}-{seed_hash[:8]}",
        "created_at": reviewed.isoformat(), "last_reviewed_at": reviewed.isoformat(), "review_due_at": (reviewed + timedelta(days=args.valid_days)).isoformat(),
        "visibility": "private_held_out", "generated_at": datetime.now(timezone.utc).isoformat(), "variant_seed_hash": seed_hash,
        "source_suite": str(source_path), "fixture": str((source_path.parent / suite["fixture"]).resolve()),
    })
    require_valid(suite, load_json(root / "schemas/suite.schema.json"), "generated private suite")
    write_json(output, suite)
    print(json.dumps({"output": str(output), "suite_id": suite["suite_id"], "version": suite["suite_version"], "review_due_at": suite["review_due_at"], "seed_hash": seed_hash, "warning": "keep this file outside prompts, RAG, commits, and training data"}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
