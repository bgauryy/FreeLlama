//! Host resource sampling: Linux /proc and cgroups, NVIDIA, and macOS `vm_stat/sysctl`, behind bounded commands.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::HostResources;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use super::MemoryPressure;

/// Collect one bounded, unprivileged observation; unsupported fields stay `None`.
#[must_use]
pub fn sample_host_resources() -> HostResources {
    #[cfg(target_os = "macos")]
    {
        sample_macos()
    }
    #[cfg(target_os = "linux")]
    {
        sample_linux()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        HostResources {
            source: "unsupported_platform".into(),
            unavailable: vec!["host_resource_telemetry".into()],
            ..HostResources::default()
        }
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_linux(
    meminfo: &str,
    loadavg: &str,
    vmstat: &str,
    cpus: Option<u32>,
) -> HostResources {
    let values = colon_numbers(meminfo);
    let bytes = |name| values.get(name).and_then(|value| value.checked_mul(1024));
    let total = bytes("MemTotal");
    let available = bytes("MemAvailable").filter(|value| total.is_none_or(|total| *value <= total));
    let free = bytes("MemFree");
    let swap_used = bytes("SwapTotal")
        .zip(bytes("SwapFree"))
        .and_then(|(total, free)| total.checked_sub(free));
    HostResources {
        source: "linux_proc".into(),
        total_memory_bytes: total,
        available_memory_bytes: available,
        available_memory_kind: "kernel_mem_available_estimate".into(),
        free_memory_bytes: free,
        reclaimable_memory_bytes: available
            .zip(free)
            .map(|(available, free)| available.saturating_sub(free)),
        occupied_memory_bytes: total
            .zip(available)
            .map(|(total, available)| total.saturating_sub(available)),
        compressor_bytes: bytes("Zswap"),
        swap_used_bytes: swap_used,
        swap_out_pages: vmstat.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next()? == "pswpout")
                .then(|| fields.next()?.parse().ok())
                .flatten()
        }),
        load_average_one_minute: loadavg
            .split_whitespace()
            .next()
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|load| load.is_finite() && *load >= 0.0),
        logical_cpus: cpus.filter(|cpus| *cpus > 0),
        unavailable: vec!["thermal_pressure".into()],
        ..HostResources::default()
    }
}

/// Apply the process's cgroup v2 memory limit: inside a container `MemAvailable` describes the
/// host, so admission could approve a load the container's OOM killer then ends.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn apply_cgroup_limit(observed: &mut HostResources, max: &str, current: &str) {
    let (Ok(limit), Ok(used)) = (max.trim().parse::<u64>(), current.trim().parse::<u64>()) else {
        return; // "max" = unlimited, or unreadable
    };
    if observed
        .total_memory_bytes
        .is_some_and(|total| limit >= total)
    {
        return;
    }
    let headroom = limit.saturating_sub(used);
    observed.cgroup_memory_limit_bytes = Some(limit);
    observed.total_memory_bytes = Some(
        observed
            .total_memory_bytes
            .map_or(limit, |total| total.min(limit)),
    );
    observed.available_memory_bytes = Some(
        observed
            .available_memory_bytes
            .map_or(headroom, |available| available.min(headroom)),
    );
    observed.available_memory_kind = "cgroup_v2_limit_minus_usage".into();
}

/// Linux pressure-stall information as an OS memory-pressure level. `some` counts time at least
/// one task stalled on memory; `full` counts time all non-idle tasks stalled (thrashing).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_memory_psi(text: &str) -> Option<(f64, MemoryPressure)> {
    let avg10 = |kind: &str| {
        text.lines()
            .find(|line| line.starts_with(kind))?
            .split_whitespace()
            .find_map(|field| field.strip_prefix("avg10="))?
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)
    };
    let some = avg10("some")?;
    let full = avg10("full").unwrap_or(0.0);
    let level = if full >= 10.0 {
        MemoryPressure::Critical
    } else if some >= 10.0 || full >= 2.0 {
        MemoryPressure::Warning
    } else {
        MemoryPressure::Normal
    };
    Some((some, level))
}

/// NVIDIA CSV's first two fields are memory.total,memory.used (MiB per GPU); optional activity
/// fields follow them without changing the memory accounting contract.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_nvidia_smi(text: &str) -> Option<(u64, u64)> {
    const MIB: u64 = 1024 * 1024;
    let mut total = 0_u64;
    let mut used = 0_u64;
    let mut devices = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line
            .split(',')
            .map(|field| field.trim().parse::<u64>().ok());
        let (Some(Some(device_total)), Some(Some(device_used))) = (fields.next(), fields.next())
        else {
            return None;
        };
        total = total.checked_add(device_total.checked_mul(MIB)?)?;
        used = used.checked_add(device_used.checked_mul(MIB)?)?;
        devices += 1;
    }
    (devices > 0).then_some((total, total.saturating_sub(used)))
}

