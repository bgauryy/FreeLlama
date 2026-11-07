# LOCAL_OCTOCODE: can a local model use Octocode from the concise instructions?

Instructions: docs/octocode/LOCAL.md and docs/octocode/GITHUB.md. Each question ran through delegate_research with the local model as the agent. One run per question on one machine, not a benchmark.

## Local tools: `lfm3-audio:2b-q4_K_M`, adapter `octocode`, 2026-11-01T10:03:37.416Z

| # | Question | Right | Calls | Tool trail | Verdict | s | Answer |
|---|---|---|---:|---|---|---:|---|
| L1 | Which source file samples the programs holding the most memory? Name t | ✅ | 2 | localSearch → localSearch | verify | 8 | packages/rust-core/src/platform/resources/holders.rs |
| L2 | Which function builds the request body sent to /v1/systemone? Give its | ❌ | 2 | lspSearch → structureSearch | verify | 8 | The function that builds the request body sent to /v1/systemone is likely named 'build_request_body' and is located in the file 'AGENTS.md'  |
| L3 | What is the value of the constant EMBEDDINGS_PER_REQUEST? | ❌ | 2 | lspSearch → structureSearch | verify | 9 | The constant EMBEDDINGS_PER_REQUEST is not present in the provided code or evidence. The search for this constant returned no results, and t |
| L4 | Which files call the function memory_holders? List them. | ✅ | 3 | lspSearch → lspSearch → localSearch | verify | 12 | packages/rust-core/src/platform/resources/holders.rs, packages/rust-core/src/platform/resources/mod.rs, packages/rust-core/src/platform/exec |
| L5 | What are the variants of the ModelType enum, and which file defines it | ✅ | 2 | localSearch → localSearch | verify | 6 | The variants of the ModelType enum are ModelType::EmbeddingOnly, ModelType::Multimodal, ModelType::Generative, ModelType::Decision, and Mode |
| L6 | What does the function requested_context in routing/profile.rs return  | ❌ | 8 | astSearch → astSearch → astSearch → astSearch → astSearch → astSearch → astSearch → astSea | verify | 111 | The function requested_context in routing/profile.rs returns information related to decision tasks, as evidenced by its declaration in the f |
| L7 | Which MCP tools does packages/mcp/src/index.ts register? List their re | ❌ | 8 | lspSearch → localFetch → lspSearch → localFetch → lspSearch → localFetch → lspSearch → loc | verify | 16 | The packages/mcp/src/index.ts register function registers the following MCP tools: installProcessLifecycle, delegate_research, audio, and ol |
| L8 | Is there a function named frobnicate_widgets anywhere in this reposito | ✅ | 8 | lspSearch → lspSearch → localSearch → localSearch → astSearch → localSearch → astSearch →  | verify | 14 | no |

**4/8** right; 4.4 calls per question; narrowed before reading on 8/8.

## GitHub tools: `lfm3-audio:2b-q4_K_M`, adapter `github`, 2026-11-01T10:06:41.420Z

| # | Question | Right | Calls | Tool trail | Verdict | s | Answer |
|---|---|---|---:|---|---|---:|---|
| G1 | In the GitHub repository ollama/ollama, which file defines the type Sy | ✅ | 8 | ghSearchCode → ghGetFileContent → ghGetFileContent → ghGetFileContent → ghGetFileContent → | verify | 33 | api/types.go defines the type SystemOneRequest (line 604 in the file) |
| G2 | In ollama/ollama, what JSON fields does SystemOneRequest have? List th | ❌ | 1 | ghSearchCode | verify | 12 | type Questions, type ( Request = api.SystemOneRequest Question = api.SystemOneQuestion ) |
| G3 | Which pull request number added the System One scoring API to ollama/o | ❌ | 0 |  | escalate | 16 |  |
| G4 | Which GitHub repository hosts the npm package zod? Answer owner/repo. | ✅ | 1 | artifactSearch | verify | 8 | colinhacks/zod |
| G5 | According to the docs in ollama/ollama, what is the default response_f | ❌ | 2 | ghSearchCode → ghSearchCode | verify | 19 | application/json |
| G6 | What is the main programming language of the GitHub repository ollama/ | ❌ | 4 | ghSearchCode → ghSearchCode → ghStructure → ghStructure | verify | 33 | Python |

