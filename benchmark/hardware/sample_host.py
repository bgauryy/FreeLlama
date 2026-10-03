#!/usr/bin/env python3
"""Bounded, read-only macOS host observations; never starts or controls model services."""

from __future__ import annotations

import argparse
import ctypes
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import json
import math
import multiprocessing
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import tempfile
import time
from typing import Callable

MAX_DURATION_SECONDS = 180
MAX_OUTPUT_BYTES = 65536
MAX_PROCESSES = 16


@dataclass(frozen=True)
class ProcessSpec:
    pid: int
    executable: str

    def __post_init__(self):
        if not isinstance(self.pid, int) or isinstance(self.pid, bool) or not 0 < self.pid <= 2**31 - 1:
            raise ValueError("process PID must be a positive integer")
        if not isinstance(self.executable, str) or not os.path.isabs(self.executable) or "\x00" in self.executable:
            raise ValueError("process executable must be an absolute path")


def self_process_spec() -> ProcessSpec:
    # Python framework launchers can differ from the main Mach-O image. Use dyld's
    # own documented executable-path API, then resolve symbolic links for comparison.
    executable = sys.executable
    if sys.platform == "darwin":
        size = ctypes.c_uint32(4096)
        path = ctypes.create_string_buffer(size.value)
        function = ctypes.CDLL(None)._NSGetExecutablePath
        function.argtypes = [ctypes.c_char_p, ctypes.POINTER(ctypes.c_uint32)]
        function.restype = ctypes.c_int
        if function(path, ctypes.byref(size)) != 0:
            if size.value > MAX_OUTPUT_BYTES:
                raise ValueError("current executable path exceeds the bounded buffer")
            path = ctypes.create_string_buffer(size.value)
            if function(path, ctypes.byref(size)) != 0:
                raise ValueError("dyld could not report the current executable path")
        executable = os.fsdecode(path.value)
    return ProcessSpec(os.getpid(), os.path.realpath(executable))


def process_spec(value: str) -> ProcessSpec:
    pid, separator, executable = value.partition("=")
    if not separator:
        raise ValueError("process must have the form PID=/absolute/executable")
    return ProcessSpec(int(pid), executable)


def run_command(argv: list[str], timeout_seconds: float, deadline: float,
                max_output_bytes: int = MAX_OUTPUT_BYTES) -> dict:
    """Cap time and both output streams. Terminate only this owned probe child on failure."""
    started = time.monotonic()
    timeout = min(timeout_seconds, deadline - started)
    result = {"argv": argv, "status": "budget_exhausted", "exit_code": None,
              "stdout": "", "stderr": "", "elapsed_seconds": 0.0,
              "monotonic_started": started, "monotonic_finished": started,
              "termination_confirmed": None}
    if timeout <= 0:
        return result
    child = None
    streams = {"stdout": bytearray(), "stderr": bytearray()}
    try:
        child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, start_new_session=True,
                                 env={**os.environ, "LC_ALL": "C"})
        stop = min(deadline, started + timeout)
        with selectors.DefaultSelector() as selector:
            for name in streams:
                pipe = getattr(child, name)
                os.set_blocking(pipe.fileno(), False)
                selector.register(pipe, selectors.EVENT_READ, name)
            result["status"] = "ok"
            while selector.get_map():
                remaining = stop - time.monotonic()
                if remaining <= 0:
                    result["status"] = "timeout"
                    break
                for key, _ in selector.select(remaining):
                    data = os.read(key.fileobj.fileno(), 8192)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    target = streams[key.data]
                    space = max_output_bytes - len(target)
                    target.extend(data[:space])
                    if len(data) > space:
                        result["status"] = "output_limit"
                        break
                if result["status"] != "ok":
                    break
            if result["status"] == "ok":
                try:
                    child.wait(timeout=max(.001, stop - time.monotonic()))
                except subprocess.TimeoutExpired:
                    result["status"] = "timeout"
        if result["status"] != "ok" and child.poll() is None:
            child.kill()
        try:
            child.wait(timeout=.2)
        except subprocess.TimeoutExpired:
            result["termination_confirmed"] = False
        else:
            result["termination_confirmed"] = True
        result["exit_code"] = child.returncode
        if result["status"] == "ok" and child.returncode != 0:
            result["status"] = "nonzero_exit"
    except (OSError, ValueError) as error:
        result["status"] = "spawn_or_read_error"
        result["stderr"] = f"{type(error).__name__}: {error}"
        if child is not None and child.poll() is None:
            child.kill()
            try:
                child.wait(timeout=.2)
            except subprocess.TimeoutExpired:
                result["termination_confirmed"] = False
    finally:
        if child is not None:
            for name in streams:
                getattr(child, name).close()
        for name, data in streams.items():
            result[name] += data.decode("utf-8", errors="replace")
        result["monotonic_finished"] = time.monotonic()
        result["elapsed_seconds"] = result["monotonic_finished"] - started
    return result


