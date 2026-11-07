# Deferred jobs and recovery

FreeLlama's deferred jobs keep one accepted task identity through polling, cancellation and result retrieval. A missing result or an uncertain outcome never authorizes automatic inference replay. Ollama still owns execution and physical cancellation; these records coordinate the FreeLlama worker.

## Enable durable receipts

Persistence is disabled by default. Set `FREELLAMA_DURABLE_JOBS=true` (or `1`) before starting the server with its file-backed SQLite store. Rust embedders can use `PlatformConfig::with_store(store).with_durable_jobs(true)`. Enabling it without a file-backed store fails server construction. `false` and `0` disable it; invalid environment values fail configuration.

The journal opens an independent connection to that database, using SQLite WAL with `synchronous=FULL`. The ordinary usage/learning connection keeps its existing synchronization setting. Acceptance is committed before a new worker is spawned. This uses SQLite's durability guarantees subject to the filesystem and hardware honoring synchronization; it is not a distributed execution lease.

The journal persists receipts, bounded results/errors, and keyed hashes of request identity and input (see [request hashes](#request-hashes)). It does not save executable requests for replay. Results and upstream errors may themselves contain sensitive user content. Database access and retention therefore need the same care as task output storage.

## Submit, inspect and cancel

`POST /_freellama/v1/tasks` with `defer:true` returns `deferred:true`, a `job` containing its `id`, and `retention` limits. Use `GET /_freellama/v1/jobs/{id}` for its result and `GET /_freellama/v1/jobs` for a metadata-only list. The list strips result/error payloads in SQLite before loading recovered records into the control plane.

An embedding task may carry `nearest` (`{query?, top?, min_score?, labels?}`, with `return_embeddings` to keep the vectors). The core embeds the items (the query first, in slices of 32 pinned to one verified runner identity), ranks them by cosine similarity and stores `nearest: {mode, matches|pairs}` in place of vectors, so a deferred job's result is ranked and small. The whole input is one job and must fit the 16 MiB job input limit; without a query at most 4,000 items are compared pairwise. A malformed `nearest` is refused at submission. Through MCP, `run_task {defer:true, tasks:[...]}` submits every item as its own job and returns `{deferred:true, jobs:[{id, job}]}`; the HTTP `/tasks/batch` endpoint still refuses deferred items.

`POST /_freellama/v1/jobs/{id}/cancel` requests cancellation and waits for the attached worker to settle and release local permits. It also waits for pending terminal persistence. A result already committed by the execution path retains its completed status. Acknowledged local cancellation is not proof that Ollama physically stopped accepted work.

`DELETE /_freellama/v1/jobs/{id}` first settles locally owned work, then hides the record and removes its retained payload. With persistence enabled, its idempotency tombstone remains until expiry. An unattached worker with an unknown outcome cannot be removed early. A failed terminal database commit also prevents removal of its still-unfinished durable record.

MCP task cancellation and managed research use addressable jobs regardless of whether the caller requested progress notifications. The MCP wrapper retains accepted job IDs after polling, cancellation or result errors; it does not replay them synchronously. Managed research and MCP task calls reject definitive pre-execution job refusals with `cancellable_task_unavailable`, `execution_outcome:"not_started"` and `retry_safe:true`. They do not switch to unaddressable synchronous execution.

`cancellation_state:"unknown"` means the acknowledgement is unknown; a request may have reached the server even if its response was lost. `cancellation_requested:true` records the local attempt, not confirmed delivery or physical stop. Native MCP polling currently has no abort signal: cancellation dispatch starts immediately, but an in-flight job GET can delay the caller's error until that GET returns or its control timeout expires (30 seconds by default, configurable with `FREELLAMA_CONTROL_TIMEOUT_SECONDS`). The five-second cancellation-report cap does not interrupt that pending native poll.

## Idempotency

With persistence enabled, the HTTP task request may include `idempotency_key`, containing 1–128 UTF-8 bytes. It requires `defer:true`; synchronous requests and servers without durable jobs reject the field. The current MCP tool schema does not add an idempotency-key argument.

Keys are hashed (HMAC-SHA256, see [request hashes](#request-hashes)) with authenticated caller identity and scoped to a server topology: listen address, primary endpoint, CPU endpoint and configured CPU model list. Keep that configuration and caller identity stable when recovering jobs. Task input is serialized and hashed after removing the key, together with the set of explicitly supplied fields. Retries must preserve both values and field presence: omission can inherit scope defaults while an explicit default overrides them. Reusing a key with changed input returns `task_job_idempotency_conflict`.

A repeated key returns the original job, including a retained terminal result, before checking local registry capacity. Claiming the key and reserving its durable record is one transaction. Concurrent requests cannot both claim that key. A new durable claim rejected by local capacity is recorded as failed with `execution_outcome:"not_started"` and `retry_safe:true`; repeating the key returns that original refusal instead of starting work.

After restart, only recorded terminal outcomes are recoverable as settled results. An unfinished record becomes a read-time `failed` receipt with reason `execution_outcome_unknown`, error code `task_job_outcome_unknown`, its original `job_id`, and `retry_safe:false`. It may represent an interrupted worker, a worker still attached to another server, or a submission interrupted around acceptance. No worker is resumed or recreated. If the original worker later commits a terminal record, subsequent retrieval can observe it.

Idempotency applies only while the record is retained. After expiry, a key can start new work. Changing topology, caller identity or database also changes the deduplication boundary. Use one coordinating FreeLlama authority per topology; durable receipts do not coordinate model resources across servers or provide exactly-once execution.

## Request hashes

The request identity (caller identity plus idempotency key) and the serialized task input are stored only as HMAC-SHA256 digests, domain-separated for the two purposes. The key is a 32-byte per-installation secret in a file beside the database, `<database file name>.job-key`, never in the `task_jobs` table. Someone who can read only the database cannot confirm a guessed key or prompt by hashing it. Someone who can read the secret file can, so give it the permissions of the database itself and back both up together.

The file is created on first durable use with mode `0600` on Unix. The complete key is written under a private temporary name and hard-linked into place, so concurrent starts never replace one another's key. A file with looser permissions is tightened to `0600` when read. A file that is not exactly 32 bytes fails server construction instead of being replaced, because replacing it would silently end deduplication for every retained key.

Idempotency survives restart as long as the same secret file sits beside the same database. Deleting or replacing the file, or moving the database without it, changes every digest: retained keys stop matching and a retry starts new work. There is no recognition of rows written under earlier unkeyed hashes; those rows do not match and expire normally. The server-topology namespace remains a plain SHA-256 of non-secret configuration. It scopes keys and is not a secret.

## Persistence states and retention

Execution status and storage status are separate. Each local receipt exposes `persistence.state` and `persistence.stage`:

| State | Meaning |
|---|---|
| `disabled` | Only process-local retention is enabled |
| `confirmed`, stage `acceptance` | The original queued record committed; this does not confirm an eventual result |
| `pending`, stage `terminal` | The local worker settled, but its terminal database transaction has not finished |
| `confirmed`, stage `terminal` | The terminal transaction committed; normal payload eviction can still apply |
| `failed`, stage `terminal` | The local result/error is available, but its commit failed; `persistence_error` gives the failure |

Consumers requiring restart recovery must check terminal persistence confirmation, not only `status:"completed"`. A local result can be returned while persistence is pending. The database retains the original acceptance if terminal persistence fails, so restart recovery reports an unknown outcome rather than inventing completion.

| Limit | Contract |
|---|---|
| Local handles | 64 total; settled handles may be evicted to admit new work |
| Durable records | 1,024 across the entire database, including other topology namespaces and removed tombstones |
| Durable receipt budget | 32 MiB of serialized receipts across that database |
| Durable metadata | 16 KiB per accepted/terminal receipt excluding terminal result/error payloads |
| Individual result or error | 2 MiB of encoded JSON |
| Job input | 16 MiB of encoded JSON per job and 64 MiB across queued and running jobs (`task_jobs_input_budget`, retry after 5 s); executable input is not retained in the journal |
| Terminal retention | 600 seconds after terminal commit; local retention starts when the worker settles |
| Unfinished retention | Accepted timeout plus 600 seconds from claim |
| Database workers | At most four blocking journal operations per process; canceled awaiters do not release a running operation's permit early |

When the receipt budget is exceeded, the journal removes older terminal payloads while preserving job identity and its unexpired key. Such a receipt has `status:"failed"`, reason `retained_payload_unavailable`, `original_status`, and error `task_job_result_evicted` with `retry_safe:false` and `result_recoverable:false`. This describes unavailable retained output; `original_status` preserves whether execution completed, failed or was cancelled. Explicit removal uses `task_job_result_removed` on a repeated key. Neither operation starts a replacement inference. A still-retained local handle may continue to provide its in-memory result even if the database payload has been evicted.

Once all durable slots are occupied by unexpired keys, new claims fail; keys are not discarded to admit unrelated work. Expired rows are pruned on a later claim. These are logical serialized-data limits, not exact process-RSS or SQLite-file-size bounds. WAL, indexes, allocator overhead and reusable database pages add storage overhead; expiry and removal are not secure erasure.

## Validation

Focused tests exercise keyed hash stability across restart, per-installation secrets, secret-file permissions and refusal of a malformed secret, concurrent same-key claims, file-backed restart recovery, unknown outcomes, FULL-sync isolation, deduplication at local capacity, terminal result delivery, refused-work accounting, bounded payload eviction, hidden tombstones, slim listing, persistence failures and cancellation during database work. They use no live models. See [testing](TESTING.md) for the full project checks.
