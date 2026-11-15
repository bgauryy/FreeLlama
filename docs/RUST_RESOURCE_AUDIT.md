# Rust resource and panic audit

The resource boundary now measures Windows physical-memory headroom and requires that observation
by default. It also propagates telemetry reader-thread creation failures after killing and reaping
the child process. Test assertions using `unwrap()` were retained.

## Scope and findings

The initial production-library scan used `cargo clippy -p freellama-core --lib -- -W clippy::unwrap_used
-W clippy::expect_used -W clippy::await_holding_lock`. It found no production `unwrap()` or
`await_holding_lock` diagnostics and 30 explicit `expect()` sites at that audit stage. This is a lint census, not proof
that every panic or deadlock is impossible. Deep review covered resource sampling, reservation
accounting, and the holder cache; additional lock spot checks covered raw admission, scope history,
telemetry, and the SQLite store.

| Boundary | Finding and disposition |
|---|---|
| [Resource governor](../packages/rust-core/src/platform/resources/governor.rs) | Its three `expect()` calls state private invariants: the constant default policy validates, a sampling handle is installed before it is awaited, and a sample is saved before the cache is read. The Tokio state mutex serializes these transitions. Keep the explicit invariants. |
| Governor synchronous accounting mutex | All lock acquisitions recover poisoned guards. They protect generation/reservation updates and do not span an await. A fault-injection test poisons the lock and verifies both permit release and conservative reservation decisions. |
| Governor sampling await | The Tokio mutex intentionally spans the shared sampling handle await so cancellation leaves one reusable in-flight sample. Existing cancellation and concurrent-reservation tests cover the affected behavior. A panicking sampler becomes `sampler_failed` with unknown memory and refused admission. |
| [Bounded subprocess](../packages/rust-core/src/platform/resources/sampling.rs) | Reader creation previously used `thread::spawn`, which can panic after a child was started. It now uses fallible `Builder::spawn`; failure kills/reaps the child and returns the I/O error. Reader-buffer locks recover poison and guard only bounded buffer updates. Existing deadline, output-cap, and inherited-pipe tests pass. OS thread-exhaustion injection was not run. |
| [Memory holders](../packages/rust-core/src/platform/resources/holders.rs) | The diagnostic cache recovers poisoned locks and releases its mutex before OS commands and `spawn_blocking` awaits. Holder output does not authorize admission. |
| Store, telemetry, scope and raw-admission spot checks | Their synchronous mutex helpers recover poisoned guards. Inspected async paths release these guards before awaits. This does not establish every application-level lock ordering or runtime contention property. |

## Windows memory contract

[The Windows collector](../packages/rust-core/src/platform/resources/windows.rs) runs one
unprivileged, noninteractive PowerShell CIM query with a 1.5-second command deadline and the shared
64-KiB output cap. It accepts only successful command output containing one JSON object.
[Microsoft defines](https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-operatingsystem)
`TotalVisibleMemorySize` and `FreePhysicalMemory` in KiB. Conversion uses checked multiplication;
the total must be positive and available memory must not exceed it. Missing, malformed, negative,
fractional, overflowing, or inconsistent readings cannot authorize a load under `require-memory`.
Observed zero remains zero.

The collector does not invent CPU-load, OS-pressure, thermal, swap, or GPU observations. These stay
unknown, so `require-all` holds work on Windows. Operators can explicitly select `best-effort` for
unknown-memory admission. See [monitoring](MONITORING.md) for the receipt fields and policy.

## Verification and limits

`cargo test -p freellama-core --lib platform::resources` passed 59 tests with one live-host test
ignored on the macOS development host. The new fixtures exercise valid Windows conversion,
unknown metrics, bad/missing/overflowing input, zero headroom, policy defaults, poisoned accounting,
and sampler panic. Those tests are grouped in
[`resources/tests/windows.rs`](../packages/rust-core/src/platform/resources/tests/windows.rs) and
[`resources/governor/audit_tests.rs`](../packages/rust-core/src/platform/resources/governor/audit_tests.rs);
the existing resource contracts remain enabled in `resources/tests.rs`. The strict production-library check
`cargo clippy -p freellama-core --lib -- -D clippy::unwrap_used -D clippy::await_holding_lock`
passed. A subsequent local workspace check with
`cargo clippy --workspace --all-targets --all-features -- -D warnings` also passed after engine work
settled; that gate does not enable `expect_used` or prove every `expect()` safe.
The explicit read-only live-host smoke also passed on macOS in 60 ms after the subprocess change.

The [CI workflow](../.github/workflows/ci.yml) configures the actual PowerShell/CIM smoke on
Windows, in addition to cross-platform parser fixtures. No completed hosted Windows result is
recorded by this local audit. Actual Windows startup, CIM permissions, command timing, and hardware
behavior need that run. Reproduce the read-only host smoke there with:

```sh
cargo test -p freellama-core --lib live_host_resource_observation -- --ignored --nocapture
```

No model was loaded, downloaded, or executed for these checks. The audit does not claim complete
panic freedom, measured contention, or live Windows GPU qualification.
