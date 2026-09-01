# Prepare FreeLlama for production

This runbook prepares a single-operator or trusted-team FreeLlama deployment. FreeLlama now
supports authenticated nonloopback listeners, but it is not a multi-tenant authorization or
billing service. Put TLS and tenant-specific rate limits at an external ingress when traffic
crosses a machine boundary.

## Meet the prerequisites

- Install one Ollama CLI and server version. Run `freellama doctor` and resolve any mismatch.
- Install Node.js 20 or later for MCP and npm distribution use.
- Use the release artifacts for your operating system, or build with Rust 1.85 or later.
- Preinstall and benchmark exact model tags. Discovery never authorizes a pull.
- Keep the primary and optional CPU Ollama processes on distinct loopback sockets.

After a tag has been published, install that exact checksummed release on macOS or Linux:

```bash
curl -fsSLO https://raw.githubusercontent.com/bgauryy/FreeLlama/main/scripts/install.sh
sh install.sh --version vX.Y.Z
```

The installer downloads the exact-version binary and verifies it against the release's
`SHA256SUMS`. From a Windows checkout, run the following command after replacing `VERSION` with the
published tag:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\install.ps1 -Version VERSION
```

A tag release builds five tested package targets and publishes GitHub assets plus the `freellama`
and `freellama-mcp-server` npm packages. The first npm publication requires the repository's
`NPM_TOKEN`; later releases can use npm trusted publishing after the packages are linked to
`.github/workflows/release.yml`.

## Configure Ollama explicitly

Set these variables on each Ollama service rather than relying on invisible defaults:

| Setting | Starting value | Reason |
|---|---:|---|
| `OLLAMA_FLASH_ATTENTION` | `1` | Required before using quantized K/V cache |
| `OLLAMA_KV_CACHE_TYPE` | `q8_0` | Reduces K/V memory; recheck quality on each important model |
| `OLLAMA_NUM_PARALLEL` | `1` | Avoids multiplying context memory before measuring same-model concurrency |
| `OLLAMA_MAX_LOADED_MODELS` | Primary `2`; CPU `1` | Matches one large accelerator model plus bounded helpers on the measured topology |

These are starting values, not universal constants. NVIDIA, AMD, Apple, and CPU-only hosts must
run the hardware validation before promotion. See [Ollama optimization](OLLAMA_SYSTEM_OPTIMIZATION.md)
for device-specific controls.

Put the values in the service manager that owns each Ollama process: launchd on macOS, systemd or
the container definition on Linux, and the Windows service wrapper. Shell exports and
`launchctl setenv` are useful diagnostics but are login-session state, not a reboot-persistent
production configuration. After a real service restart, rerun `doctor` and `scripts/check.sh` and
require the same visible values before admitting work.

## Create the security and state files

Generate a token without placing the secret in shell history or the process list:

```bash
freellama auth-token --out ~/.local/share/freellama/auth.token
```

The command creates a new file with mode `0600` on Unix and refuses to overwrite an existing
token. Start the service with persistent feedback and authentication:

```bash
freellama serve \
  --feedback-file ~/.local/share/freellama/feedback.json \
  --auth-token-file ~/.local/share/freellama/auth.token \
  --upstream http://127.0.0.1:11434 \
  --cpu-upstream http://127.0.0.1:11436 \
  --cpu-model nomic-embed-text:latest
```

Set `FREELLAMA_AUTH_TOKEN_FILE` to the same file for the CLI, MCP server, and bundled research
adapters. Authentication covers both `/_freellama/v1/*` and Ollama-compatible passthrough routes.
Use `--allow-remote` only with an authentication token. The server refuses unauthenticated
nonloopback listeners.

The feedback snapshot is versioned, bounded by backend and task type, and replaced atomically.
Only physically verified warm samples are saved. A corrupt or unsupported snapshot makes startup
fail instead of silently discarding routing evidence. Use `--ephemeral-feedback` only for
disposable tests.

## Configure MCP

Set these values in the MCP host environment:

- `FREELLAMA_AUTH_TOKEN_FILE`: bearer-token file used by native and adapter HTTP clients.
- `FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR`: persistent, per-model token-estimate records.
- `FREELLAMA_MCP_ALLOWED_ROOTS`: exact directories available to `delegate_research`.
- `FREELLAMA_SERVE_ENDPOINT`: authenticated FreeLlama endpoint.

Persistent token calibration eliminates the repeated “uncalibrated first call” after a model has
been observed on that host. A genuinely new model/template still starts from the conservative
configurable estimate because Ollama exposes no stable preflight tokenizer API. Records contain
only model identifiers, scale, and sample count; they contain no prompts.

## Verify lifecycle and placement

`keep_alive:0` no longer sacrifices placement evidence. FreeLlama temporarily holds the runner,
observes `/api/ps`, records eligible feedback, explicitly unloads it, and verifies that it is no
longer resident before returning. Inspect both `execution.observation` and
`execution.lifecycle.status`.

Require these health contracts before admitting traffic:

- `placement_feedback_persistence: versioned_atomic_snapshot_v1`
- `authentication: optional_bearer_all_routes`
- `immediate_unload_observation: observe_then_unload`
- `placement_observation: ollama_api_ps_after_execution`
- `placement_evidence_gate: configured_or_observed`

Run `skills/freellama/scripts/check.sh` with `FREELLAMA_AUTH_TOKEN_FILE` set. It verifies the
active versions, visible Ollama settings, persisted-feedback contract, authentication contract,
backend topology, and binary freshness.

## Validate every hardware class

Compile-only CI runs on Windows, Linux, and macOS. Live accelerator acceptance is separate because
hosted runners do not represent production GPU drivers or memory topology. Prepare self-hosted
Apple Metal, NVIDIA Linux, AMD Linux, and NVIDIA Windows runners, then dispatch
`.github/workflows/hardware-validation.yml`.

Each environment supplies its exact GPU and CPU model tags and a running FreeLlama endpoint. The
portable runner in `benchmark/hardware/run_validation.py` executes GPU coding and CPU embedding
concurrently, verifies physical processor receipts, checks admission receipts, and writes a JSON
artifact. Add `--vision-model` and `--vision-image` for that host's OCR/vision gate.

Do not mark a hardware row validated until its uploaded receipt says `verdict: accept`. Missing
hardware is an explicit release gap, not a result that documentation can waive.

## Promotion checklist

Promote a release only when all conditions pass:

1. `cargo fmt --all --check`, Clippy, Rust tests, typecheck, unit, integration, and live E2E pass.
2. Release packaging contains every declared target and `SHA256SUMS` verifies.
3. The active CLI and Ollama server versions match.
4. Reboot-persistent service definitions carry the explicit Ollama settings and FreeLlama args.
5. Health reports authentication, atomic feedback persistence, and current placement contracts.
6. Restart testing proves feedback reloads and a corrupt snapshot fails startup.
7. Immediate unload reports verified pre-unload placement and verified post-unload absence.
8. Every claimed hardware row has a real uploaded acceptance receipt.
9. The MCP schema remains six tools and within its context budget.

Rollback by returning the listener to loopback, stopping the new service, and starting the prior
checksummed binary with the same policy and feedback snapshot. Never downgrade across an unsupported
feedback schema without preserving the old binary and snapshot together.
