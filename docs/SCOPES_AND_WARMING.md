# Scope history and model warming

Use scopes when delegated chat tasks need retained message history, and warming when an installed
model needs preparation before related work. FreeLlama manages these controls through the same
admission and memory checks as task execution. The calling agent owns conversation instructions,
task dependencies, and verification.

## Ownership

| State | Owner | Lifetime and meaning |
|---|---|---|
| Session affinity | FreeLlama `session` | Bounded, idle-expiring model preference; stores no messages or KV. |
| Scope history | FreeLlama `scope` | Bounded process-local messages and routing defaults, protected by a revision. |
| Runner residency | Ollama | Controlled by `keep_alive`, loading, eviction, and available resources. |
| Prefix cache and KV | Ollama and its inference backend | Opportunistic reuse of compatible requests; no scope-owned KV save/restore contract. |

Reusing a scope replays its stored messages. Switching scope IDs does not transfer KV tensors.
Keeping an identical message prefix can preserve cache reuse when the backend supports it; changing
the model, runner profile, or prefix can invalidate that reuse. A resident model does not imply that
the requested history is cached. Cache counts remain unknown when Ollama does not report them.

## Scope API

All HTTP paths below are relative to `/_freellama/v1`. Authentication matches the other control
routes. Bodies reject unknown fields.

| Operation | HTTP | MCP | CLI |
|---|---|---|---|
| Create | `POST /scopes` | `scope {action:"create"}` | `scope create` |
| Inspect metadata | `GET /scopes/:id` | `scope {action:"get",scopeId}` | `scope get --scope-id SCOPE_ID` |
| Inspect history | `GET /scopes/:id?include_messages=true` | `scope {action:"get",scopeId,includeMessages:true}` | `scope get --scope-id SCOPE_ID --include-messages` |
| Fork a snapshot | `POST /scopes/:id/fork` | `scope {action:"fork",scopeId,revision}` | `scope fork --scope-id SCOPE_ID --revision REVISION` |
| Delete | `DELETE /scopes/:id` | `scope {action:"delete",scopeId}` | `scope delete --scope-id SCOPE_ID` |

Replace `SCOPE_ID` with the returned scope identifier and `REVISION` with its current revision.
Deletion returns HTTP `204`. Create and fork return HTTP `201` with metadata; history requires an
explicit read. CLI create and fork accept a JSON object through `--json`.

Create accepts the following optional fields:

| HTTP field | MCP field | Meaning |
|---|---|---|
| `messages` | `messages` | Initial Ollama messages, preserving fields, content, and order. |
| `route_defaults` | — | Task-routing defaults for later scoped execution (HTTP/CLI only). |
| `limits` | — | Per-scope bounds within the server's configured limits (HTTP/CLI only). |

Messages require a supported role and can omit content, including for assistant tool calls.
When present, content must be a string.

Routing defaults accept `task`, `objective`, `model`, `required_capabilities`, `context_tokens`,
`execution_preference`, `min_placement_evidence`, and `min_confidence`. MCP callers set routing per
`run_task` call instead. Defaults do not grant admission or qualify a model. Explicit task fields
take precedence, and execution checks current eligibility and resources.

The metadata receipt contains `scope_id`, `revision`, `route_defaults`, `limits`, `message_count`,
`bytes`, `estimated_tokens`, `expires_at`, and `storage:"process_local"`. Full history can include images, thinking, and tool calls; request it only when needed. Counts and estimates
describe the stored transcript; they are not an exact tokenizer result or runner-memory estimate.
Token estimates derive from the full serialized message bytes and count both media and extra fields.
This storage-budget estimate can refuse media-heavy history before inference; it does not measure
image token cost or determine the model context required to execute the history.

Fork requires `revision`; HTTP/CLI forks also accept replacement routing defaults and limit overrides. It creates a
separate history snapshot with revision `0`; later writes to either scope do not change the other.
Fork independent branches before parallel execution instead of submitting concurrent writes to one
scope.

## Scoped task execution

HTTP `POST /tasks` accepts `scope_id` and `scope_revision` together. MCP `run_task` uses `scopeId`
and `scopeRevision`; CLI `task` uses `--scope-id` and `--scope-revision`. Send only the new caller
messages or prompt. FreeLlama prepends the stored transcript and appends the successful assistant
response to that scope. Embedding tasks and embedding inputs cannot use scopes. MCP previews
reject scope references; use a separate execution call.

