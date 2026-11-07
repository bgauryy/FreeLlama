# Handoff: local model trials, catalog and Rust failures

Paused 2026-11-01 about 11:50 UTC at the user's request. Nothing is committed; the user handles git and publishing. 180 paths show in `git status`, much of it staged, and not all of it from this work.

## Rules from the user

- Do not overload the machine: one model at a time, and no model runs during a build. Use `nice` and limited cores for builds.
- **The user will not close VS Code.** Never plan around closing it.
- Ask for approval of one exact tag and its size before any pull. Delete only a tag the user names.
- Other sessions work on this machine. Attribute a failure by file times before changing code you did not write. Peers: `octocode-d1` (not FreeLlama), `octocode-13` (only `octocodecode` vitest).

## 1. Fix the Rust failures (done 2026-11-01)

All fixed. Two were product bugs:
- Catalog: a tag that changed at the confirmation read now gives pinned work a 409 `model_identity_changed` instead of a 502.
- Missed wake: a load waiting for a busy model to go idle now plans eviction on entry.

The rest were fixtures without the digest and `context_length` that Ollama reports. The `/api/ps` reads per cold task were cut from 8 to 6, and a test pins that number. Full checks are green: JS 936, Rust core lib 341, integration 271, CLI 14+5, plus clippy and fmt. The history below is kept for context.

Codex wrote the hardening in `docs/OLLAMA_HARDENING_2026-11-01.md`. Its sandbox blocked sockets, so these tests never ran against its changes. The user approved fixing them. They failed deterministically, not from load.

Started from 11 failures in `cargo test --workspace` (592 passed). Run them with:

```bash
nice -n 15 cargo test -p freellama-core --test integration -j 4 -- --test-threads=1 a_load_unloads_the_cheapest stale_residency_is_rechecked resident_managed_tasks_keep large_background_work cancelling_a_capacity managed_tasks_never_exceed warm_forecast_is_rechecked
```

**Fixed, passing:**

- **Real bug:** a raw call for a model that is not installed returned 503 "retry" instead of Ollama's 404, for every raw path. Fixed in `proxy/mod.rs` (`not_installed`, `not_listed`), with a new test in `tests/proxy_contract.rs`. The release binary does not have this fix yet.
- `model_identity_contract` (2): discovery now reads `/api/tags` twice, so the mock's change moved to the third read.
- `chat_task_preserves_typed_history`: the request uses `think`, so the mock now reports `thinking`.
- `raw_passthrough_stays_byte_exact`: the mock now has `/api/tags`.

**Still failing (7). Do not assume these are stale tests; find each cause first:**

| Test | Symptom | Lead (unverified) |
|---|---|---|
| `resident_managed_tasks_keep_same_model_concurrency` | Max concurrency 1, expected 2 | I added `digest` and `context_length` to its `/api/ps` mock, but its `/api/tags` mock (`platform_contract.rs` about line 1589) still has no digest. Reuse now requires identity to match |
| `managed_tasks_never_exceed_the_configured_concurrency` | Panics at `platform_contract.rs:2769` | Same mock change, tags about line 2699 |
| `stale_residency_is_rechecked...` | 5 `/api/ps` calls, expected 4 | Find what the fifth call is before changing the count. If it is unjustified, fix the code |
| `large_background_work_cannot_be_starved...`, `cancelling_a_capacity_reservation...` | `expected request to start` at `admission_fairness_contract.rs:114` | Mocks lack digest and context. Embedding and completion share one model with different default contexts, so reuse may now reload. Could be a real scheduling regression |
| `a_load_unloads_the_cheapest_idle_runner...` | `measured_load_seconds` is null, expected 2.5, `monitoring_contract.rs:541` | Read `monitor.rs:125` and where `usage.load_ms` is set. May be a real loss of load learning |
| `warm_forecast_is_rechecked_after_waiting...` | Panics at `resource_governor_contract.rs:575` | Not read yet |

