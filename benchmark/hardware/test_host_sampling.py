"""Contracts for truthful, bounded host observations; no services or inference."""

import copy
import math
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest

import sample_host as host

VM = """Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free: 100.
Pages inactive: 200.
Pages speculative: 30.
File-backed pages: 220.
Pages purgeable: 180.
Pages occupied by compressor: 40.
Pages stored in compressor: 400.
Compressions: 1000.
Decompressions: 200.
Swapins: 10.
Swapouts: 20.
Pageins: 50.
Pageouts: 60.
"""
SYS = """hw.memsize: 16777216
hw.logicalcpu: 10
vm.loadavg: { 2.5 1.5 1.0 }
vm.swapusage: total = 2048.00M  used = 123.25M  free = 1924.75M
kern.memorystatus_vm_pressure_level: 1
kern.boottime: { sec = 1000, usec = 0 }
"""
GPU = '''+-o AGXAcceleratorG14X <class AGXAcceleratorG14X, id 0x401, registered>
  | "PerformanceStatistics" = {"Renderer Utilization %"=99,"Device Utilization %"=0}
  +-o Client <class AGXDeviceUserClient, id 0x999, active>
  | "PerformanceStatistics" = {"Device Utilization %"=99}
'''
PROC = "42 1 Sat Oct 3 00:09:57 2026 0:02.50 30.0 1024 /owned/server\n"


def command(argv, stdout="", stderr="", status="ok", at=None):
    at = time.monotonic() if at is None else at
    return {"argv": argv, "stdout": stdout, "stderr": stderr, "status": status,
            "exit_code": 0 if status == "ok" else 1, "elapsed_seconds": .01,
            "monotonic_started": at, "monotonic_finished": at + .01,
            "termination_confirmed": True}


def hung_collector(specs, duration, interval, timeout, checkpoint):
    host.write_report(checkpoint, {"schema_version":1,"status":"sampling","samples":[{"retained":True}],
                                  "capabilities":{"power":"unknown"},"errors":["prior source failure"]})
    time.sleep(30)