**2/6** right; 2.7 calls per question; narrowed before reading on 5/5.

## Local tools: `bonsai-2:9b`, adapter `octocode`, 2026-11-01T10:19:48.277Z

| # | Question | Right | Calls | Tool trail | Verdict | s | Answer |
|---|---|---|---:|---|---|---:|---|
| L1 | Which source file samples the programs holding the most memory? Name t | ❌ | 5 | localSearch → localSearch → localSearch → localSearch → localSearch | verify | 52 | scripts/local-test/lifecycle.mjs |
| L2 | Which function builds the request body sent to /v1/systemone? Give its | ✅ | 4 | localSearch → localSearch → localFetch → localFetch | verify | 26 | The function is `build_decision_request` in `packages/rust-core/src/platform/execution/request.rs`. It returns a tuple with the endpoint `/v |
| L3 | What is the value of the constant EMBEDDINGS_PER_REQUEST? | ❌ | 3 | localSearch → localSearch → localSearch | verify | 20 | The constant EMBEDDINGS_PER_REQUEST was found in one of the 335 files scanned, but the specific file path and line number were not included  |
| L4 | Which files call the function memory_holders? List them. | ✅ | 2 | lspSearch → localSearch | verify | 29 | The function memory_holders is called in: - packages/rust-core/src/platform/execution/pipeline.rs (line 247, 251) - packages/rust-core/src/p |
| L5 | What are the variants of the ModelType enum, and which file defines it | ❌ | 8 | localSearch → localSearch → localSearch → localSearch → localFetch → localFetch → localFet | verify | 103 | The ModelType enum is defined in packages/rust-core/src/capability.rs (line 12: use crate::capability::{Capability, ModelType};). The varian |
| L6 | What does the function requested_context in routing/profile.rs return  | ❌ | 6 | localSearch → localSearch → structureSearch → structureSearch → structureSearch → localSea | verify | 103 | The function requested_context in routing/profile.rs does not exist in this codebase. The path ./routing/profile.rs was not found, and no ma |
| L7 | Which MCP tools does packages/mcp/src/index.ts register? List their re | ✅ | 1 | localSearch | verify | 15 | The MCP tools registered in packages/mcp/src/index.ts are: 1. registerDocsResources (line 60) 2. registerDoctorTool (line 62) 3. registerSes |
| L8 | Is there a function named frobnicate_widgets anywhere in this reposito | ✅ | 1 | localSearch | verify | 7 | no |

**4/8** right; 3.8 calls per question; narrowed before reading on 8/8.

## GitHub tools: `bonsai-2:9b`, adapter `github`, 2026-11-01T10:28:00.232Z

| # | Question | Right | Calls | Tool trail | Verdict | s | Answer |
|---|---|---|---:|---|---|---:|---|
| G1 | In the GitHub repository ollama/ollama, which file defines the type Sy | ✅ | 2 | ghSearchCode → ghSearchCode | verify | 48 | The type SystemOneRequest is defined in ollama/ollama/api/types.go at line 606. |
| G2 | In ollama/ollama, what JSON fields does SystemOneRequest have? List th | ❌ | 2 | ghSearchCode → ghSearchCode | verify | 38 | SystemOneRequest has these JSON fields: - model (string) - runner (string, optional) - state (json.RawMessage) |
| G3 | Which pull request number added the System One scoring API to ollama/o | ✅ | 2 | ghSearchHistory → ghSearchHistory | verify | 45 | PR #18606 added the System One scoring API |
| G4 | Which GitHub repository hosts the npm package zod? Answer owner/repo. | ✅ | 2 | artifactSearch → artifactSearch | verify | 14 | colinhacks/zod |
| G5 | According to the docs in ollama/ollama, what is the default response_f | ❌ | 1 | ghSearchCode | verify | 24 | The default response_format for /v1/audio/speech is 'json'. |
| G6 | What is the main programming language of the GitHub repository ollama/ | ✅ | 1 | ghSearchRepo | verify | 22 | Go |

**4/6** right; 1.7 calls per question; narrowed before reading on 6/6.

