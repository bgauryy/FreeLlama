//! Optional driver activity observations. Missing readings never imply idle hardware.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
use super::resources::{GpuActivityObservation, GpuDeviceActivity};

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn percentage(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn observation(source: &str, devices: Vec<GpuDeviceActivity>) -> Option<GpuActivityObservation> {
    (!devices.is_empty()).then(|| GpuActivityObservation {
        source: source.into(),
        scope: "observed_devices_all_processes",
        window: "driver_defined",
        devices,
    })
}

/// Read only the quoted device-utilization counter in an accelerator's statistics dictionary.
/// The bounded command retains at most 64 KiB, so later devices may be absent from this sample.
#[cfg(any(target_os = "macos", test))]
pub(super) fn parse_apple_activity(text: &str) -> Option<GpuActivityObservation> {
    let mut device = None;
    let mut devices = Vec::new();
    for line in text.lines() {
        if let Some((_, header)) = line.split_once("+-o ") {
            // Reset at every registry node, so a user client's properties cannot be attributed
            // to its accelerator parent. Class plus registry ID identifies the observed node.
            device = header.split_once("<class ").and_then(|(_, rest)| {
                let (class, rest) = rest.split_once(',')?;
                if !class.starts_with("AGXAccelerator") {
                    return None;
                }
                let id = rest
                    .trim_start()
                    .strip_prefix("id ")?
                    .split(',')
                    .next()?
                    .trim();
                (!id.is_empty()).then(|| format!("{class}@{id}"))
            });
            continue;
        }
        let Some(identity) = &device else {
            continue;
        };
        let property = line.trim_start_matches([' ', '\t', '|']);
        let Some(dictionary) = property
            .strip_prefix("\"PerformanceStatistics\"")
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(str::trim)
            .and_then(|rest| rest.strip_prefix('{'))
            .and_then(|rest| rest.strip_suffix('}'))
        else {
            continue;
        };
        let busy = dictionary.split(',').find_map(|field| {
            let (key, value) = field.split_once('=')?;
            (key.trim() == "\"Device Utilization %\"")
                .then(|| percentage(value))
                .flatten()
        });
        if let Some(busy) = busy {
            devices.push(GpuDeviceActivity {
                device: identity.clone(),
                busy_percent: Some(busy),
                memory_busy_percent: None,
            });
        }
    }
    observation("apple_ioreg_performance_statistics", devices)
}

/// NVIDIA's first two CSV fields remain the memory collector's contract; activity and UUID
/// follow them in the same query. N/A counters stay nullable without losing a known device.
#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_nvidia_activity(text: &str) -> Option<GpuActivityObservation> {
    let devices = text
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() != 5 {
                return None;
            }
            let uuid = fields[4];
            if uuid.strip_prefix("GPU-").is_none_or(str::is_empty) {
                return None;
            }
            Some(GpuDeviceActivity {
                device: uuid.to_owned(),
                busy_percent: percentage(fields[2]),
                memory_busy_percent: percentage(fields[3]),
            })
        })
        .collect();
    observation("nvidia_smi", devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apple(value: &str) -> String {
        format!(
            "+-o AGXAcceleratorG14X <class AGXAcceleratorG14X, id 0x401, registered>\n  | {{\n  | \"PerformanceStatistics\" = {{\"Renderer Utilization %\"=99,\"Device Utilization %\"={value}}}\n  | }}\n"
        )
    }

    #[test]
    fn apple_preserves_observed_zero_and_rejects_unknown_or_invalid_percentages() {
        let observed = parse_apple_activity(&apple("0")).unwrap();
        assert_eq!(observed.devices[0].busy_percent, Some(0.0));
        assert_eq!(observed.devices[0].memory_busy_percent, None);
        assert_eq!(observed.devices[0].device, "AGXAcceleratorG14X@0x401");
        assert_eq!(observed.scope, "observed_devices_all_processes");
        assert_eq!(observed.window, "driver_defined");
        for value in ["NaN", "-1", "101", "\"31\"", "N/A", ""] {
            assert_eq!(parse_apple_activity(&apple(value)), None, "{value}");
        }
        assert_eq!(parse_apple_activity(""), None);
        assert_eq!(
            parse_apple_activity(
                &apple("31").replace("Device Utilization %", "Tiler Utilization %")
            ),
            None
        );
    }

    #[test]
    fn apple_accepts_only_accelerator_statistics_and_keeps_device_identity() {
        let text = format!(
            "{}  +-o Client <class AGXDeviceUserClient, id 0x999, active>\n  | \"PerformanceStatistics\" = {{\"Device Utilization %\"=99}}\n{}",
            apple("31"),
            apple("12.5").replace("0x401", "0x402")
        );
        let observed = parse_apple_activity(&text).unwrap();
        assert_eq!(observed.devices.len(), 2);
        assert_eq!(observed.devices[0].busy_percent, Some(31.0));
        assert_eq!(observed.devices[1].busy_percent, Some(12.5));
        assert_ne!(observed.devices[0].device, observed.devices[1].device);
        assert_eq!(
            parse_apple_activity("\"PerformanceStatistics\" = {\"Device Utilization %\"=31}"),
            None
        );
        assert_eq!(
            parse_apple_activity(
                &apple("31").replace("\"PerformanceStatistics\"", "\"Unrelated\"")
            ),
            None
        );
        assert_eq!(
            parse_apple_activity(&apple("31").replace("=31}", "=31")),
            None
        );
    }

    #[test]
    fn nvidia_preserves_per_device_zero_and_mixed_unavailable_counters() {
        let observed =
            parse_nvidia_activity("24576, 1024, 0, [N/A], GPU-a\n24576, 23552, N/A, 20.5, GPU-b\n")
                .unwrap();
        assert_eq!(observed.devices.len(), 2);
        assert_eq!(observed.devices[0].device, "GPU-a");
        assert_eq!(observed.devices[0].busy_percent, Some(0.0));
        assert_eq!(observed.devices[0].memory_busy_percent, None);
        assert_eq!(observed.devices[1].device, "GPU-b");
        assert_eq!(observed.devices[1].busy_percent, None);
        assert_eq!(observed.devices[1].memory_busy_percent, Some(20.5));
    }

    #[test]
    fn nvidia_malformed_activity_never_becomes_zero_or_an_invented_device() {
        let observed = parse_nvidia_activity("100, 10, 101, NaN, GPU-a\n").unwrap();
        assert_eq!(observed.devices[0].busy_percent, None);
        assert_eq!(observed.devices[0].memory_busy_percent, None);
        for text in [
            "",
            "100, 10",
            "100, 10, 40, 20, N/A",
            "100, 10, 40, 20, GPU-",
        ] {
            assert_eq!(parse_nvidia_activity(text), None, "{text}");
        }
    }
}