class HostSamplingTests(unittest.TestCase):
    def test_available_memory_avoids_overlap_and_compressor_is_physical(self):
        value = host.parse_memory(VM, SYS)
        self.assertEqual(value["available_memory_bytes"], 320 * 16384)
        self.assertEqual(value["reclaimable_memory_bytes"], 220 * 16384)
        self.assertEqual(value["compressor_physical_bytes"], 40 * 16384)
        self.assertEqual(value["compressor_logical_stored_bytes"], 400 * 16384)
        self.assertEqual(value["swap_used_bytes"], int(123.25 * 1024**2))
        self.assertEqual(value["memory_pressure"], "normal")
        self.assertEqual(value["load_average"], [2.5, 1.5, 1.0])

    def test_missing_required_page_counter_is_unknown_instead_of_zero(self):
        value = host.parse_memory(VM.replace("Pages purgeable: 180.\n", ""), SYS)
        self.assertIsNone(value["available_memory_bytes"])
        self.assertIsNone(host.parse_memory("", "")["available_memory_bytes"])
        self.assertIsNone(host.memory_bytes("NaNG"))
        self.assertEqual(host.memory_bytes("0M"), 0)

    def test_memory_counter_rates_reset_and_gauges_can_legitimately_decrease(self):
        before = host.parse_memory(VM, SYS)
        after = copy.deepcopy(before)
        after["counters_pages"]["Swapouts"] = 24
        after["compressor_physical_bytes"] -= 16384
        delta = host.memory_deltas(after, before, 2)
        self.assertEqual(delta["counters"]["Swapouts"]["pages_per_second"], 2)
        self.assertEqual(delta["counters"]["Swapouts"]["bytes_per_second"], 32768)
        self.assertEqual(delta["gauges"]["compressor_physical_bytes"]["bytes_per_second"], -8192)
        after["counters_pages"]["Swapouts"] = 1
        reset = host.memory_deltas(after, before, 2)["counters"]["Swapouts"]
        self.assertEqual(reset["reason"], "counter_reset")
        self.assertIsNone(reset["delta_pages"])
        self.assertIsNone(reset["bytes_per_second"])

    def test_reboot_and_page_size_changes_never_bridge_counter_samples(self):
        before = host.parse_memory(VM, SYS)
        for field, changed, reason in (("boot_time_unix_seconds", 2000, "host_boot_identity_unknown_or_changed"),
                                      ("page_size_bytes", 4096, "page_size_unknown_or_changed")):
            after = copy.deepcopy(before)
            after[field] = changed
            delta = host.memory_deltas(after, before, 1)["counters"]["Swapouts"]
            self.assertEqual(delta["reason"], reason)
            self.assertIsNone(delta["pages_per_second"])

    def test_driver_zero_is_valid_but_user_client_is_not_accelerator_activity(self):
        value = host.parse_gpu(GPU)
        self.assertEqual(len(value["devices"]), 1)
        self.assertEqual(value["devices"][0]["device"], "AGXAcceleratorG14X@0x401")
        self.assertEqual(value["devices"][0]["busy_percent"], 0)
        self.assertEqual(value["scope"], "observed_devices_all_processes")
        self.assertEqual(value["window"], "driver_defined")
        self.assertIn("external_GPU_clients", value["attribution"])

    def test_unavailable_or_invalid_gpu_values_are_not_idle(self):
        for raw in ("NaN", "-1", "101", '"31"', "N/A"):
            with self.subTest(raw=raw):
                value = host.parse_gpu(GPU.replace('"Device Utilization %"=0', f'"Device Utilization %"={raw}'))
                self.assertIsNone(value["devices"][0]["busy_percent"])
                self.assertIsNotNone(value["reason"])
        self.assertEqual(host.parse_gpu('"PerformanceStatistics" = {"Device Utilization %"=31}')["devices"], [])

    def test_no_recorded_thermal_status_is_unknown_and_numeric_throttle_is_observed(self):
        unknown = host.parse_thermal("Note: No thermal warning level has been recorded\n")
        self.assertIsNone(unknown["throttled"])
        self.assertEqual(host.parse_thermal("CPU_Speed_Limit = 100")["throttled"], False)
        self.assertEqual(host.parse_thermal("CPU_Speed_Limit = 50")["throttled"], True)
        self.assertIsNone(host.parse_thermal("CPU_Speed_Limit = 101")["throttled"])
        self.assertEqual(host.parse_thermal("Thermal Warning Level = 1")["throttled"], True)

    def test_power_estimates_have_units_scope_and_never_become_task_energy(self):
        value = host.parse_power("CPU Power: 1.5 W\nGPU Power: 0 mW\nThermal pressure: Nominal\n(100.0ms elapsed)\n")
        self.assertEqual(value["estimated_power_milliwatts"], {"CPU": 1500, "GPU": 0})
        self.assertEqual(value["window_seconds"], .1)
        self.assertEqual(value["thermal_pressure"], "Nominal")
        self.assertEqual(value["scope"], "host_subsystems_all_processes")
        self.assertIsNone(value["energy_joules"])
        self.assertIsNone(value["per_task_energy_joules"])
        self.assertEqual(host.parse_power("CPU Power: NaN mW")["estimated_power_milliwatts"], {})
        self.assertEqual(host.parse_power("Combined Power (CPU + GPU + ANE): 42 mW")["estimated_power_milliwatts"], {"Combined":42})

    def test_exact_process_identity_precedes_cpu_or_rss_acceptance(self):
        specs, identities = [host.ProcessSpec(42, "/owned/server")], {}
        before = host.parse_processes(PROC, specs, identities, {}, 10)[0]
        self.assertEqual(before["rss_bytes"], 1024**2)
        self.assertEqual(before["cpu_time_seconds"], 2.5)
        after = host.parse_processes(PROC.replace("0:02.50", "0:04.50"), specs, identities, {42: before}, 11)[0]
        self.assertEqual(after["cpu_percent_interval"], 200)
        mismatch = host.parse_processes(PROC.replace("/owned/server", "/other/server"), specs, identities, {}, 12)[0]
        self.assertEqual(mismatch["reason"], "executable_identity_mismatch")
        self.assertIsNone(mismatch["cpu_time_seconds"])
        self.assertIsNone(mismatch["rss_bytes"])
        reused = host.parse_processes(PROC.replace("00:09:57", "00:10:57"), specs, identities, {}, 12)[0]
        self.assertEqual(reused["reason"], "process_identity_changed")
        self.assertIsNone(reused["rss_bytes"])
        malformed = host.parse_processes(PROC.replace("2026", "garbage"), specs, identities, {}, 12)[0]
        self.assertEqual(malformed["reason"], "process_start_identity_unparseable")
        self.assertIsNone(malformed["rss_bytes"])

    def test_process_counter_reset_and_disappearance_stay_unknown(self):
        specs, identities = [host.ProcessSpec(42, "/owned/server")], {}
        before = host.parse_processes(PROC, specs, identities, {}, 10)[0]
        after = host.parse_processes(PROC.replace("0:02.50", "0:01.00"), specs, identities, {42: before}, 11)[0]
        self.assertIsNone(after["cpu_percent_interval"])
        self.assertEqual(after["interval_reason"], "cpu_counter_reset_or_invalid_interval")
        self.assertEqual(host.parse_processes("", specs, identities, {}, 12)[0]["reason"], "process_not_reported")
        self.assertEqual(host.cpu_seconds("1-02:03:04.50"), 93784.5)
        self.assertIsNone(host.cpu_seconds("0:NaN"))

    def test_unprivileged_power_failure_is_retained_and_not_retried_every_sample(self):
        calls = []
        def runner(argv, timeout, deadline):
            calls.append(argv)
            name = Path(argv[0]).name
            if name == "powermetrics":
                return command(argv, stderr="powermetrics must be invoked as the superuser", status="nonzero_exit")
            return command(argv, {"sysctl": SYS, "vm_stat": VM, "ioreg": GPU,
                                  "ps": PROC, "pmset": "No thermal warning level has been recorded"}.get(name, ""))
        report = host.sample_host([host.ProcessSpec(42, "/owned/server")], .35, .25, .1,
                                  platform_name="darwin", runner=runner)
        self.assertEqual(len(report["samples"]), 2)
        self.assertEqual(sum(Path(argv[0]).name == "powermetrics" for argv in calls), 1)
        self.assertEqual(report["capabilities"]["power"], "unknown")
        self.assertEqual(report["capabilities"]["thermal"], "unknown")
        self.assertIn("superuser", report["samples"][1]["power"]["unavailable_probe"]["stderr"])
        self.assertEqual(report["errors"][0]["source"], "powermetrics")
        self.assertTrue(all("sudo" not in argv for argv in calls))
        self.assertEqual(next(argv for argv in calls if Path(argv[0]).name == "ps")[3], "42")

    def test_unavailable_platform_never_starts_a_probe(self):
        def forbidden(*args):
            self.fail("unsupported platform must not invoke a command")
        report = host.sample_host([], .1, platform_name="linux", runner=forbidden)
        self.assertEqual(report["status"], "unsupported_platform")
        self.assertEqual(report["samples"], [])
        self.assertEqual(report["capabilities"]["power"], "unknown")

    def test_hung_owned_probe_is_bounded_and_error_is_preserved(self):
        started = time.monotonic()
        result = host.run_command([sys.executable, "-c", "import time; time.sleep(30)"], .08, started + .5)
        self.assertLess(time.monotonic() - started, .8)
        self.assertEqual(result["status"], "timeout")
        self.assertTrue(result["termination_confirmed"])
        self.assertIsNotNone(result["exit_code"])

    def test_output_cap_does_not_accept_partial_observations(self):
        result = host.run_command([sys.executable, "-c", "import os; os.write(1,b'x'*65536)"],
                                  1, time.monotonic() + 1, max_output_bytes=128)
        self.assertEqual(result["status"], "output_limit")
        self.assertEqual(len(result["stdout"]), 128)
        self.assertTrue(result["termination_confirmed"])

    def test_whole_budget_bounds_multiple_hung_sources_and_retains_partial_errors(self):
        def hung(argv, timeout, deadline):
            return host.run_command([sys.executable, "-c", "import time; time.sleep(30)"], timeout, deadline)
        started = time.monotonic()
        report = host.sample_host([], .16, .25, .1, platform_name="darwin", runner=hung)
        self.assertLess(time.monotonic() - started, .8)
        self.assertEqual(len(report["samples"]), 1)
        self.assertEqual(report["samples"][0]["commands"]["sysctl"]["status"], "timeout")
        self.assertEqual(report["samples"][0]["commands"]["ioreg"]["status"], "budget_exhausted")
        self.assertIsNone(report["samples"][0]["memory"]["available_memory_bytes"])
        self.assertGreater(len(report["errors"]), 1)

    def test_outer_worker_budget_contains_an_uncooperative_collector_and_retains_checkpoint(self):
        started = time.monotonic()
        report = host.run_bounded([], 1, worker=hung_collector)
        self.assertLess(time.monotonic() - started, 1.5)
        self.assertEqual(report["status"], "deadline_exceeded")
        self.assertTrue(report["samples"][0]["retained"])
        self.assertIn("prior source failure", report["errors"])
        self.assertTrue(any("whole sampling budget" in error for error in report["errors"]))

    def test_self_identity_uses_current_main_executable_instead_of_a_launcher_guess(self):
        spec = host.self_process_spec()
        self.assertEqual(spec.pid, os.getpid())
        self.assertTrue(os.path.isabs(spec.executable))
        if sys.platform == "darwin":
            query = host.run_command(["/bin/ps","-ww","-p",str(os.getpid()),"-o","comm="], .5, time.monotonic()+.5)
            self.assertEqual(query["status"],"ok")
            self.assertEqual(os.path.realpath(query["stdout"].strip()),spec.executable)

    def test_process_and_budget_validation_precede_commands(self):
        for invalid in ("0=/owned/server", "42=relative", "42", "-1=/owned/server"):
            with self.assertRaises(ValueError):
                host.process_spec(invalid)
        for duration in (0, 181, math.nan, math.inf):
            with self.assertRaises(ValueError):
                host.sample_host([], duration)
        with self.assertRaises(ValueError):
            host.sample_host([host.ProcessSpec(42, "/owned/server")] * 2, .1)

    def test_atomic_receipt_serializes_unknowns_and_preserves_all_samples(self):
        report = host.sample_host([], .1, platform_name="linux")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "host.json"
            host.write_report(path, report)
            self.assertEqual(__import__("json").loads(path.read_text())["status"], "unsupported_platform")
            self.assertEqual(list(Path(directory).glob("*.tmp-*")), [])


if __name__ == "__main__":
    unittest.main()