#[cfg(target_os = "linux")]
fn sample_gpu_linux(observed: &mut HostResources, read: impl Fn(&str) -> Option<String>) {
    let nvidia = bounded_command_with(
        "nvidia-smi",
        &[
            "--query-gpu=memory.total,memory.used,utilization.gpu,utilization.memory,uuid",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_millis(1500),
    );
    observed.gpu_activity = nvidia
        .as_deref()
        .and_then(super::super::device_activity::parse_nvidia_activity);
    if let Some((total, free)) = nvidia.as_deref().and_then(parse_nvidia_smi) {
        observed.gpu_memory_total_bytes = Some(total);
        observed.gpu_memory_free_bytes = Some(free);
        observed.gpu_telemetry_source = Some("nvidia_smi".into());
        return;
    }
    // amdgpu exposes VRAM counters per DRM card; integrated APUs report a small carve-out.
    // AMD activity is not collected by this slice and remains unknown.
    let mut total = 0_u64;
    let mut used = 0_u64;
    for card in 0..8 {
        let base = format!("/sys/class/drm/card{card}/device");
        let (Some(card_total), Some(card_used)) = (
            read(&format!("{base}/mem_info_vram_total"))
                .and_then(|text| text.trim().parse::<u64>().ok()),
            read(&format!("{base}/mem_info_vram_used"))
                .and_then(|text| text.trim().parse::<u64>().ok()),
        ) else {
            continue;
        };
        total = total.saturating_add(card_total);
        used = used.saturating_add(card_used);
    }
    if total > 0 {
        observed.gpu_memory_total_bytes = Some(total);
        observed.gpu_memory_free_bytes = Some(total.saturating_sub(used));
        observed.gpu_telemetry_source = Some("amdgpu_sysfs".into());
    }
}

#[cfg(target_os = "linux")]
fn sample_linux() -> HostResources {
    let read = |path: &str| {
        std::fs::File::open(path).ok().and_then(|file| {
            let mut text = String::new();
            file.take(64 * 1024).read_to_string(&mut text).ok()?;
            Some(text)
        })
    };
    let mut observed = parse_linux(
        &read("/proc/meminfo").unwrap_or_default(),
        &read("/proc/loadavg").unwrap_or_default(),
        &read("/proc/vmstat").unwrap_or_default(),
        std::thread::available_parallelism()
            .ok()
            .and_then(|cpus| u32::try_from(cpus.get()).ok()),
    );
    if let Some(path) = read("/proc/self/cgroup").and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("0::").map(|path| path.trim().to_owned()))
    }) {
        let base = format!(
            "/sys/fs/cgroup{}",
            if path == "/" { "" } else { path.as_str() }
        );
        if let (Some(max), Some(current)) = (
            read(&format!("{base}/memory.max")),
            read(&format!("{base}/memory.current")),
        ) {
            apply_cgroup_limit(&mut observed, &max, &current);
        }
    }
    match read("/proc/pressure/memory")
        .as_deref()
        .and_then(parse_memory_psi)
    {
        Some((some, level)) => {
            observed.memory_psi_some_avg10 = Some(some);
            observed.memory_pressure = Some(level);
        }
        None => observed.unavailable.push("os_memory_pressure_level".into()),
    }
    sample_gpu_linux(&mut observed, read);
    if observed.available_memory_bytes.is_none() {
        observed.unavailable.push("MemAvailable".into());
    }
    observed
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn colon_numbers(text: &str) -> BTreeMap<&str, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let value = value
                .split_whitespace()
                .next()?
                .trim_end_matches('.')
                .parse()
                .ok()?;
            Some((key.trim(), value))
        })
        .collect()
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_macos(vmstat: &str, sysctl: &str, thermal: &str) -> HostResources {
    let values = colon_numbers(vmstat);
    let sys: BTreeMap<_, _> = sysctl
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect();
    let page_size = vmstat
        .split("page size of ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size > 0);
    let bytes = |name| {
        values
            .get(name)
            .zip(page_size)
            .and_then(|(pages, size)| pages.checked_mul(size))
    };
    let total = sys
        .get("hw.memsize")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    let free = bytes("Pages free");
    // vm_stat subtracts speculative pages from its printed free count. File-backed, inactive,
    // speculative and purgeable counters overlap; summing all of them would inflate headroom.
    // Use a bounded reclaimable estimate, explicitly distinct from Linux's kernel MemAvailable.
    let file_reclaimable = bytes("Pages inactive")
        .zip(bytes("Pages speculative"))
        .and_then(|(inactive, speculative)| inactive.checked_add(speculative))
        .zip(bytes("File-backed pages"))
        .map(|(inactive_and_speculative, file)| inactive_and_speculative.min(file));
    let reclaimable = file_reclaimable
        .zip(bytes("Pages purgeable"))
        .map(|(file, purgeable)| file.max(purgeable));
    let available = free
        .zip(reclaimable)
        .and_then(|(free, reclaimable)| free.checked_add(reclaimable))
        .map(|available| total.map_or(available, |total| available.min(total)));
    let speed_limit = thermal
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            (key.trim() == "CPU_Speed_Limit")
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        })
        .filter(|value| *value <= 100);
    let thermal_warning = thermal
        .lines()
        .filter_map(|line| line.split_once('='))
        .find_map(|(key, value)| {
            key.trim()
                .ends_with("Thermal Warning Level")
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        });
    HostResources {
        source: "macos_vm_stat_sysctl_pmset".into(),
        total_memory_bytes: total,
        available_memory_bytes: available,
        available_memory_kind: "free_plus_bounded_reclaimable_estimate".into(),
        free_memory_bytes: free,
        reclaimable_memory_bytes: reclaimable,
        occupied_memory_bytes: total
            .zip(available)
            .map(|(total, available)| total.saturating_sub(available)),
        compressor_bytes: bytes("Pages occupied by compressor"),
        swap_used_bytes: sys
            .get("vm.swapusage")
            .and_then(|text| text.split("used =").nth(1))
            .and_then(|text| text.split_whitespace().next())
            .and_then(parse_memory_size),
        swap_out_pages: values.get("Swapouts").copied(),
        memory_pressure: sys
            .get("kern.memorystatus_vm_pressure_level")
            .and_then(|value| match *value {
                "1" => Some(MemoryPressure::Normal),
                "2" => Some(MemoryPressure::Warning),
                "4" => Some(MemoryPressure::Critical),
                _ => None,
            }),
        load_average_one_minute: sys
            .get("vm.loadavg")
            .and_then(|text| text.trim_start_matches('{').split_whitespace().next())
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|load| load.is_finite() && *load >= 0.0),
        logical_cpus: sys
            .get("hw.logicalcpu")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|cpus| *cpus > 0),
        thermal_throttled: if thermal_warning.is_some_and(|level| level > 0) {
            Some(true)
        } else {
            speed_limit
                .map(|limit| limit < 100)
                .or_else(|| thermal_warning.map(|_| false))
        },
        cpu_speed_limit_percent: speed_limit,
        thermal_warning_level: thermal_warning,
        ..HostResources::default()
    }
}

