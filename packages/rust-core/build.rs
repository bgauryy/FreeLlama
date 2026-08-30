fn main() {
    // Only wires up napi-rs's build glue when the `napi` feature is active (Node-addon build).
    // A normal `cargo build`/`cargo test` never touches this — see Cargo.toml's `[features]`.
    // `napi-build` is an optional dependency, so the call itself must be compile-time gated, not
    // just skipped at runtime, or a feature-less build would fail to resolve the crate at all.
    #[cfg(feature = "napi")]
    napi_build::setup();
}