The expected revision prevents a stale caller from overwriting another result. A scope permits one
active append; a competing write fails before inference. A successful task returns a `scope`
metadata receipt with the next revision. Failed or cancelled tasks do not append history. A scope
deleted or expired during execution cannot receive the result. History limits also apply to the
completed assistant message: a response that exceeds them returns an append error with the prior
revision intact even though inference has run. Inspect the failure before retrying.

Deleting a session releases its affinity without deleting scope history or invalidating an
already-running scope append. Killing a session cancels its associated work; cancelled work does
not append history.

Limits reject an oversized transcript instead of silently deleting earlier instructions. If a task
cannot fit, narrow its input, create a compact replacement scope, or fork a suitable snapshot. The
caller owns any semantic summary and its verification. Stored history also contributes to managed
context sizing; the selected model's context boundary still applies. Scoped image or audio input
requires explicit `context_tokens` (`contextTokens` through MCP). Its context receipt estimates
text and reports total multimodal token cost as unknown. Encoded media is retained in history,
but its bytes are not counted as text tokens. Text plus the finite output reserve must fit the
requested window; Ollama determines the remaining model-specific fit with truncation and shifting
disabled. A passing estimate alone does not prove the complete multimodal input fits.

Scopes are process-local. Restarting `serve` discards them. Scope identifiers are handles within one
operator service, not tenant authorization credentials. Apply external tenant isolation where
required; see [Production deployment](PRODUCTION.md).

Scope failures retain a stable code:

| Status | Code | Meaning |
|---|---|---|
| `404` | `scope_not_found` | Missing, deleted, or expired scope. |
| `409` | `scope_revision_conflict` | The expected revision differs from the stored revision. |
| `409` | `scope_busy` | Another append owns the scope; fork for parallel work. |
| `413` | `scope_history_limit_exceeded` | Complete history exceeds a per-scope bound. |
| `422` | `scope_reference_invalid` | Scope identifier and expected revision were not supplied together. |
| `422` | `scope_limit_invalid` | A requested limit exceeds operator caps or is zero. |
| `422` | `scope_embedding_unsupported` | An embedding task or input attempted to use history. |
| `429` | `scope_capacity_exceeded` | The store has reached its count or aggregate-byte bound. |
| `502` | `scope_response_invalid` | A completed valid assistant message was not returned. |

A minimal CLI sequence creates a prefix and submits its first turn:

```bash
npx @octocodeai/freellama scope create \
  --json '{"messages":[{"role":"system","content":"Answer concisely."}]}'
npx @octocodeai/freellama task --scope-id SCOPE_ID --scope-revision 0 \
  "Explain the previous instruction."
```

Replace `SCOPE_ID` with the identifier returned by `scope create`; use the returned revision for
each subsequent task.

## Scope configuration

