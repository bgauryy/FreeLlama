//! The model memory budget: the most memory the runners resident on one local backend may hold
//! together.
//!
//! Ollama admits a load against whatever memory is free at that moment, so loads keep landing
//! until the OS starts swapping and every request stalls behind it. With a budget, `FreeLlama`
//! unloads the cheapest idle runners before a load would push the resident total past it, while
//! the host still has headroom. The budget counts memory the host shares with the runners (a CPU
//! backend, or unified memory); a discrete GPU's VRAM keeps its own fit check.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much memory resident runners may hold together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum MemoryBudget {
    /// No budget: residency is left to host pressure and Ollama.
    Off,
    /// A share of host RAM, in hundredths of a percent (1..=10000).
    Percent(u32),
    /// A fixed number of bytes.
    Bytes(u64),
}

impl MemoryBudget {
    /// Three quarters of RAM: Ollama's own pre-flight check treats a load above 80% of free
    /// memory as needing an eviction, and the rest stays for the OS and the file cache that makes
    /// a reload of an unloaded model fast.
    pub const DEFAULT: Self = Self::Percent(7_500);

    /// The budget in bytes on a host with `total` bytes of RAM; `None` when off or unknowable.
    #[must_use]
    pub fn bytes(self, total: Option<u64>) -> Option<u64> {
        match self {
            Self::Off => None,
            Self::Bytes(bytes) => Some(bytes),
            Self::Percent(hundredths) => total.map(|total| {
                let scaled = u128::from(total) * u128::from(hundredths) / 10_000;
                u64::try_from(scaled).unwrap_or(u64::MAX)
            }),
        }
    }
}

/// `text` as an exact fraction `(numerator, denominator)`: digits with at most one decimal point.
fn decimal(text: &str) -> Option<(u128, u128)> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
        || whole.len() + fraction.len() > 30
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let numerator = digits.parse::<u128>().ok()?;
    let denominator = 10_u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    Some((numerator, denominator))
}

impl FromStr for MemoryBudget {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        let invalid = || {
            format!(
                "model memory budget {text:?} must be `off`, a percent of RAM such as `75%`, or a \
                 size such as `24GiB`, `512MB` or a byte count"
            )
        };
        if text.eq_ignore_ascii_case("off") || text == "0" {
            return Ok(Self::Off);
        }
        if let Some(percent) = text.strip_suffix('%') {
            let (numerator, denominator) = decimal(percent.trim()).ok_or_else(invalid)?;
            let hundredths = numerator * 100 / denominator;
            return match u32::try_from(hundredths) {
                Ok(hundredths @ 1..=10_000) => Ok(Self::Percent(hundredths)),
                _ => Err(invalid()),
            };
        }
        let split = text
            .find(|character: char| !(character.is_ascii_digit() || character == '.'))
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(split);
        let (numerator, denominator) = decimal(number).ok_or_else(invalid)?;
        let scale: u128 = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "kb" => 1_000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            "tb" => 1_000_000_000_000,
            "kib" => 1 << 10,
            "mib" => 1 << 20,
            "gib" => 1 << 30,
            "tib" => 1 << 40,
            _ => return Err(invalid()),
        };
        let bytes = numerator.checked_mul(scale).ok_or_else(invalid)? / denominator;
        match u64::try_from(bytes) {
            Ok(bytes) if bytes > 0 => Ok(Self::Bytes(bytes)),
            _ => Err(invalid()),
        }
    }
}

impl TryFrom<String> for MemoryBudget {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<MemoryBudget> for String {
    fn from(budget: MemoryBudget) -> Self {
        budget.to_string()
    }
}

impl fmt::Display for MemoryBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => formatter.write_str("off"),
            Self::Percent(hundredths) => match hundredths % 100 {
                0 => write!(formatter, "{}%", hundredths / 100),
                rest => {
                    let fraction = format!("{rest:02}");
                    write!(
                        formatter,
                        "{}.{}%",
                        hundredths / 100,
                        fraction.trim_end_matches('0')
                    )
                }
            },
            Self::Bytes(bytes) => match bytes.trailing_zeros() {
                30.. => write!(formatter, "{}GiB", bytes >> 30),
                20.. => write!(formatter, "{}MiB", bytes >> 20),
                _ => write!(formatter, "{bytes}"),
            },
        }
    }
}

/// Bytes that runners in `/api/ps` hold, except `keep`: a load of `keep` replaces its own runner.
pub(super) fn resident_bytes(ps: &Value, keep: &str) -> u64 {
    ps["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| crate::ollama_http::ps_entry_name(entry) != Some(keep))
        .filter_map(|entry| entry["size"].as_u64())
        .fold(0, u64::saturating_add)
}

/// How far a load of `load` bytes would take the resident total past `budget`.
pub(super) const fn overage(budget: u64, resident: u64, load: u64) -> u64 {
    resident.saturating_add(load).saturating_sub(budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn budgets_parse_as_off_percent_or_size() {
        assert_eq!("off".parse(), Ok(MemoryBudget::Off));
        assert_eq!("0".parse(), Ok(MemoryBudget::Off));
        assert_eq!("75%".parse(), Ok(MemoryBudget::Percent(7_500)));
        assert_eq!("62.5%".parse(), Ok(MemoryBudget::Percent(6_250)));
        assert_eq!("24GiB".parse(), Ok(MemoryBudget::Bytes(24 * GIB)));
        assert_eq!("1.5 GB".parse(), Ok(MemoryBudget::Bytes(1_500_000_000)));
        assert_eq!("4096".parse(), Ok(MemoryBudget::Bytes(4096)));
        for invalid in [
            "",
            "150%",
            "-5%",
            "0%",
            "12 parsecs",
            "GiB",
            "%",
            "1.2.3GB",
            ".",
        ] {
            assert!(invalid.parse::<MemoryBudget>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn a_percent_budget_scales_host_ram_and_needs_it() {
        assert_eq!(MemoryBudget::DEFAULT.bytes(Some(32 * GIB)), Some(24 * GIB));
        assert_eq!(
            MemoryBudget::Percent(10_000).bytes(Some(u64::MAX)),
            Some(u64::MAX)
        );
        assert_eq!(MemoryBudget::Percent(7_500).bytes(None), None);
        assert_eq!(MemoryBudget::Bytes(5).bytes(None), Some(5));
        assert_eq!(MemoryBudget::Off.bytes(Some(32 * GIB)), None);
    }

    #[test]
    fn a_load_is_over_budget_by_what_the_other_runners_and_it_exceed() {
        let ps = json!({"models":[
            {"name":"big:20b","size":13 * GIB},
            {"name":"ocr:latest","size":4 * GIB},
            {"name":"target:2b","size":GIB},
        ]});
        // The target's own runner is replaced by its load, so it is not counted twice.
        assert_eq!(resident_bytes(&ps, "target:2b"), 17 * GIB);
        assert_eq!(overage(24 * GIB, 17 * GIB, 2 * GIB), 0);
        assert_eq!(overage(18 * GIB, 17 * GIB, 2 * GIB), GIB);
    }

    #[test]
    fn budgets_round_trip_through_their_text_form() {
        for budget in [
            MemoryBudget::Off,
            MemoryBudget::Percent(6_250),
            MemoryBudget::Percent(7_505),
            MemoryBudget::Bytes(24 * GIB),
            MemoryBudget::Bytes(512 << 20),
            MemoryBudget::Bytes(4096),
        ] {
            assert_eq!(budget.to_string().parse(), Ok(budget));
        }
        assert_eq!(MemoryBudget::Bytes(14 * GIB).to_string(), "14GiB");
    }
}