def finite(value, minimum=0.0, maximum=None):
    try:
        number = float(value)
    except (TypeError, ValueError):
        return None
    return number if math.isfinite(number) and number >= minimum and (maximum is None or number <= maximum) else None


def memory_bytes(text: str | None):
    if text is None:
        return None
    match = re.fullmatch(r"([0-9]+(?:\.[0-9]+)?)([BKMG]?)", text.strip())
    if not match:
        return None
    try:
        return int(Decimal(match[1]) * {"": 1, "B": 1, "K": 1024, "M": 1024**2, "G": 1024**3}[match[2]])
    except InvalidOperation:
        return None


def parse_memory(vmstat: str, sysctl: str) -> dict:
    page_match = re.search(r"page size of ([0-9]+) bytes", vmstat)
    page_size = int(page_match[1]) if page_match and int(page_match[1]) > 0 else None
    pages = {key.strip(): int(value) for key, value in re.findall(r"^([^:\n]+):\s*([0-9]+)\.?\s*$", vmstat, re.M)}
    system = dict(line.split(":", 1) for line in sysctl.splitlines() if ":" in line)
    system = {key.strip(): value.strip() for key, value in system.items()}

    def integer(name):
        value = system.get(name, "")
        return int(value) if value.isdigit() and int(value) > 0 else None

    def byte_count(name):
        return pages[name] * page_size if page_size and name in pages else None

    total = integer("hw.memsize")
    free = byte_count("Pages free")
    inactive, speculative, file_backed, purgeable = (byte_count(key) for key in
        ("Pages inactive", "Pages speculative", "File-backed pages", "Pages purgeable"))
    reclaimable = max(min(inactive + speculative, file_backed), purgeable) if all(
        value is not None for value in (inactive, speculative, file_backed, purgeable)) else None
    available = free + reclaimable if free is not None and reclaimable is not None else None
    if available is not None and total is not None:
        available = min(available, total)
    swap_match = re.search(r"\bused\s*=\s*([0-9.]+[BKMG]?)", system.get("vm.swapusage", ""))
    boot_match = re.search(r"\bsec\s*=\s*([0-9]+)", system.get("kern.boottime", ""))
    load = [finite(value) for value in system.get("vm.loadavg", "").strip("{} ").split()]
    return {"source": "macos_vm_stat_sysctl", "scope": "whole_host", "page_size_bytes": page_size,
            "boot_time_unix_seconds": int(boot_match[1]) if boot_match else None,
            "total_memory_bytes": total, "free_memory_bytes": free,
            "reclaimable_memory_bytes": reclaimable, "available_memory_bytes": available,
            "available_memory_kind": "free_plus_bounded_reclaimable_estimate",
            "compressor_physical_bytes": byte_count("Pages occupied by compressor"),
            "compressor_logical_stored_bytes": byte_count("Pages stored in compressor"),
            "swap_used_bytes": memory_bytes(swap_match[1]) if swap_match else None,
            "memory_pressure": {"1": "normal", "2": "warning", "4": "critical"}.get(system.get("kern.memorystatus_vm_pressure_level")),
            "logical_cpus": integer("hw.logicalcpu"),
            "load_average": load if len(load) == 3 and all(value is not None for value in load) else None,
            "counters_pages": {key: pages.get(key) for key in
                ("Compressions", "Decompressions", "Swapins", "Swapouts", "Pageins", "Pageouts")}}


def parse_thermal(text: str) -> dict:
    speed = re.search(r"CPU_Speed_Limit\s*=\s*([0-9]+)", text)
    warning = re.search(r"Thermal Warning Level\s*=\s*([0-9]+)", text)
    limit = finite(speed[1], maximum=100) if speed else None
    level = int(warning[1]) if warning else None
    throttled = True if level is not None and level > 0 else (
        limit < 100 if limit is not None else (False if level == 0 else None))
    return {"source": "pmset_g_therm", "scope": "whole_host", "cpu_speed_limit_percent": limit,
            "warning_level": level, "throttled": throttled,
            "reason": None if throttled is not None else "no_valid_thermal_status_reported"}


