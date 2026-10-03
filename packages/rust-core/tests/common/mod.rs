use freellama::platform::{
    PlatformConfig,
    resources::{HostResources, ResourceGovernor, ResourcePolicy},
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
#[allow(dead_code)] // Shared by integration binaries with different fixture needs.
pub fn platform_config(
    listen: impl Into<String>,
    upstream: impl Into<String>,
    benchmark: Option<PathBuf>,
    policy: Option<PathBuf>,
    intent: impl Into<String>,
) -> PlatformConfig {
    let mut config = PlatformConfig::new(listen, upstream, benchmark, policy, intent);
    config.resource_governor =
        ResourceGovernor::with_sampler(ResourcePolicy::default(), protocol_telemetry).unwrap();
    config
}

#[allow(dead_code)] // Some integration binaries use only the platform fixture.
pub fn proxy_config(
    listen: impl Into<String>,
    upstream: impl Into<String>,
    remote: bool,
) -> freellama::proxy::ProxyConfig {
    freellama::proxy::ProxyConfig::new(listen, upstream, remote).with_resource_governor(
        ResourceGovernor::with_sampler(ResourcePolicy::default(), protocol_telemetry).unwrap(),
    )
}
