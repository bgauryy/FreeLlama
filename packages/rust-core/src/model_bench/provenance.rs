//! Bind throughput evidence to the measured host and its hardware, without publishing its ID.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkMachine {
    /// Domain-separated hash of the OS machine identifier; never the raw identifier or hostname.
    pub host_id_sha256: String,
    pub os: String,
    pub architecture: String,
    pub chip: Option<String>,
    pub logical_cpus: usize,
    pub memory_bytes: u64,
    pub gpu: Option<serde_json::Value>,
}

impl BenchmarkMachine {
    /// Missing host identity or physical memory cannot establish machine-local evidence.
    #[must_use]
    pub fn current() -> Option<Self> {
        let identity = normalize_identity(&host_identity()?)?;
        let profile = crate::platform::machine_profile(crate::DEFAULT_OLLAMA_ENDPOINT);
        let memory_bytes = profile.memory_bytes.filter(|bytes| *bytes > 0)?;
        let mut hash = Sha256::new();
        hash.update(b"freellama-benchmark-host-v1\0");
        hash.update(identity.trim().as_bytes());
        Some(Self {
            host_id_sha256: hash
                .finalize()
                .iter()
                .fold(String::new(), |mut text, byte| {
                    use std::fmt::Write as _;
                    let _ = write!(text, "{byte:02x}");
                    text
                }),
            os: profile.os,
            architecture: profile.architecture,
            chip: profile.chip,
            logical_cpus: profile.logical_cpus,
            memory_bytes,
            gpu: profile.gpu.and_then(|gpu| serde_json::to_value(gpu).ok()),
        })
    }

    #[must_use]
    pub fn matches(&self, current: &Self) -> bool {
        self.host_id_sha256.len() == 64 && self.memory_bytes > 0 && self == current
    }
}

fn normalize_identity(identity: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(identity.trim()).ok()?;
    (!id.is_nil() && id.as_u128() != u128::MAX).then(|| id.to_string())
}

#[cfg(target_os = "linux")]
fn host_identity() -> Option<String> {
    use std::io::Read as _;
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        let Some(mut file) = std::fs::File::open(path).ok().map(|file| file.take(128)) else {
            continue;
        };
        let mut identity = String::new();
        if file.read_to_string(&mut identity).is_ok() && !identity.trim().is_empty() {
            return Some(identity);
        }
    }
    None
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn host_identity() -> Option<String> {
    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) =
        ("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"]);
    #[cfg(target_os = "windows")]
    let (program, args): (&str, &[&str]) = (
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystemProduct -ErrorAction Stop).UUID",
        ],
    );
    let (success, text) = crate::platform::resources::bounded_output(
        program,
        args,
        std::time::Duration::from_secs(5),
    )?;
    if !success {
        return None;
    }
    #[cfg(target_os = "macos")]
    let identity = text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == "\"IOPlatformUUID\"").then(|| value.trim().trim_matches('"').to_owned())
    })?;
    #[cfg(target_os = "windows")]
    let identity = text.trim().to_owned();
    (!identity.is_empty()).then_some(identity)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn host_identity() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_placeholder_machine_ids_do_not_qualify() {
        for identity in [
            "",
            "unknown",
            "00000000-0000-0000-0000-000000000000",
            "ffffffffffffffffffffffffffffffff",
        ] {
            assert!(normalize_identity(identity).is_none());
        }
        assert_eq!(
            normalize_identity("a1b2c3d400005555aaaa0123456789ab"),
            normalize_identity("A1B2C3D4-0000-5555-AAAA-0123456789AB\n")
        );
    }

    #[test]
    fn host_and_hardware_changes_invalidate_calibration() {
        let measured = BenchmarkMachine {
            host_id_sha256: "a".repeat(64),
            os: "test".into(),
            architecture: "arm".into(),
            chip: Some("chip".into()),
            logical_cpus: 8,
            memory_bytes: 32_000_000_000,
            gpu: None,
        };
        assert!(measured.matches(&measured));
        let mut other = measured.clone();
        other.host_id_sha256 = "b".repeat(64);
        assert!(!measured.matches(&other));
        other = measured.clone();
        other.memory_bytes *= 2;
        assert!(!measured.matches(&other));
        other = measured.clone();
        other.gpu = Some(serde_json::json!({"name":"new accelerator"}));
        assert!(!measured.matches(&other));
    }
}