def parse_gpu(text: str) -> dict:
    devices, device = {}, None
    for line in text.splitlines():
        if "+-o " in line:
            header = re.search(r"<class (AGXAccelerator[^,]*),\s*id ([^,]+)", line)
            device = f"{header[1]}@{header[2].strip()}" if header else None
            continue
        if not device:
            continue
        stats = re.fullmatch(r'"PerformanceStatistics"\s*=\s*\{(.*)\}', line.lstrip(" |\t"))
        if not stats:
            continue
        busy = re.search(r'"Device Utilization %"\s*=\s*([^,}]+)', stats[1])
        value = finite(busy[1], maximum=100) if busy else None
        devices[device] = {"device": device, "busy_percent": value, "memory_busy_percent": None,
                           "reason": None if value is not None else "device_busy_counter_unavailable_or_invalid"}
    return {"source": "apple_ioreg_performance_statistics", "scope": "observed_devices_all_processes",
            "window": "driver_defined", "devices": list(devices.values()),
            "attribution": "cannot_attribute_external_GPU_clients_or_individual_tasks",
            "reason": None if any(item["busy_percent"] is not None for item in devices.values()) else "no_valid_device_busy_counter_reported"}


def parse_power(text: str) -> dict:
    power = {}
    for label, raw, unit in re.findall(r"^(CPU|GPU|ANE|Combined) Power(?: \([^\n:]+\))?:\s*([0-9.]+)\s*(mW|W)\s*$", text, re.M):
        value = finite(raw)
        if value is not None:
            power[label] = value * (1000 if unit == "W" else 1)
    interval = re.search(r"\(([0-9.]+)ms elapsed\)", text)
    thermal = re.search(r"^Thermal pressure:\s*([A-Za-z ]{1,64})\s*$", text, re.M)
    return {"source": "powermetrics", "scope": "host_subsystems_all_processes",
            "estimated_power_milliwatts": power,
            "window_seconds": finite(interval[1]) / 1000 if interval and finite(interval[1]) is not None else None,
            "thermal_pressure": thermal[1].strip() if thermal else None,
            "energy_joules": None, "energy_reason": "no_energy_counter_reported",
            "per_task_energy_joules": None, "attribution": "not_task_attributable",
            "reason": None if power or thermal else "no_valid_power_or_thermal_reading_reported"}


def cpu_seconds(text: str):
    match = re.fullmatch(r"(?:(\d+)-)?(?:(\d+):)?(\d+):(\d+(?:\.\d+)?)", text)
    if not match:
        return None
    days, hours, minutes, seconds = match.groups()
    seconds = finite(seconds, maximum=59.999999)
    return None if seconds is None else int(days or 0) * 86400 + int(hours or 0) * 3600 + int(minutes) * 60 + seconds


def parse_processes(text: str, specs: list[ProcessSpec], identities: dict[int, tuple], previous: dict,
                    observed_monotonic: float) -> list[dict]:
    rows = {}
    for line in text.splitlines():
        fields = line.split(None, 10)
        if len(fields) == 11 and fields[0].isdigit():
            rows[int(fields[0])] = fields
    observations = []
    for spec in specs:
        row = rows.get(spec.pid)
        result = {"pid": spec.pid, "expected_executable": spec.executable,
                  "scope": "exact_process_excludes_children", "status": "unknown",
                  "identity_resolution": "pid_executable_ps_lstart_second_precision",
                  "cpu_percent_ps": None, "cpu_percent_ps_window": "ps_decaying_average",
                  "cpu_time_seconds": None, "cpu_percent_interval": None,
                  "cpu_percent_scale": "100_percent_is_one_logical_cpu", "rss_bytes": None,
                  "rss_kind": "ps_resident_set_not_physical_footprint", "observed_monotonic": observed_monotonic}
        if row is None:
            result["reason"] = "process_not_reported"
        else:
            start, executable = " ".join(row[2:7]), row[10]
            identity = (os.path.realpath(executable), start)
            result["observed_identity"] = {"executable": executable, "started": start, "ppid": row[1]}
            try:
                datetime.strptime(start, "%a %b %d %H:%M:%S %Y")
            except ValueError:
                result["reason"] = "process_start_identity_unparseable"
                observations.append(result)
                continue
            if os.path.realpath(spec.executable) != identity[0]:
                result["reason"] = "executable_identity_mismatch"
            elif spec.pid in identities and identities[spec.pid] != identity:
                result["reason"] = "process_identity_changed"
            else:
                identities.setdefault(spec.pid, identity)
                cpu, pct = cpu_seconds(row[7]), finite(row[8])
                rss = int(row[9]) * 1024 if row[9].isdigit() else None
                result.update(status="observed", reason=None, cpu_time_seconds=cpu,
                              cpu_percent_ps=pct, rss_bytes=rss)
                before = previous.get(spec.pid)
                if before and cpu is not None and before["cpu_time_seconds"] is not None:
                    elapsed = observed_monotonic - before["observed_monotonic"]
                    delta = cpu - before["cpu_time_seconds"]
                    if elapsed > 0 and delta >= 0:
                        result["cpu_percent_interval"] = delta / elapsed * 100
                    else:
                        result["interval_reason"] = "cpu_counter_reset_or_invalid_interval"
                else:
                    result["interval_reason"] = "no_previous_comparable_process_sample"
        observations.append(result)
    return observations


