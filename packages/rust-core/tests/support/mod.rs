use freellama::platform::{
    PlatformConfig,
    resources::{HostResources, LowMemoryMode, ResourceGovernor, ResourcePolicy},
};
use std::path::PathBuf;

fn protocol_telemetry() -> HostResources {
    HostResources {
        source: "protocol_fixture".into(),
        total_memory_bytes: Some(1 << 50),
        available_memory_bytes: Some(1 << 49),
        ..HostResources::default()
    }
}

/// Protocol tests use deterministic telemetry. Resource policy tests inject their own signals.
pub fn platform_config(
    listen: impl Into<String>,
    upstream: impl Into<String>,
    benchmark: Option<PathBuf>,
    policy: Option<PathBuf>,
) -> PlatformConfig {
    let mut config = PlatformConfig::new(listen, upstream, benchmark, policy);
    config.resource_governor = ResourceGovernor::with_sampler(
        ResourcePolicy {
            low_memory: LowMemoryMode::Advisory,
            ..ResourcePolicy::default()
        },
        protocol_telemetry,
    )
    .unwrap();
    config
}

pub fn proxy_config(
    listen: impl Into<String>,
    upstream: impl Into<String>,
    remote: bool,
) -> freellama::proxy::ProxyConfig {
    freellama::proxy::ProxyConfig::new(listen, upstream, remote).with_resource_governor(
        ResourceGovernor::with_sampler(
            ResourcePolicy {
                low_memory: LowMemoryMode::Advisory,
                ..ResourcePolicy::default()
            },
            protocol_telemetry,
        )
        .unwrap(),
    )
}
