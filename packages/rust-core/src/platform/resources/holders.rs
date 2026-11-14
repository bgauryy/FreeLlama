//! The programs holding the most memory, named when a load has to wait for memory, so the caller
//! can tell its user what to close ("node: tsserver.js ×4 holds 5.3 GB") instead of only that the
//! machine is short. Two bounded `ps` samples, taken only on that path and reused for a few
//! seconds; never an admission input.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

/// How many programs a hold names.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const LIMIT: usize = 5;
/// How long one sample is reused.
const FRESH: Duration = Duration::from_secs(10);
/// Runtimes whose own name says nothing; the script they run names the program instead.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
const INTERPRETERS: [&str; 9] = [
    "node", "python", "python3", "ruby", "java", "bun", "deno", "perl", "php",
];

/// One program (processes with the same name are summed) and the memory it holds.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemoryHolder {
    pub name: String,
    /// How many processes share this name.
    pub processes: u32,
    /// The largest of them, the one to look at first.
    pub pid: u32,
    pub resident_bytes: u64,
}

struct HolderCache {
    sample: Option<(Instant, Vec<MemoryHolder>)>,
    refreshing: bool,
}

static CACHE: Mutex<HolderCache> = Mutex::new(HolderCache {
    sample: None,
    refreshing: false,
});

impl HolderCache {
    fn snapshot(&mut self) -> (Vec<MemoryHolder>, bool) {
        let fresh = self
            .sample
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < FRESH);
        let refresh = !fresh && !self.refreshing;
        self.refreshing |= refresh;
        // Stale diagnostics are less useful than no diagnostics: never present an old holder
        // as the reason current work is blocked. These values are not admission evidence.
        let holders = if fresh {
            self.sample
                .as_ref()
                .map_or_else(Vec::new, |(_, holders)| holders.clone())
        } else {
            Vec::new()
        };
        (holders, refresh)
    }
}

/// Singleflight ownership survives a caller dropping its request, and clears on spawn failure
/// or panic. No mutex is held while the bounded operating-system commands run.
struct RefreshClaim(&'static Mutex<HolderCache>);

impl Drop for RefreshClaim {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .refreshing = false;
    }
}

/// Fresh cached diagnostics, largest first. A missing/stale sample schedules one bounded
/// background refresh and returns empty immediately, so diagnostics never delay admission.
#[must_use]
pub fn memory_holders() -> Vec<MemoryHolder> {
    let (holders, refresh) = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .snapshot();
    if refresh {
        let claim = RefreshClaim(&CACHE);
        // One short-lived worker at a time, at most once per freshness interval. This API is
        // also called by synchronous error formatting, so it cannot rely on a Tokio runtime.
        let _ = std::thread::Builder::new()
            .name("freellama-memory-holders".into())
            .spawn(move || {
                let sample = sample();
                claim
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .sample = Some((Instant::now(), sample));
                drop(claim);
            });
    }
    holders
}

/// Like [`memory_holders`], for a refusal that is about to be returned: a cold or stale cache
/// is sampled now (two bounded `ps` calls off the async threads) instead of returning empty, so
/// the first refusal after startup still names who holds the memory.
pub async fn memory_holders_now() -> Vec<MemoryHolder> {
    let cached = memory_holders();
    if !cached.is_empty() {
        return cached;
    }
    let Ok(sampled) = tokio::task::spawn_blocking(sample).await else {
        return Vec::new();
    };
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .sample = Some((Instant::now(), sampled.clone()));
    sampled
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sample() -> Vec<MemoryHolder> {
    // `-A` lists every process on both macOS and Linux (`-e` means environment on macOS). Sorted
    // largest first, so the command-output cap keeps the processes that matter.
    let sort: &[&str] = if cfg!(target_os = "macos") {
        &["-m"]
    } else {
        &["--sort=-rss"]
    };
    let run = |args: &[&str]| {
        super::sampling::bounded_command_with("/bin/ps", args, Duration::from_secs(1))
    };
    let Some(executables) = run(&[&["-A"], sort, &["-o", "rss=,pid=,comm="]].concat()) else {
        return Vec::new();
    };
    // Command lines only for the interpreters among the largest, to name the script they run.
    let interpreters = executables
        .lines()
        .take(200)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _rss = fields.next()?;
            let pid = fields.next()?;
            let executable = fields.collect::<Vec<_>>().join(" ");
            INTERPRETERS
                .contains(&basename(&executable))
                .then(|| pid.to_owned())
        })
        .take(64)
        .collect::<Vec<_>>();
    let command_lines = if interpreters.is_empty() {
        None
    } else {
        run(&["-o", "pid=,args=", "-p", &interpreters.join(",")])
    };
    parse(&executables, command_lines.as_deref().unwrap_or_default())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sample() -> Vec<MemoryHolder> {
    Vec::new()
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// "node: tsserver.js" for an interpreter running a script; the executable's name otherwise.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn program_name(executable: &str, args: Option<&str>) -> String {
    let name = basename(executable);
    if !INTERPRETERS.contains(&name) {
        return name.to_owned();
    }
    let script = args.and_then(|args| {
        args.split_whitespace()
            .skip(1)
            .find(|arg| !arg.starts_with('-') && (arg.contains('/') || arg.contains('.')))
    });
    script.map_or_else(
        || name.to_owned(),
        |script| format!("{name}: {}", basename(script)),
    )
}

