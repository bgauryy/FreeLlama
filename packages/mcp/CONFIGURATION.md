# FreeLlama MCP configuration

Settings for the [FreeLlama MCP server](README.md).

## Environment variables

The following settings have environment-variable overrides. Set them in the MCP client's
server-launch config (for example `.mcp.json`'s `env` block) or the launching shell.

| Variable | Default | Affects |
|---|---|---|
| `FREELLAMA_OLLAMA_ENDPOINT` | `http://127.0.0.1:11434` | Default Ollama endpoint for `doctor`, the `models` detail/raw/library views, all `ollama_*` tools, and the autostarted serve's `--upstream` |
| `FREELLAMA_SERVE_ENDPOINT` | `http://127.0.0.1:11435` | Default `endpoint` for serve-backed tools and `delegate_research` |
| `FREELLAMA_MCP_RECEIPT` | `text` | `structured` keeps `run_task` and `task_jobs` receipts inline as `structuredContent` (see [Where an answer goes](#where-an-answer-goes)) |
| `FREELLAMA_MCP_DEFAULT_MODEL` | unset (routes to an installed coding model) | `delegate_research`'s default `model` |
| `FREELLAMA_MCP_DEFAULT_ADAPTER` | `bash` | `delegate_research`'s adapter (`octocode` to switch) |
| `FREELLAMA_MCP_MAX_TURNS` | `8` | Max agent turns for `delegate_research` |
| `FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS` | `180` | Whole-subprocess timeout for `delegate_research` |
| `FREELLAMA_MCP_PULL_TIMEOUT_SECONDS` | `1200` | Default `ollama_pull` timeout |
| `FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS` | `30` | Timeout for other direct Ollama HTTP calls |
| `FREELLAMA_MCP_ALLOWED_ROOTS` | checkout: this repository; published: unset | Required for `delegate_research` in a published install: unset, every call with a `workspacePath` is refused. Platform-separated allowlist of directories it may read (`:` on macOS/Linux, `;` on Windows), for example `/Users/me/code/project:/Users/me/work`. `run_task` and `audio` `files` use `FREELLAMA_FILE_ROOTS` instead |
| `FREELLAMA_FILE_ROOTS` | unset | Platform-separated extra directories `run_task` and `audio` may read `files` from and `audio` may write `outputFile` to, besides the temp directory and the server's working directory (the working directory is skipped when it is `/` or the home directory) |
| `FREELLAMA_MCP_MODEL_EVIDENCE` | checkout: `packages/mcp/evidence/model-evidence.json`; published: unset | Path to per-model research grades (see "Runtime evidence") |
| `FREELLAMA_EXTERNAL_COST_MODEL` | unset | External-model label in estimated avoided-cost telemetry; set all three cost variables or none |
| `FREELLAMA_EXTERNAL_COST_INPUT_USD_PER_M` | unset | External input USD per million tokens |
| `FREELLAMA_EXTERNAL_COST_OUTPUT_USD_PER_M` | unset | External output USD per million tokens |
| `FREELLAMA_CONTROL_TIMEOUT_SECONDS` | `30` | Decision-only serve calls; read by the native client (`packages/rust-core/src/serve_client.rs`) |
| `FREELLAMA_TASK_TIMEOUT_SECONDS` | `900` | Generation calls; read by the native client (`packages/rust-core/src/serve_client.rs`) |
| `FREELLAMA_AUTH_TOKEN_FILE` | unset | Bearer-token file used by the native client and research adapters for all serve routes |
| `FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR` | platform data directory | Prompt-free, model-specific token-estimator calibration shared across adapter processes |
| `FREELLAMA_MCP_AUTOSTART_SERVE` | `1` | When the default loopback serve endpoint does not answer, start `freellama serve` as a child of the MCP server (stopped with it): once in the background at launch, so other local clients find it, and again on a tool call if it is still down. Under the agent commands (`freellama run`, `chat`…) it starts detached instead, logging to `$FREELLAMA_HOME/serve.log` (default `~/.octocode/freellama/serve.log`), and keeps running for later calls. `0` disables |
| `FREELLAMA_WORKFLOWS_DIR` | `$FREELLAMA_HOME/workflows` | Directory of your own workflow files (`NAME.md`), read when the MCP server or an agent command starts. A file named like a built-in replaces it; an invalid one is skipped and named by `freellama workflows` and `doctor`. Format: [your own workflows](../../docs/PROMPTS.md#your-own-workflows) |
| `FREELLAMA_EMBED_INDEX` | `1` | `0` turns off the persistent embedding index that lets a repeated `nearest` search over files (find-code, duplicates) embed only new or changed chunks. Vectors live in `$FREELLAMA_HOME/embedding-index/`, one file per model, digest and dimensions. |
| `FREELLAMA_OLLAMA_CLI` | `ollama` | Path of the `ollama` binary that `freellama chat` runs |
| `FREELLAMA_SERVE_BINARY` | checkout `target/release`/`debug`, then the platform package | Binary used for serve autostart |
| `FREELLAMA_AGENT_OS_SANDBOX` | `auto` | Bash adapter OS confinement: `bwrap` on Linux, `sandbox-exec` on macOS, when present. `off` keeps only the restricted shell |
| `FREELLAMA_AGENT_OCTOCODE_PACKAGE` | `octocode@19.1.0` | Pinned npm spec the Octocode adapter runs through `npx` |
| `FREELLAMA_MCP_ALLOW_CODER` | unset (off) | `1` enables `delegate_research` `adapter:"coder"`, which edits files and runs the caller's `agent.commands` in an isolated copy of the workspace and returns a patch for the caller to review and `git apply` (see [`src/research/coder-agent.ts`](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/src/research/coder-agent.ts)) |
| `FREELLAMA_CODER_RUN_TIMEOUT_SECONDS` | `300` (at most `1800`) | Timeout of one coder `run`; the whole call is still bounded by `agent.timeoutSeconds` (coder default at least 600 s) |
| `FREELLAMA_CODER_MAX_COPY_BYTES` | `209715200` (200 MB) | Size cap of a coder copy of a workspace that is not a git repository (`node_modules`, `target`, `dist` and `.git` excluded) |
| `FREELLAMA_CODER_WRITABLE` | unset | Platform-separated extra directories the coder's allowed commands may write, such as `~/.cargo` for `cargo test`; an entry that holds or lies inside the workspace is refused |
| `FREELLAMA_CODER_UNSANDBOXED_RUN` | unset | `1` lets the coder `run` without an OS sandbox (`sandbox-exec` or `bwrap`); without one, `run` is refused |

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
install, where it is required and every call is refused until you set it). A relative `workspacePath`
resolves against the server's working directory unless that is `/` or the home directory, then is
resolved through symlinks so a link inside an allowed root can't escape
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

## Research model and sweeps

Without `model` (and without `FREELLAMA_MCP_DEFAULT_MODEL`), `delegate_research` uses the first
research-agent catalog pick (stronger, default, small) that is installed and fits in memory now;
GitHub research skips the small pick, which scored 1 of 6 there. When none fits, it takes the
installed default (then stronger) pick and lets serve's memory gate admit, wait or refuse the load;
with no pick installed it falls back to routing. The receipt's `researchModel` says which and why.
GitHub answers are grounded against the adapter's own `gh` results; local adapters check the workspace on disk. A sweep whose
server process exited reports `interrupted` with the `sweep{action:"resume", sweepId}` call, which
reuses saved chunk results; sweeps never resume on their own.

## Where an answer goes

Every result is saved whole (`response_file`; read one part with `read_response{file, section}`).

`run_task` and `task_jobs` answer with **text alone**: the answer, its `check:` lines and a closing
`response_file: <path>` line, with no `structuredContent`. Hosts such as Claude Code show
`structuredContent` in place of the text when a result has it, which handed the agent the whole
receipt (1–3k tokens) around a two-line answer. The receipt (route, model, timings, placement,
tokens) stays in the response file, one `read_response` away; `receipt:"full"` on a call returns it
inline, and `FREELLAMA_MCP_RECEIPT=structured` restores inline receipts for every call (those two
tools then declare their output schema again). The other tools always return `structuredContent`.
`run_task` and `delegate_research` take two more fields that choose what comes back inline:

- `output`: `auto` (default) returns the answer inline up to 8,000 characters (about 2,000 tokens),
  and above that a short view: the label line, the first lines of the answer, every `check:`,
  `handoff_file:`, `verification:` and `cited:` line unchanged, and the file holding the whole
  answer. `inline` always returns the whole text; `file` always returns the short view. The
  default exists because hosts limit tool output (Claude Code warns above 10,000 tokens and caps
  it at 25,000 by default) and every inline character costs the calling agent tokens.
  A shortened view also replaces the answer's text inside `structuredContent` with
  `answer moved: <output_file or read_response location>` and adds `output: {mode, chars, file}`.
- `outputPath`: also write the answer there as Markdown (the `saveAs` handoff format), returned as
  `output_file`. The caller owns the path: a new file is created and an existing one replaced. It
  must resolve inside the readable roots (the project, the temp directory, `FREELLAMA_FILE_ROOTS`),
  outside any `.git` directory, and not be a directory. It is checked before the model runs, so it
  is refused with `preview` or `defer`; with `tasks` one file holds the combined text. A failed
  write keeps the whole answer inline and names the error.

A shortened view without an `outputPath` writes the answer to a new
`${FREELLAMA_HOME}/tmp/<timestamp>-<pid>-<n>.md` (default `~/.octocode/freellama`) and ends with
`output_file: <path> (the full answer, N chars)`; a later `run_task{files:[output_file]}` can read
it. Those files are deleted after `FREELLAMA_RESPONSE_TTL_DAYS` (default 7; `0` keeps them). If
that write fails, the view points at the saved response
(`read_response{file, section:"/result/content/0/text"}`); with neither, the whole answer is
returned with a note.

`task_jobs{action:"get"}` shapes a finished deferred job the same way (its own `output`, default
`auto`). Receipts keep the routing, timing and token counts inline; telemetry and nested metrics move
to the saved `response_file`. `models{view:"installed"}` and `resident` return a slim per-model set;
`includeVerbose:true` returns the full records.