def memory_deltas(current: dict, previous: dict | None, elapsed: float | None) -> dict:
    reason = None
    if previous is None or elapsed is None or elapsed <= 0:
        reason = "no_previous_comparable_host_sample"
    elif current["boot_time_unix_seconds"] is None or current["boot_time_unix_seconds"] != previous["boot_time_unix_seconds"]:
        reason = "host_boot_identity_unknown_or_changed"
    elif current["page_size_bytes"] is None or current["page_size_bytes"] != previous["page_size_bytes"]:
        reason = "page_size_unknown_or_changed"
    result = {"interval_seconds": elapsed, "counters": {}, "gauges": {}}
    for key, value in current["counters_pages"].items():
        old = previous["counters_pages"].get(key) if previous else None
        error = reason or ("counter_unavailable" if value is None or old is None else
                           ("counter_reset" if value < old else None))
        delta = value - old if error is None else None
        result["counters"][key] = {"delta_pages": delta,
            "pages_per_second": delta / elapsed if delta is not None else None,
            "bytes_per_second": delta * current["page_size_bytes"] / elapsed if delta is not None else None,
            "reason": error}
    for key in ("compressor_physical_bytes", "swap_used_bytes"):
        value, old = current[key], previous[key] if previous else None
        error = reason or ("gauge_unavailable" if value is None or old is None else None)
        delta = value - old if error is None else None
        result["gauges"][key] = {"delta_bytes": delta, "bytes_per_second": delta / elapsed if delta is not None else None, "reason": error}
    return result


def command_metadata(result: dict) -> dict:
    return {key: value for key, value in result.items() if key not in ("stdout", "stderr")} | {
        "stderr": result["stderr"][:4096],
        "stdout_error_excerpt": result["stdout"][:512] if result["status"] != "ok" else None}