/// Apply the macOS thermal pressure level (`notifyutil -g com.apple.system.thermalpressurelevel`
/// prints `<name> <level>`). On macOS the levels are 0 nominal, 1 moderate, 2 heavy, 3 trapping,
/// 4 sleeping (`OSThermalNotification.h`). `pmset -g therm` reports nothing on Apple silicon, so
/// this is the only unprivileged thermal signal there. Throttled from `heavy` up; a pmset
/// observation of throttling is never cleared by it.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn apply_thermal_pressure(observed: &mut HostResources, notify: &str) {
    let Some(level) = notify
        .split_whitespace()
        .last()
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return;
    };
    let name = match level {
        0 => "nominal",
        1 => "moderate",
        2 => "heavy",
        3 => "trapping",
        4 => "sleeping",
        _ => return,
    };
    observed.thermal_pressure = Some(name);
    observed.thermal_throttled = Some(observed.thermal_throttled == Some(true) || level >= 2);
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_memory_size(value: &str) -> Option<u64> {
    let (number, multiplier) = match value.as_bytes().last()? {
        b'K' => (&value[..value.len() - 1], 1024_u64),
        b'M' => (&value[..value.len() - 1], 1024_u64.pow(2)),
        b'G' => (&value[..value.len() - 1], 1024_u64.pow(3)),
        _ => (value, 1),
    };
    // Fixed decimal parsing avoids lossy float-to-integer conversion of byte counters.
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole = whole.parse::<u64>().ok()?.checked_mul(multiplier)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u64>()
            .ok()?
            .checked_mul(multiplier)?
            .checked_div(10_u64.checked_pow(u32::try_from(fraction.len()).ok()?)?)?
    };
    whole.checked_add(fraction)
}

