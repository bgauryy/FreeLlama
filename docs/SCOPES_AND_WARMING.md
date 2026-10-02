# Scope history and model warming

Scopes retain message history for managed chat tasks. Warming controls runner residency through
the same managed execution boundary. Both are opt-in controls; the caller owns the conversation
instructions, task dependencies, and verification.

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
| `route_defaults` | `routeDefaults` | Task-routing defaults for later scoped execution. |
| `limits` | `limits` | Per-scope bounds within the server's configured limits. |

Messages require a supported role and can omit content, including for assistant tool calls.
When present, content must be a string.

Routing defaults accept `task`, `objective`, `model`, `required_capabilities`, `context_tokens`,
`execution_preference`, `min_placement_evidence`, and `min_confidence`. MCP uses the corresponding
camelCase field names. Defaults do not grant admission or qualify a model. Explicit task fields
take precedence, and execution checks current eligibility and resources.

The metadata receipt contains `scope_id`, `revision`, `route_defaults`, `limits`, `message_count`,
`bytes`, `estimated_tokens`, `expires_at`, and `storage:"process_local"`. Full history can include images, thinking, and tool calls; request it only when needed. Counts and estimates
describe the stored transcript; they are not an exact tokenizer result or runner-memory estimate.
Token estimates derive from the full serialized message bytes and count both media and extra fields.
This conservative estimate can refuse media-heavy history before inference; it does not measure
exact image token cost.

Fork requires `revision` and accepts optional replacement routing defaults and limit overrides. It creates a
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
context sizing; the selected model's context boundary still applies.

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

Per-scope `limits` accepts `max_messages`, `max_bytes`, `max_estimated_tokens`, and `ttl_seconds`;
MCP uses `maxMessages`, `maxBytes`, `maxEstimatedTokens`, and `ttlSeconds`. A request can lower these
bounds within server policy. Finite safety bounds remain operator-owned. Lowering runtime caps
does not summarize or trim stored transcripts; subsequent writes must satisfy the active caps.
Expired entries are removed during store operations.

## Warm API

`POST /warm`, MCP `warm_model`, and CLI `warm` load an exact installed model with a matching
non-embedding task profile. They do not accept prompts or messages, pull a model, or store conversation
history. The default task profile is `completion`.

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

Synchronous warming returns HTTP `200`; deferred warming returns HTTP `202`. Inspect deferred
work through `task_jobs` or `jobs` using its returned ID. Warming reuses ordinary routing,
admission, memory-fit checks, transition locking, deadlines, and physical-placement observations.
A cold warm request can therefore wait or refuse. Read the `warm` receipt alongside
`execution.observation` and `execution.keep_alive`; a successful load response alone does not prove
physical residency or placement. Use configured placement for an initial load,
inspect its receipt, and require observed placement for subsequent work when placement matters.

With `keep_alive:"0"`, warming observes placement and then unloads. The `warm` receipt describes
residency after unload: `loaded:false` on verified unload, with
`residency_source:"ollama_api_ps_after_unload"`.

Use a context/task profile compatible with the following task. Warming a different profile can
require another runner transition. Warm residency does not reserve a later execution slot. Cache reuse depends on the compatible
profile, unchanged prefix, and inference backend. See [Managed execution](ARCHITECTURE.md#managed-task-execution).

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
| `base_seconds` | `FREELLAMA_KEEP_ALIVE_BASE_SECONDS` | `60` | Base duration before measured reuse and load cost. |
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
