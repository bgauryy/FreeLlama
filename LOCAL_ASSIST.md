# LOCAL_ASSIST: local models doing an agent's routine work on a real repository

Real data: the octocode repository (1025 Rust source files of its native crates) and its
40 most recent GitHub issues. Apple M2 Pro, 32 GB, Ollama 0.40.13, FreeLlama 0.8.0, with
other work running; each step waited for FreeLlama to admit its model. Ground truth for issue
locations and labels was set by reading the code and issues, before running.

## 1. From a real issue to the code it concerns (embeddings)

Indexed 1025 source files (7.5 MB, about 1,880,044 tokens) as one card each in 27.2 s.

| Issue | Top 5 files | Right area in top 5 | Query ms |
|---|---|---|---:|
| #477 A source folder named `credentials` is withheld as if it wer | `runtime/src/policy/discovery.rs`<br>`runtime/src/policy/discovery/withheld.rs`<br>`engine/src/security/detector/credential_values.rs`<br>`runtime/src/policy/prune.rs`<br>`runtime/src/tools.rs` | ✅ | 52 |
| #450 local* tools: misleading "Symlink target ... is outside allo | `runtime/src/policy/path/messages.rs`<br>`runtime/src/policy/path.rs`<br>`runtime/src/private_file.rs`<br>`runtime/src/policy/path/walk.rs`<br>`runtime/src/tools/lsp_search/scope/paths.rs` | ✅ | 55 |
| #479 lspSearch documentSymbols waits ~400 ms per file it doesn't  | `engine/src/lsp/client.rs`<br>`engine/src/lsp/client/sync.rs`<br>`engine/src/lsp/client/open_documents.rs`<br>`engine/src/lsp/transport/push_diagnostics.rs`<br>`runtime/src/tools/lsp_search/ops.rs` | ✅ | 49 |
| #480 lspSearch callers: an implementing method's own declaration  | `runtime/src/tools/lsp_search/call_sites.rs`<br>`runtime/src/tools/lsp_search/walk/caller_names.rs`<br>`engine/src/signatures/js_oxc/call_sites.rs`<br>`runtime/src/tools/leads/lsp.rs`<br>`runtime/src/tools/lsp_search.rs` | ✅ | 46 |
| #478 Graph: calls to private methods (`this.#x()`) are not record | `engine/src/signatures/js_oxc_calls.rs`<br>`engine/src/signatures/js_oxc_references.rs`<br>`engine/src/signatures.rs`<br>`engine/src/signatures/graph_facts/collect.rs`<br>`engine/src/signatures/js_oxc_calls/names.rs` | ✅ | 49 |
| #473 Feature: Shared bare clone with on-demand worktree materiali | `runtime/src/tools/gh_clone_repo.rs`<br>`runtime/src/tools/gh_clone_repo/git.rs`<br>`runtime/src/tools/gh_clone_repo/plan.rs`<br>`runtime/src/tools/gh_clone_repo/recovery.rs`<br>`runtime/src/tools/gh_clone_repo/checkout.rs` | ✅ | 54 |
| #304 Bug: localSearchCode mode="discovery" fails with ripgrep fla | `runtime/src/tools/source_view/extraction/search_lead.rs`<br>`engine/src/search/rust_ranges.rs`<br>`engine/src/structural/files.rs`<br>`runtime/src/tools/ast_rule.rs`<br>`runtime/src/tools/local_search/executor.rs` | ✅ | 51 |

**7/7** issues put the right area in the top 5. An agent reads 5 short file cards (~4,375 tokens) instead of searching 1025 files.

## 2. Related or duplicate issues (embeddings)

| Issue | Nearest other issue | Expected | Match |
|---|---|---|---|
| #140 | #158 (0.769) | #158 | ✅ |
| #158 | #140 (0.769) | #140 | ✅ |
| #257 | #323 (0.866) | #323 | ✅ |
| #323 | #257 (0.866) | #257 | ✅ |
| #289 | #58 (0.786) | #452 | ❌ |
| #452 | #413 (0.812) | #289 | ❌ |
| #411 | #443 (0.697) | #412 | ❌ |
| #412 | #321 (0.704) | #411 | ❌ |
| #414 | #415 (0.807) | #415 | ✅ |
| #415 | #414 (0.807) | #414 | ✅ |
| #357 | #429 (0.816) | #429 | ✅ |
| #429 | #357 (0.816) | #357 | ✅ |

**8/12** found their related issue as the nearest neighbour.

## 3. Triage of 40 real issues (decision model)

**35/36** matched my labels in 16.6 s total. Misses: #411 security→bug.

## 4. Reading real screens (OCR)

| Screen | Expected text | Read back | ms |
|---|---|---|---:|
| FreeLlama status page | FreeLlama | ✅ — FreeLlama holding for memory updated 7:13:29 PM GPU BACKEND Units in use 0 / 2 Queued 0 / 16 Oldest wait - Circuit close | 39199 |
| Terminal: test run summary | Tests  838 passed, Test Files  29 passed | ✅ — ```bash RUN v5.0.5 /Users/bgaryy/code/FreeLlama [freellama serve] store 3 ms [freellama serve] background start failed:  | 9378 |
