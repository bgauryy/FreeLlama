# FreeLlama MCP configuration

Settings for the [FreeLlama MCP server](README.md).

## Environment variables

The following settings have environment-variable overrides. Set them in the MCP client's
server-launch config (for example `.mcp.json`'s `env` block) or the launching shell.

| Variable | Default | Affects |
|---|---|---|
| `FREELLAMA_OLLAMA_ENDPOINT` | `http://127.0.0.1:11434` | Default Ollama endpoint for `doctor`, the `models` detail/raw/library views, all `ollama_*` tools, and the autostarted serve's `--upstream` |
| `FREELLAMA_SERVE_ENDPOINT` | `http://127.0.0.1:11435` | Default `endpoint` for serve-backed tools and `delegate_research` |
| `FREELLAMA_MCP_DEFAULT_MODEL` | unset (routes to an installed coding model) | `delegate_research`'s default `model` |
| `FREELLAMA_MCP_DEFAULT_ADAPTER` | `bash` | `delegate_research`'s adapter (`octocode` to switch) |
| `FREELLAMA_MCP_MAX_TURNS` | `8` | Max agent turns for `delegate_research` |
| `FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS` | `180` | Whole-subprocess timeout for `delegate_research` |
| `FREELLAMA_MCP_PULL_TIMEOUT_SECONDS` | `1200` | Default `ollama_manage` `"pull"` timeout |
| `FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS` | `30` | Timeout for other direct Ollama HTTP calls |
| `FREELLAMA_MCP_ALLOWED_ROOTS` | checkout: this repository; published: must be set | Platform-separated allowlist of directories `delegate_research` may read (`:` on macOS/Linux, `;` on Windows) |
| `FREELLAMA_MCP_MODEL_EVIDENCE` | checkout: `packages/mcp/evidence/model-evidence.json`; published: unset | Path to per-model research grades (see "Runtime evidence") |
| `FREELLAMA_EXTERNAL_COST_MODEL` | unset | External-model label in estimated avoided-cost telemetry; set all three cost variables or none |
| `FREELLAMA_EXTERNAL_COST_INPUT_USD_PER_M` | unset | External input USD per million tokens |
| `FREELLAMA_EXTERNAL_COST_OUTPUT_USD_PER_M` | unset | External output USD per million tokens |
| `FREELLAMA_CONTROL_TIMEOUT_SECONDS` | `30` | Decision-only serve calls; read by the native client (`packages/rust-core/src/serve_client.rs`) |
| `FREELLAMA_TASK_TIMEOUT_SECONDS` | `900` | Generation calls; read by the native client (`packages/rust-core/src/serve_client.rs`) |
| `FREELLAMA_AUTH_TOKEN_FILE` | unset | Bearer-token file used by the native client and research adapters for all serve routes |
| `FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR` | platform data directory | Prompt-free, model-specific token-estimator calibration shared across adapter processes |
| `FREELLAMA_MCP_AUTOSTART_SERVE` | `1` | When the default loopback serve endpoint does not answer, start `freellama serve` as a child of the MCP server (stopped with it): once in the background at launch, so other local clients find it, and again on a tool call if it is still down. `0` disables |
| `FREELLAMA_SERVE_BINARY` | checkout `target/release`/`debug`, then the platform package | Binary used for serve autostart |
| `FREELLAMA_AGENT_OS_SANDBOX` | `auto` | Bash adapter OS confinement: `bwrap` on Linux, `sandbox-exec` on macOS, when present. `off` keeps only the restricted shell |
| `FREELLAMA_AGENT_OCTOCODE_PACKAGE` | `octocode@19.1.0` | Pinned npm spec the Octocode adapter runs through `npx` |

Serve-side placement settings (read by `freellama serve`, not the MCP process):

| Variable | Default | Affects |
|---|---|---|
| `FREELLAMA_EVICT_IDLE_MODELS` | on | Unload idle, unpinned models using reuse and load costs when a cold load needs room. `0` turns eviction off |
| `FREELLAMA_CPU_NUM_THREAD` | half the logical cores (not set on macOS) | `num_thread` for CPU-placed tasks only; GPU tasks leave it to Ollama |
| `FREELLAMA_MAX_QUEUE_WAIT_SECONDS` | `120` | How long a managed task queues for a slot or memory |
| `OLLAMA_NUM_PARALLEL`, `OLLAMA_KV_CACHE_TYPE`, `OLLAMA_FLASH_ATTENTION` | Ollama's | Read as hints for the memory estimate: parallel slots multiply the KV cache, and `q8_0`/`q4_0` shrink it only with flash attention. Set them for serve the same way as for Ollama |

Research-adapter settings are deployment defaults. The optional `delegate_research.agent` object
overrides the per-call budget (`maxTurns`, `contextTokens`, `outputTokens`); explicit per-call
fields win. Sampling, keep-alive, timeouts, and placement preference for research come from the
operator's `FREELLAMA_AGENT_*` variables or the adapter defaults.
The research subprocess inherits only these server variables: `FREELLAMA_*`; `PATH`, `HOME`,
`USER`, `LOGNAME`; `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`; `TMPDIR`, `TEMP`, `TMP`; Windows `SYSTEMROOT`,
`SYSTEMDRIVE`, `WINDIR`, `COMSPEC`, `PATHEXT`, `APPDATA`, `LOCALAPPDATA`, `USERPROFILE`;
`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` (and lowercase forms); `NODE_EXTRA_CA_CERTS`,
`SSL_CERT_FILE`, `SSL_CERT_DIR`, `REQUESTS_CA_BUNDLE`; `XDG_CACHE_HOME`, `XDG_DATA_HOME`,
`npm_config_cache`.

| Variable | Default | Affects |
|---|---:|---|
| `FREELLAMA_AGENT_NUM_CTX` | `8192` | Ollama context window |
| `FREELLAMA_AGENT_EXECUTION_PREFERENCE` | `auto` | Managed coding-agent backend hint |
| `FREELLAMA_AGENT_MIN_PLACEMENT_EVIDENCE` | `configured` | Set `observed` to fail closed on cold/mismatched placement |
| `FREELLAMA_AGENT_NUM_PREDICT` | `512` | Reserved output budget |
| `FREELLAMA_AGENT_TEMPERATURE` / `FREELLAMA_AGENT_SEED` | `0` / `42` | Repeatable decoding |
| `FREELLAMA_AGENT_THINK` / `FREELLAMA_AGENT_KEEP_ALIVE` | `false` / `5m` | Ollama reasoning and residency |
| `FREELLAMA_AGENT_REQUEST_TIMEOUT_SECONDS` | `600` | Each Ollama chat attempt |
| `FREELLAMA_AGENT_TOOL_TIMEOUT_SECONDS` | Bash `30`, Octocode `45` | Each local tool call |
| `FREELLAMA_AGENT_RETRY_ATTEMPTS` / `FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS` | `2` / `5` | Conversation-level retry |
| `FREELLAMA_AGENT_MAX_PARSE_REPAIRS` / `FREELLAMA_AGENT_PARSE_REPAIR_ECHO_CHARS` | `2` / `500` | Invalid JSON correction limit and retained reply text |
| `FREELLAMA_AGENT_CHARS_PER_TOKEN` | `4` | First-call token estimate; later calibrated from Ollama |
| `FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS` | `256` | Input headroom beyond `num_predict` |
| `FREELLAMA_AGENT_IMAGE_TOKEN_ESTIMATE` | `1024` | Bounded charge per image |
| `FREELLAMA_AGENT_KEEP_RECENT` | `2` | Recent observations preserved verbatim |
| `FREELLAMA_AGENT_COMPACT_PREVIEW_CHARS` | `180` | Old-observation breadcrumb size |
| `FREELLAMA_AGENT_COMPACT_RETAIN_RATIO` | `0.8` | Current payload retained per emergency pass |
| `FREELLAMA_AGENT_CLIP_HEAD_RATIO` | `0.667` | Head share when an emergency clip preserves both ends |
| `FREELLAMA_AGENT_OBSERVATION_PAGE_CHARS` | `3000` | Lossless observation page target |
| `FREELLAMA_AGENT_PINNED_OVERFLOW` | `error` | Preserve system/task bytes; `clip` is explicit opt-in |

Invalid values fail before an Ollama request. Each successful result returns
`contextManagement` with the resolved policy, calibration source/scale, estimated input budget,
and compaction count. Ollama has no stable preflight tokenizer endpoint, so “exact before the first
call” is not claimed. Model-specific calibration persists after successful calls, so later
processes start conservatively from prior `prompt_eval_count` evidence without sharing estimates
between model templates.

**Security:** `delegate_research` grants a local model read access to `workspacePath`. The path is
confined to `FREELLAMA_MCP_ALLOWED_ROOTS` (default: this repository in a checkout; unset in a published
install until you set it), resolved through symlinks so a link inside an allowed root can't escape
it. The default `bash` adapter runs an allowlist of read-only tools in a restricted shell with a
scrubbed environment and, where available, an OS sandbox; it also rejects command substitution,
home-directory paths, `..`, and absolute paths outside that workspace. Production deployments should generate a permission-restricted token with
`freellama auth-token`, start `serve` with `--auth-token-file`, and set
`FREELLAMA_AUTH_TOKEN_FILE` for MCP. Authentication covers managed and raw passthrough routes; use
an external TLS and authorization layer for untrusted or multi-tenant networks.

## Runtime evidence

Admission weights can be set by task kind in the server runtime file's `[task_costs]` table.
Inspect `doctor {view:"full"}` at `platform_health.admission.costs.base_by_task` for effective
weights and `run_task {preview:true}` for current cost and capacity advice. The table changes
managed admission; it does not create Ollama
slots or relax memory guards. Existing queued work keeps its captured cost. See
[monitoring controls](https://github.com/bgauryy/FreeLlama/blob/main/docs/MONITORING.md#adaptive-concurrency).

**No measurements are compiled in.** In a repository checkout, per-model research grades load from
`packages/mcp/evidence/model-evidence.json`. A published package does not include that repository
file, so its evidence table is empty unless you set `FREELLAMA_MCP_MODEL_EVIDENCE`. An unmeasured
model yields a `verify` verdict, and an unreadable evidence file is reported on stderr and treated
as empty. Tool descriptions state the rules.