/// Join `ps -o rss=,pid=,comm=` (RSS in KiB) with `ps -o pid=,args=` and sum by program.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn parse(executables: &str, command_lines: &str) -> Vec<MemoryHolder> {
    let args: BTreeMap<u32, &str> = command_lines
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, rest.trim_start()))
        })
        .collect();
    let mut programs: BTreeMap<String, MemoryHolder> = BTreeMap::new();
    // The largest single process per program, whose pid the holder reports.
    let mut largest: BTreeMap<String, u64> = BTreeMap::new();
    for line in executables.lines() {
        let mut fields = line.split_whitespace();
        let (Some(kib), Some(pid)) = (
            fields.next().and_then(|value| value.parse::<u64>().ok()),
            fields.next().and_then(|value| value.parse::<u32>().ok()),
        ) else {
            continue;
        };
        let executable = fields.collect::<Vec<_>>().join(" ");
        if executable.is_empty() {
            continue;
        }
        let name = program_name(&executable, args.get(&pid).copied());
        let bytes = kib.saturating_mul(1024);
        let entry = programs.entry(name.clone()).or_insert(MemoryHolder {
            name,
            processes: 0,
            pid,
            resident_bytes: 0,
        });
        let biggest = largest.entry(entry.name.clone()).or_insert(0);
        if bytes >= *biggest {
            *biggest = bytes;
            entry.pid = pid;
        }
        entry.processes += 1;
        entry.resident_bytes = entry.resident_bytes.saturating_add(bytes);
    }
    let mut holders: Vec<MemoryHolder> = programs.into_values().collect();
    holders.sort_by_key(|holder| std::cmp::Reverse(holder.resident_bytes));
    holders.truncate(LIMIT);
    holders
}

/// One decimal of GB (1024³ bytes, the unit [`describe`] uses): "15.6 GB".
#[must_use]
pub fn gb(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / (1024 * 1024 * 1024);
    format!("{}.{} GB", tenths / 10, tenths % 10)
}

/// "node: tsserver.js ×4 5.3 GB, rustc 2.7 GB" for a human-readable message.
#[must_use]
pub fn describe(holders: &[MemoryHolder]) -> String {
    holders
        .iter()
        .map(|holder| {
            let tenths = holder.resident_bytes.saturating_mul(10) / (1024 * 1024 * 1024);
            let count = if holder.processes > 1 {
                format!(" ×{}", holder.processes)
            } else {
                String::new()
            };
            format!("{}{count} {}.{} GB", holder.name, tenths / 10, tenths % 10)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_diagnostics_refresh_once_without_waiting_or_reusing_stale_data() {
        let mut cache = HolderCache {
            sample: None,
            refreshing: false,
        };
        assert_eq!(cache.snapshot(), (Vec::new(), true));
        assert_eq!(cache.snapshot(), (Vec::new(), false));
        let holder = MemoryHolder {
            name: "worker".into(),
            processes: 1,
            pid: 7,
            resident_bytes: 1024,
        };
        cache.sample = Some((Instant::now(), vec![holder.clone()]));
        cache.refreshing = false;
        assert_eq!(cache.snapshot(), (vec![holder.clone()], false));
        cache.sample = Some((Instant::now().checked_sub(FRESH).unwrap(), vec![holder]));
        assert_eq!(cache.snapshot(), (Vec::new(), true));
    }

    #[test]
    fn names_programs_by_script_and_sums_their_processes() {
        let executables = "  2097152  4321 /Users/u/.rustup/toolchains/x/bin/rustc\n  \
            1048576  100 /opt/homebrew/bin/node\n  1048576  101 /opt/homebrew/bin/node\n  \
            524288  200 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n    \
            1024  7 /sbin/launchd\n  garbage line\n";
        let command_lines = "4321 /x/rustc --crate-name a\n  100 /opt/homebrew/bin/node --max-old-space-size=7680 \
            /repo/node_modules/typescript/lib/tsserver.js --stdio\n  101 node --inspect /repo/node_modules/typescript/lib/tsserver.js\n";
        let holders = parse(executables, command_lines);
        assert_eq!(holders[0].name, "node: tsserver.js");
        assert_eq!(holders[0].processes, 2);
        assert_eq!(holders[0].resident_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(holders[1].name, "rustc");
        assert_eq!(holders[2].name, "Google Chrome");
        assert_eq!(
            describe(&holders[..2]),
            "node: tsserver.js ×2 2.0 GB, rustc 2.0 GB"
        );
    }
}