def sample_host(specs: list[ProcessSpec], duration_seconds=5.0, interval_seconds=1.0,
                command_timeout_seconds=1.0, platform_name=None, runner: Callable = run_command,
                checkpoint: Callable | None = None) -> dict:
    if not math.isfinite(duration_seconds) or not 0 < duration_seconds <= MAX_DURATION_SECONDS:
        raise ValueError("duration must be positive and at most 180 seconds")
    if not math.isfinite(interval_seconds) or interval_seconds < .25:
        raise ValueError("interval must be finite and at least .25 seconds")
    if not math.isfinite(command_timeout_seconds) or not 0 < command_timeout_seconds <= 5:
        raise ValueError("command timeout must be positive and at most 5 seconds")
    if len(specs) > MAX_PROCESSES or len({spec.pid for spec in specs}) != len(specs):
        raise ValueError("at most 16 distinct operator-owned process PIDs may be supplied")
    platform_name = platform_name or sys.platform
    started, wall_started = time.monotonic(), datetime.now(timezone.utc).isoformat()
    deadline = started + duration_seconds
    report = {"schema_version": 1, "platform": platform_name, "started_utc": wall_started,
              "scope": "read_only_host_and_exact_operator_owned_processes",
              "sampling": "sequential_non_atomic_source_observations", "status": "complete",
              "configuration": {"duration_seconds": duration_seconds, "interval_seconds": interval_seconds,
                  "command_timeout_seconds": command_timeout_seconds, "maximum_output_bytes_per_stream": MAX_OUTPUT_BYTES,
                  "owned_processes": [{"pid": item.pid, "executable": item.executable} for item in specs]},
              "capabilities": {key: "unknown" for key in
                  ("memory", "processes", "gpu_busy", "thermal", "power", "energy_joules", "per_task_energy")},
              "samples": [], "errors": []}
    if checkpoint:
        checkpoint(report)
    if platform_name != "darwin":
        report.update(status="unsupported_platform", elapsed_seconds=time.monotonic() - started)
        report["errors"].append("only macOS read-only sources are implemented")
        if checkpoint:
            checkpoint(report)
        return report
    identities, previous_processes, previous_memory, previous_vm_time = {}, {}, None, None
    power_available, power_failure = None, None
    while time.monotonic() < deadline:
        sample_started = time.monotonic()
        commands = {}
        def query(name, argv):
            result = runner(argv, command_timeout_seconds, deadline)
            commands[name] = command_metadata(result)
            if result["status"] != "ok":
                report["errors"].append({"sample": len(report["samples"]), "source": name,
                    "status": result["status"], "stderr": result["stderr"][:4096]})
            return result
        system = query("sysctl", ["/usr/sbin/sysctl", "hw.memsize", "hw.logicalcpu", "vm.loadavg",
                                  "vm.swapusage", "kern.memorystatus_vm_pressure_level", "kern.boottime"])
        vm = query("vm_stat", ["/usr/bin/vm_stat"])
        thermal = query("pmset", ["/usr/bin/pmset", "-g", "therm"])
        gpu = query("ioreg", ["/usr/sbin/ioreg", "-r", "-c", "AGXAccelerator", "-l", "-d", "1", "-w", "0"])
        # sysctl can return usable fields alongside an unavailable optional key's nonzero exit.
        system_text = system["stdout"] if system["status"] in ("ok", "nonzero_exit") else ""
        memory = parse_memory(vm["stdout"] if vm["status"] == "ok" else "", system_text)
        vm_time = (vm["monotonic_started"] + vm["monotonic_finished"]) / 2
        processes = []
        if specs:
            proc = query("ps", ["/bin/ps", "-ww", "-p", ",".join(str(item.pid) for item in specs),
                                "-o", "pid=,ppid=,lstart=,time=,%cpu=,rss=,comm="])
            proc_time = (proc["monotonic_started"] + proc["monotonic_finished"]) / 2
            processes = parse_processes(proc["stdout"] if proc["status"] == "ok" else "",
                                        specs, identities, previous_processes, proc_time)
            previous_processes = {item["pid"]: item for item in processes if item["status"] == "observed"}
        power = parse_power("")
        if power_available is not False:
            energy = query("powermetrics", ["/usr/bin/powermetrics", "--samplers", "cpu_power,gpu_power,thermal",
                                            "-n", "1", "-i", "100"])
            power = parse_power(energy["stdout"] if energy["status"] == "ok" else "")
            power_available = bool(power["estimated_power_milliwatts"] or power["thermal_pressure"])
            if not power_available:
                power_failure = {"status": energy["status"], "stderr": energy["stderr"][:4096], "reason": power["reason"]}
        if power_available is False:
            power["unavailable_probe"] = power_failure
        sample = {"timestamp_utc": datetime.now(timezone.utc).isoformat(),
                  "elapsed_seconds": time.monotonic() - started, "commands": commands,
                  "memory": memory, "memory_interval": memory_deltas(memory, previous_memory,
                       vm_time - previous_vm_time if previous_vm_time is not None else None),
                  "thermal": parse_thermal(thermal["stdout"] if thermal["status"] == "ok" else ""),
                  "gpu": parse_gpu(gpu["stdout"] if gpu["status"] == "ok" else ""),
                  "power": power, "processes": processes}
        previous_memory, previous_vm_time = memory, vm_time
        report["samples"].append(sample)
        report["capabilities"] = {"memory": "observed" if memory["available_memory_bytes"] is not None else "unknown",
            "processes": "observed" if processes and all(item["status"] == "observed" for item in processes) else "unknown",
            "gpu_busy": "observed" if sample["gpu"]["reason"] is None else "unknown",
            "thermal": "observed" if sample["thermal"]["throttled"] is not None or power["thermal_pressure"] else "unknown",
            "power": "observed_estimate" if power["estimated_power_milliwatts"] else "unknown",
            "energy_joules": "unknown", "per_task_energy": "unknown"}
        report["elapsed_seconds"] = time.monotonic() - started
        if checkpoint:
            checkpoint(report)
        remaining = min(deadline - time.monotonic(), interval_seconds - (time.monotonic() - sample_started))
        if remaining > 0:
            time.sleep(remaining)
    report["elapsed_seconds"] = time.monotonic() - started
    report["cleanup_bound_seconds_per_timed_out_probe"] = .2
    if checkpoint:
        checkpoint(report)
    return report