Change a test only when its expectation is truly outdated. Then run the full suite and `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

## 2. Resume the head-to-head (done 2026-11-01)

**Outcome:** every model that fit ran three rounds on the new build (server on 11465, now stopped); `bonsai-2:27b` (pulled with approval) never had its 16.3 GB, so it is "not measured here". All 12 cookbook recipes pass live. The run found two product bugs, both fixed: sliced embeddings read the model name from the wrong receipt field, and an explicit `contextTokens` skipped the fit check (now `context_window_exceeded`). Results: `LOCAL_RIVALS.md`, `docs/LOCAL_MODELS.md`. The table below is the pre-run state.

Results so far are in `LOCAL_RIVALS.md`; saved state is `.octocode/tmp/rivals.json`. Rerunning resumes and skips finished models:

```bash
cd /Users/bgaryy/code/FreeLlama && node scripts/local-test/rivals.mjs http://127.0.0.1:11455 > .octocode/tmp/rivals.txt 2>&1
```

The test server on 11455 is still running (PID 30290, `--home` in the session scratchpad `home-trial`) on the older binary, from before the Codex edits. If it is gone, start one; note in the report that later results used a newer binary. Port 11435 has no server.

| Job | Results so far (3 rounds unless noted) | Still to run |
|---|---|---|
| Classify | `strands-decider` 53/56 | `nimble-v3` (needs 12.8 GB) |
| OCR | `MiniCPM-OCR` 30/30, 257 ms; `glm-ocr` 30/30, 1,885 ms | done |
| Speech to text | `lfm3-audio` and `lfm3-omni` both 1.7% word error | done |
| Text to speech | `phonon` 0.8–1.7%, 102 ms; both LFM3 0%, 950 ms (the judge is LFM3, so it may favor itself) | done |
| Image recognition | `lfm3-vl` 20/20; `lfm3-omni` 18/20 | `bonsai-2:9b` (needs 8.6 GB) |
| Code search (1 round) | `embeddinggemma-2` first 9/10, top 3 9/10, 67 s; `nomic` 6/10, 8/10, 42 s | done |
| Coding | `lfm3` 21, 20, 20 of 25 | `bonsai-2:9b`, Nemotron (13.5 GB) |
| Research agent | `lfm3-audio` 3/8 local, 3/6 GitHub | `lfm3`, `bonsai-2:9b`, Nemotron |
| Cookbook recipes run live | none yet | all 12 |
| Model profiles (caching, context, memory) | none yet | all |

Available memory has ranged from about 4 to 11 GB, so `nimble-v3` and Nemotron may never get room. Report them as "not measured here" rather than forcing them. `qwen3.6:27b-q4_K_M` was deleted on 2026-11-01 at the user's request; it needed 28.7 GB.

## 3. Then (done 2026-11-01)

**Outcome:** catalog has no "pending" entry (defaults now `embeddinggemma-2` for find-code, `lfm3` for coding); cookbooks and `docs/LOCAL_MODELS.md` updated. Final checks: fmt, clippy, `cargo test --workspace` (14 + 5 + 345 + 274), `yarn build`, typecheck, JS 948/948, integration 141 passed with 5 skipped. Logs: `.octocode/ollama-rating/final/*3.log`, `js4.log`.

1. Update `packages/rust-core/src/model_catalog.json`, replacing every "head-to-head pending" with results and changing defaults where a rival won. Likely changes: **`embeddinggemma-2` beat `nomic` on code search**, though it is slower, and `lfm3-vl` is confirmed for images. Update the default-model names in `docs/cookbooks/` to match; `test/integration/cookbooks.test.ts` checks this.
2. Fill in `docs/LOCAL_MODELS.md` from the results, and update its Octocode table.
3. Run `yarn build` (Rust release, native addon, JS), then `yarn test:js`, `yarn test:integration` and `cargo test --workspace`. As of this handoff, JS unit was 927 passed and integration 145 passed with 1 skipped; those predate this Rust fix.
4. Give the user final ratings per job. Be plain that "best" means best among the models tried, on synthetic images and generated speech.

## User decisions (2026-11-01)

- `qwen3.6:27b-q4_K_M` is deleted.
- Keep the `AGENTS.md` rule: an agent may unload a model it warmed itself.
- No changelog: `CHANGELOG.md` is deleted. Do not recreate it.
- No legacy: remove backward-compatibility code (store and snapshot migrations, legacy imports, deprecated fields and aliases). Old inputs get a clear error.
- The MCP renames stand without aliases: `ollama_manage {action:"warm"}` and `{action:"unload"}`; `code_review` is gone (use `coding`).

## Decided, and where it lives

- The user wanted one general agent model: `bonsai-2:9b`, chosen because it fits here, not on proven quality. Its first run scored 8/14.
- Shipped this session: the catalog (`models {view:"catalog"}`, `freellama catalog`), 12 cookbooks (`docs/cookbooks/`), Octocode instructions (`docs/octocode/`), the `audio` tool, `run_task` `decision`, `files` and `nearest`.