#[cfg(target_os = "macos")]
fn sample_macos() -> HostResources {
    let vm = bounded_command("/usr/bin/vm_stat", &[]);
    let sys = bounded_command(
        "/usr/sbin/sysctl",
        &[
            "hw.memsize",
            "hw.logicalcpu",
            "vm.loadavg",
            "vm.swapusage",
            "kern.memorystatus_vm_pressure_level",
        ],
    );
    let therm = bounded_command("/usr/bin/pmset", &["-g", "therm"]);
    let pressure = bounded_command(
        "/usr/bin/notifyutil",
        &["-g", "com.apple.system.thermalpressurelevel"],
    );
    let mut result = parse_macos(
        vm.as_deref().unwrap_or_default(),
        sys.as_deref().unwrap_or_default(),
        therm.as_deref().unwrap_or_default(),
    );
    apply_thermal_pressure(&mut result, pressure.as_deref().unwrap_or_default());
    result.gpu_activity = bounded_command("/usr/sbin/ioreg", &["-r", "-c", "AGXAccelerator", "-l"])
        .as_deref()
        .and_then(super::super::device_activity::parse_apple_activity);
    for (name, missing) in [
        ("vm_stat", vm.is_none()),
        ("sysctl", sys.is_none()),
        ("pmset", therm.is_none()),
        ("thermal_pressure", result.thermal_throttled.is_none()),
        ("available_memory", result.available_memory_bytes.is_none()),
    ] {
        if missing {
            result.unavailable.push(name.into());
        }
    }
    result
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn bounded_command(program: &str, args: &[&str]) -> Option<String> {
    bounded_command_with(program, args, Duration::from_millis(400))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) fn bounded_command_with(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<String> {
    let (_, text) = bounded_output(program, args, timeout)?;
    // A missing optional sysctl may cause nonzero status while the other fields remain valid.
    (!text.trim().is_empty()).then_some(text)
}

/// Default stdout cap for telemetry and discovery commands.
const TELEMETRY_OUTPUT_CAP: u64 = 64 * 1024;

/// Run a command with a deadline and return its exit success and up to 64 KiB of stdout. The
/// child is killed and reaped at the deadline; `None` means it could not run or timed out.
pub(crate) fn bounded_output(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Option<(bool, String)> {
    let mut command = Command::new(program);
    command.args(args);
    let output = run_bounded(&mut command, timeout, TELEMETRY_OUTPUT_CAP, false).ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    Some((output.status.success(), text))
}

/// Exit status and capped output of a [`run_bounded`] child.
#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    /// Empty unless stderr capture was requested.
    pub(crate) stderr: Vec<u8>,
}

/// Run `command` with null stdin and a deadline, keeping at most `cap` bytes of stdout (and of
/// stderr when `capture_stderr`; otherwise stderr is discarded). Output beyond the cap is drained
/// and dropped so the child never blocks on a full pipe.
///
/// The child is killed and reaped on every non-success path. Reader threads are joined until the
/// same deadline and detached after it, so a grandchild that inherited a pipe cannot stretch the
/// caller past its deadline. Blocking: call it off the async runtime.
///
/// # Errors
/// Returns the spawn or wait error, or [`std::io::ErrorKind::TimedOut`] at the deadline.
pub(crate) fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    cap: u64,
    capture_stderr: bool,
) -> std::io::Result<BoundedOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .spawn()?;
    // Drain pipes while the child runs: reading only after exit let a child with more than a
    // pipe buffer of output block on write until the deadline killed it.
    let stdout = child.stdout.take().map(|pipe| capped_reader(pipe, cap));
    let stderr = child.stderr.take().map(|pipe| capped_reader(pipe, cap));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match outcome {
                    Err(error) => error,
                    _ => std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "command exceeded its deadline",
                    ),
                });
            }
        }
    };
    // A grandchild that inherited a pipe keeps it open after the child exits; stop waiting for
    // EOF at the deadline and keep what was read so far.
    let collect = |reader: Option<CappedReader>| {
        let Some((buffer, handle)) = reader else {
            return Vec::new();
        };
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::mem::take(
            &mut *buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    };
    Ok(BoundedOutput {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

/// Bytes read so far, and the thread still filling them.
type CappedReader = (
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    std::thread::JoinHandle<()>,
);

fn capped_reader(mut pipe: impl Read + Send + 'static, cap: u64) -> CappedReader {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&buffer);
    let handle = std::thread::spawn(move || {
        let cap = usize::try_from(cap).unwrap_or(usize::MAX);
        let mut chunk = [0_u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let mut kept = sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let room = cap.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..read.min(room)]);
                }
            }
        }
    });
    (buffer, handle)
}