def write_report(path: Path, report: dict):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + f".tmp-{os.getpid()}")
    temporary.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def _sample_worker(specs, duration_seconds, interval_seconds, command_timeout_seconds, checkpoint):
    sample_host(specs, duration_seconds, interval_seconds, command_timeout_seconds,
                checkpoint=lambda report: write_report(checkpoint, report))


def run_bounded(specs, duration_seconds=5.0, interval_seconds=1.0,
                command_timeout_seconds=1.0, worker=_sample_worker) -> dict:
    # Validate before starting a worker. The unsupported route invokes no host commands.
    if not math.isfinite(duration_seconds) or not 0 < duration_seconds <= MAX_DURATION_SECONDS:
        raise ValueError("duration must be positive and at most 180 seconds")
    if not math.isfinite(interval_seconds) or interval_seconds < .25:
        raise ValueError("interval must be finite and at least .25 seconds")
    if not math.isfinite(command_timeout_seconds) or not 0 < command_timeout_seconds <= 5:
        raise ValueError("command timeout must be positive and at most 5 seconds")
    if len(specs) > MAX_PROCESSES or len({spec.pid for spec in specs}) != len(specs):
        raise ValueError("at most 16 distinct operator-owned process PIDs may be supplied")
    started = time.monotonic()
    report = {"schema_version": 1, "platform": sys.platform, "status": "worker_not_started",
              "samples": [], "errors": [], "capabilities": {key: "unknown" for key in
              ("memory", "processes", "gpu_busy", "thermal", "power", "energy_joules", "per_task_energy")}}
    with tempfile.TemporaryDirectory(prefix="freellama-host-sampling-") as directory:
        checkpoint = Path(directory) / "host.json"
        write_report(checkpoint, report)
        process = multiprocessing.get_context("spawn").Process(target=worker,
            args=(specs, max(.001, duration_seconds - .6), interval_seconds, command_timeout_seconds, checkpoint))
        try:
            process.start()
        except (OSError, RuntimeError) as error:
            report["errors"].append(f"sampler worker could not start: {type(error).__name__}: {error}")
        else:
            # Leave .3s inside the declared budget for owned-worker cleanup and receipt readback.
            process.join(max(0, duration_seconds - .3 - (time.monotonic() - started)))
            timed_out = process.is_alive()
            if timed_out:
                process.terminate()
                process.join(.1)
                if process.is_alive():
                    process.kill()
                    process.join(.1)
            try:
                if checkpoint.stat().st_size > 64 * 1024**2:
                    raise ValueError("sampler checkpoint exceeds 64 MiB")
                report = json.loads(checkpoint.read_text(encoding="utf-8"))
            except (OSError, ValueError) as error:
                report["errors"].append(f"sampler checkpoint unavailable: {type(error).__name__}: {error}")
            if timed_out:
                report["status"] = "deadline_exceeded"
                report["errors"].append("whole sampling budget exhausted; owned worker terminated; partial observations retained")
            elif process.exitcode != 0:
                report["status"] = "worker_failed"
                report["errors"].append(f"sampler worker exited with code {process.exitcode}")
            if process.is_alive():
                report["errors"].append(f"owned sampler worker {process.pid} termination not confirmed")
                report["worker_cleanup_confirmed"] = False
            else:
                report["worker_cleanup_confirmed"] = True
                process.close()
    report["whole_budget_seconds"] = duration_seconds
    report["elapsed_seconds_including_worker_startup"] = time.monotonic() - started
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--process", action="append", default=[], help="operator-owned PID=/absolute/executable; repeat for exact PIDs")
    parser.add_argument("--self", action="store_true", help="include this sampler's own PID")
    parser.add_argument("--duration-seconds", type=float, default=5)
    parser.add_argument("--interval-seconds", type=float, default=1)
    parser.add_argument("--command-timeout-seconds", type=float, default=1)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        specs = [process_spec(value) for value in args.process]
        if args.self:
            specs.append(self_process_spec())
        # The worker's observation window leaves time for bounded startup/teardown inside the parent budget.
        report = run_bounded(specs, args.duration_seconds, args.interval_seconds, args.command_timeout_seconds)
        if args.output:
            write_report(args.output, report)
    except ValueError as error:
        parser.error(str(error))
    if not args.output:
        print(json.dumps(report, indent=2, allow_nan=False))
    return 0 if report["status"] == "complete" else 1


if __name__ == "__main__":
    raise SystemExit(main())
