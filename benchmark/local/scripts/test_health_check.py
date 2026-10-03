"""Execute the standalone health helper against offline command fixtures."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[3]
CHECK = REPO / "skills/freellama/scripts/check.sh"
CONTRACTS = {
    "hardware_fit": "sent_num_ctx",
    "machine_profile": "portable_host_memory_v2",
    "model_backends": "explicit_cpu_assignment",
    "placement_preference": "guarded_hint",
    "placement_observation": "ollama_api_ps_after_execution",
    "placement_evidence_gate": "configured_or_observed",
    "placement_feedback": "three_sample_runtime",
    "placement_feedback_metric": "normalized_work_unit_10_percent",
    "placement_feedback_persistence": "versioned_atomic_snapshot_v1",
    "authentication": "optional_bearer_all_routes",
    "immediate_unload_observation": "observe_then_unload",
}


def current_health():
    return {
        "contracts": CONTRACTS.copy(),
        "backends": {"gpu": {"upstream": "http://fixture-ollama.invalid", "admission": {"slots_total": 2}}, "cpu": None},
        "security": {"authentication": "none", "remote_access": False},
        "feedback": {"persistence": {"enabled": True, "last_error": None}},
    }


class HealthCheckTests(unittest.TestCase):
    def run_check(self, health, model_rows=""):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            tools.mkdir()
            payload = root / "health.json"
            payload.write_text(json.dumps(health))
            programs = {
                "curl": f"#!{sys.executable}\nimport json,os,sys\nurl=sys.argv[-1]\nif url.endswith('/api/version'): print(json.dumps({{'version':'0.0.1'}}))\nelif url.endswith('/api/ps'): print(json.dumps({{'models':[]}}))\nelif url.endswith('/_freellama/v1/health'): print(open(os.environ['HEALTH_FIXTURE']).read())\nelse: sys.exit(22)\n",
                "ollama": "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'ollama version is 0.0.1'; else echo 'NAME ID SIZE MODIFIED'; printf '%s\\n' \"$HEALTH_MODEL_ROWS\"; fi\n",
                "df": "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted'\necho 'fixture 100000000 10000000 90000000 10% /'\n",
                "launchctl": "#!/bin/sh\nexit 0\n",
            }
            for name, program in programs.items():
                executable = tools / name
                executable.write_text(program)
                executable.chmod(0o755)
            (tools / "python3").symlink_to(sys.executable)
            env = {
                **os.environ,
                "PATH": f"{tools}{os.pathsep}/usr/bin{os.pathsep}/bin",
                "HEALTH_FIXTURE": str(payload),
                "HEALTH_MODEL_ROWS": model_rows,
                "FREELLAMA_CHECK_ROOT": str(root), "FREELLAMA_REPO": "", "FREELLAMA_AUTH_TOKEN_FILE": "",
                "OLLAMA_ENDPOINT": "http://fixture-ollama.invalid", "FREELLAMA_ENDPOINT": "http://fixture-gateway.invalid",
                "OLLAMA_HOST": "127.0.0.1:11434", "OLLAMA_NO_CLOUD": "1", "OLLAMA_KV_CACHE_TYPE": "q8_0",
                "OLLAMA_NUM_PARALLEL": "1", "OLLAMA_MAX_LOADED_MODELS": "1", "OLLAMA_MAX_QUEUE": "512", "OLLAMA_FLASH_ATTENTION": "1",
                "MIN_FREE_GB": "15",
            }
            return subprocess.run(["/bin/bash", str(CHECK)], env=env, cwd=root, text=True, capture_output=True, timeout=30, check=False)

    def test_current_loopback_health_passes(self):
        result = self.run_check(current_health())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("serve contracts are current", result.stdout)

    def test_ephemeral_feedback_warns_without_claiming_stale_contracts(self):
        health = current_health()
        health["feedback"]["persistence"]["enabled"] = False
        result = self.run_check(health)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("WARN", result.stdout)
        self.assertIn("ephemeral", result.stdout)
        self.assertNotIn("stale or incomplete", result.stdout)

    def test_resource_hold_warns_without_misreporting_service_health(self):
        health = current_health()
        health["admission"] = {"resources": {"holding": True, "reasons": ["active_swapping"]}}
        result = self.run_check(health)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("managed task execution is held: active_swapping", result.stdout)
        self.assertNotIn("nothing blocking", result.stdout)
        self.assertNotIn("stale or incomplete", result.stdout)

    def test_authenticated_remote_access_warns_about_ingress(self):
        health = current_health()
        health["security"] = {"remote_access": True, "authentication": "bearer"}
        result = self.run_check(health)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("WARN", result.stdout)
        self.assertIn("TLS", result.stdout)
        self.assertNotIn("stale or incomplete", result.stdout)

    def test_unauthenticated_remote_access_fails_as_security_error(self):
        health = current_health()
        health["security"]["remote_access"] = True
        result = self.run_check(health)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("remote access", result.stdout)
        self.assertIn("unauthenticated", result.stdout)
        self.assertNotIn("stale or incomplete", result.stdout)

    def test_each_required_contract_remains_a_hard_failure(self):
        for name in CONTRACTS:
            with self.subTest(contract=name):
                health = current_health()
                del health["contracts"][name]
                result = self.run_check(health)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("stale or incomplete", result.stdout)

    def test_missing_backend_ownership_or_admission_still_fails(self):
        for field in ["upstream", "admission"]:
            with self.subTest(field=field):
                health = current_health()
                del health["backends"]["gpu"][field]
                result = self.run_check(health)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("stale or incomplete", result.stdout)

    def test_model_metadata_age_distinguishes_two_and_three_months(self):
        rows = "recent:tag id2 1GB 2 months ago\nolder:tag id3 1GB 3 months ago\nyearly:tag id4 1GB 1 year ago"
        result = self.run_check(current_health(), rows)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("old model metadata: recent:tag", result.stdout)
        self.assertIn("old model metadata: older:tag", result.stdout)
        self.assertIn("old model metadata: yearly:tag", result.stdout)
        self.assertIn("2 model(s) have metadata timestamps at least 3 months old", result.stdout)
        self.assertIn("Metadata age does not establish when a model was last used", result.stdout)
        self.assertNotIn("ollama rm", result.stdout)


if __name__ == "__main__":
    unittest.main()