The runtime file accepts a `[scopes]` table. Matching `FREELLAMA_SCOPE_*` environment values take
precedence. Inspect effective values and their sources with
`config`; reload behavior follows [Runtime configuration](MONITORING.md#change-settings-without-a-restart).

| Setting | Environment variable | Default | Meaning |
|---|---|---:|---|
| `max_count` | `FREELLAMA_SCOPE_MAX_COUNT` | `128` | Maximum retained scopes. |
| `max_messages` | `FREELLAMA_SCOPE_MAX_MESSAGES` | `256` | Maximum messages in one scope. |
| `max_bytes` | `FREELLAMA_SCOPE_MAX_BYTES` | `1048576` | Maximum serialized history bytes in one scope. |
| `max_estimated_tokens` | `FREELLAMA_SCOPE_MAX_ESTIMATED_TOKENS` | `32768` | Maximum estimated tokens in one scope. |
| `total_max_bytes` | `FREELLAMA_SCOPE_TOTAL_MAX_BYTES` | `16777216` | Aggregate retained history-byte budget. |
| `ttl_seconds` | `FREELLAMA_SCOPE_TTL_SECONDS` | `3600` | Idle expiry since create, fork, or successful append; reads do not renew it. |

Per-scope HTTP/CLI `limits` accepts `max_messages`, `max_bytes`, `max_estimated_tokens`, and
`ttl_seconds` and can only lower these bounds within server policy. Finite safety bounds remain operator-owned. Lowering runtime caps
does not summarize or trim stored transcripts; subsequent writes must satisfy the active caps.
Expired entries are removed during store operations.

## Warm API

`POST /warm`, MCP `ollama_manage {action:"warm"}`, and CLI `warm` load an exact installed model with a matching
task profile; `task: "embedding"` loads an embedding model with an empty `/api/embed` request. A
chat warm may carry `messages` (MCP: `systemPrompt` and/or `messages`): the runner evaluates them
into its prompt cache with `num_predict: 0` and generates nothing, so a later request that starts
with the same messages, at the same context, reuses that work (its receipt reports
`cached_prompt_tokens`). Warming never pulls a model or stores conversation history. The default task
profile is `completion`. Ollama has no load-only request for decision models: they load on their
first `/v1/systemone` call, and `keep_alive` on that call sets their residency.

| HTTP field | MCP field | Meaning |
|---|---|---|
| `model` | `model` | Required exact installed tag. |
| `task` | `task` | Optional capability/profile selection. |
| `context_tokens` | `contextTokens` | Context profile used for warming. |
| `execution_preference` | `executionPreference` | Guarded preference within operator assignments. |
| `min_placement_evidence` | `minPlacementEvidence` | Required configured or observed placement evidence. |
| `priority` | `priority` | Admission scheduling class. |
| `keep_alive` | `keepAlive` | Explicit residency control; overrides adaptive selection. |
| `max_wait_seconds` | `maxWaitSeconds` | Shorter admission/resource wait budget. |
| `timeout_seconds` | `timeoutSeconds` | Total request deadline, capped by server policy. |
| `defer` | `defer` | Return a job handle for the same managed operation. |
| `messages` | `systemPrompt`, `messages` | A prompt prefix to evaluate into the runner's prompt cache. |

A warm of a model that is not resident is a load. Like any load it takes the backend's load lock
(see [Load lock and use lease](CPU_GPU_ROUTING.md#load-lock-and-use-lease)) until its runner is
observed in `/api/ps`, so it queues behind another load on the same backend but never waits for
another model's decoding. It makes room only by unloading idle runners: a model with a running
task is never evicted for a warm.

Synchronous warming returns HTTP `200`; deferred warming returns HTTP `202`. Inspect deferred
work through `task_jobs` or `jobs` using its returned ID. Warming reuses ordinary routing,
admission, memory-fit checks, transition locking, deadlines, and physical-placement observations.
A cold warm request can therefore wait or refuse. Read the `warm` receipt alongside
`execution.observation` and `execution.keep_alive`; a successful load response alone does not prove
physical residency or placement. Use configured placement for an initial load,
inspect its receipt, and require observed placement for subsequent work when placement matters.

`warm.loaded` reports observed residency independently of CPU/GPU placement: `true` when the exact
runner is present, `false` when a successful residency query finds it absent, and `null` when that
query is unavailable. A resident runner can still have a placement mismatch. Managed non-streaming
chat requires Ollama's `done:true`; an absent or non-boolean completion marker is an upstream error.
These are separate observations in Ollama's [chat response](https://docs.ollama.com/api/chat) and
[running-model inventory](https://docs.ollama.com/api/ps).

With `keep_alive:"0"` on a model that is not resident, warming loads it, observes placement and then
unloads. The `warm` receipt describes residency after unload: `loaded:false` on verified unload,
with `residency_source:"ollama_api_ps_after_unload"`. On a model that is already resident,
`keep_alive:"0"` is a plain unload: it waits for that model's running tasks, unloads it and returns
`{"unloaded": true, "status": "unloaded", "unload_ms": …}` without loading or routing anything.
`POST /_freellama/v1/unload {"model": …}` (CLI `unload`) does the same and never loads a model that is
not resident (`status: "not_resident"`).

A warm without `task` loads an embedding-only model as an embedding task, so
`{"model":"nomic-embed-text:v1.5"}` works without `"task":"embedding"`. Warms and preloads teach
FreeLlama a model's load time but do not count as uses, so they never lengthen their own residency.

Warming uses the same truncation and context-shift policy as managed chat. This preserves a
compatible runner profile instead of forcing a transition solely because warming omitted those
controls. Use a context/task profile compatible with the following task. Warming a different profile can
require another runner transition. Warm residency does not reserve a later execution slot. Cache reuse depends on the compatible
profile, unchanged prefix, and inference backend. See [Managed execution](ARCHITECTURE.md#managed-task-execution).

## Preload

The runtime file's `[preload]` table loads models without a caller asking:

- `startup`: tags, or tables with `model` and optional `context_tokens`, `keep_alive` (default: the
  warming policy's `max_seconds`) and `messages`, loaded in order once the server is up and again
  for entries a reloaded runtime file adds. `FREELLAMA_PRELOAD` sets a comma-separated list.
- `predictive` (on by default): every switch from one model to another within
  `follow_window_seconds` (600) is counted in the shared database. Once at least `min_observations`
  (3) switches away from a model happened and `min_probability` (0.5) of them went to one successor,
  finishing a caller's work on the model loads that successor in the background, but only when it
  is not loaded, fits without unloading anything (budget, GPU and host memory), the host is under no
  pressure, and nothing is running or queued on the backend. Only the primary backend predicts.
  `FREELLAMA_PREDICTIVE_PRELOAD=0` turns it off.

  Predictive warm is refused on a discrete-GPU or remote backend. Vendor-wide free VRAM does not
  say which devices that Ollama process may allocate on, and nothing in FreeLlama verifies it
  (`gpu_capacity_scope` stays `host_aggregate_not_verified_for_backend`), so a speculative load
  could displace work on a pool it cannot see. The skip reason is `GPU device eligibility for this
  Ollama backend is unverified` (`opportunistic_warm_skipped`). Unified-memory hosts and the CPU
  backend, where host memory is the budget, still predict. Explicit warms, startup preloads and
  managed tasks on a discrete GPU run through ordinary admission.

Each preload is a background warm under the usual admission, memory and pressure rules, and the
residency audit records it as `preload` (or `preload_failed`) with trigger `startup` or
`predicted_after:<model>`. Switches are counted per process from the order requests finish, so
several agents interleaving models add noise that the probability threshold has to outweigh.

For a bounded warm operation, replace `MODEL_TAG` with an exact installed tag:

```bash
npx @octocodeai/freellama warm --model MODEL_TAG \
  --context-tokens 4096 --keep-alive 60s --timeout-seconds 45 --defer
```

## Adaptive keep-alive

When a managed request omits `keep_alive`, FreeLlama selects a finite duration from measured reuse
and load cost, within the runtime `[warming]` bounds. Explicit per-call values take precedence,
including immediate unload (`"0"`) and indefinite residency (`"-1"`). Use
`--keep-alive=-1` in the CLI so the negative value remains attached to its flag. The configured eviction policy and available
resources still govern whether retaining a runner is useful.

Matching environment values override `[warming]` file values.

| Setting | Environment variable | Default | Meaning |
|---|---|---:|---|
| `min_seconds` | `FREELLAMA_KEEP_ALIVE_MIN_SECONDS` | `15` | Minimum automatic duration. |
| `max_seconds` | `FREELLAMA_KEEP_ALIVE_MAX_SECONDS` | `900` | Maximum automatic duration. |
| `base_seconds` | `FREELLAMA_KEEP_ALIVE_BASE_SECONDS` | `300` | Base duration before measured reuse and load cost; matches Ollama's five-minute default. |
| `reuse_gain_seconds` | `FREELLAMA_KEEP_ALIVE_REUSE_GAIN_SECONDS` | `30` | Additional duration per recent use. |
| `load_multiplier` | `FREELLAMA_KEEP_ALIVE_LOAD_MULTIPLIER` | `2.0` | Multiplier for measured load seconds. |
| `pressure_factor` | `FREELLAMA_KEEP_ALIVE_PRESSURE_FACTOR` | `0.25` | Duration factor while observed resource pressure holds admission. |

Automatic duration starts from `base_seconds + recent_uses × reuse_gain_seconds + load_seconds ×
load_multiplier`. A pressure hold applies `pressure_factor`; minimum and maximum bounds clamp the
result, rounded to whole seconds. Recent-use weight decays with the residency tracker; the load
measurement comes from completed managed requests. These controls tune retention rather than model quality, CPU/GPU assignment, or host-memory
reserves. Inspect `execution.keep_alive` policy evidence instead of inferring residency from a model name.

Cold and warm helper-model measurements demonstrate feasibility on a specific host and request
profile. They do not establish a universal speedup, justify lowering production reserves, or
qualify sustained-load behavior. Compare representative cold, warm, scope-switching, and parallel
work before changing policy; see [Testing](TESTING.md).
