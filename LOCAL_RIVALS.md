# LOCAL_RIVALS: the same questions on rival models

Three rounds per model unless noted, one model at a time, through FreeLlama at http://127.0.0.1:11465. Apple M2 Pro, 32 GB, Ollama 0.40.13, with other work running; timings are noisy, so compare accuracy first. Questions were written before these runs, by the same author as the harness. Small sets on one machine: a difference of one or two questions is not a real difference.

## Classify and triage

56 questions: 20 decision questions plus 36 real labeled issues.

| Model | Right, each round | Changed between rounds | Median ms | Usually wrong |
|---|---|---|---:|---|
| `strands-decider:2b-nvfp4` | 53, 53, 53 of 56 | 0 | 7 | D18, D19, #411 |
| `nimble-v3:9b-q4_K_M` | 51, 51, 51 of 56 | 0 | 1496 | #194, #158, #429, #346, #411 |

## OCR

30 rendered text images; right means every expected phrase is present. Same prompt for both, as each model's card documents.

| Model | Right, each round | Changed between rounds | Median ms | Usually wrong |
|---|---|---|---:|---|
| `glm-ocr:latest` | 30, 30, 30 of 30 | 0 | 1885 | none |
| `hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M` | 30, 30, 30 of 30 | 0 | 257 | none |

## Speech to text

12 sentences spoken by phonon:en; "forty two" and "42" count as equal. Generated speech, not human recordings.

| Model | Word error | Perfect sentences, each round | Median ms | French | Spanish | Sample misses |
|---|---:|---|---:|---|---|---|
| `lfm3-audio:2b-q4_K_M` | 1.7% | 11, 11, 11 of 12 | 276 | ✅ Veuillez redémarrer le serveur après avoir mis à jour les variables d'environnement. | ✅ Abrir la solicitud de extracción y asignarla al equipo de revisión. | We remove the old cache because it returns stale results. |
| `lfm3-omni:2b-q4_K_M` | 1.7% | 11, 11, 11 of 12 | 280 | ✅ Veuillez redémarrer le serveur après avoir mis à jour les variables d'environnement. | ✅ Abrir la solicitud de reclamo y asignarla al equipo de revisión. | We remove the old cache because it returns stale results. |

## Text to speech

12 sentences spoken, then transcribed by lfm3-audio:2b-q4_K_M. This scores clarity to a machine, not how natural it sounds; the judge may favor its own voice.

| Model | Word error, each round | Clean sentences, each round | Median ms | Average audio |
|---|---|---|---:|---:|
| `phonon:en` | 0.8%, 1.7%, 1.7% | 11, 11, 11 of 12 | 102 | 3.8 s |
| `lfm3-audio:2b-q4_K_M` | 0%, 0%, 0% | 12, 12, 12 of 12 | 950 | 3.9 s |
| `lfm3-omni:2b-q4_K_M` | 0%, 0%, 0% | 12, 12, 12 of 12 | 950 | 4 s |

## Image recognition

20 drawn images with known answers: shapes, counts, charts, screens. Drawings, not photos, so this says little about real-world pictures.

| Model | Right, each round | Changed between rounds | Median ms | Usually wrong |
|---|---|---|---:|---|
| `lfm3-vl:2b-q4_K_M` | 20, 20, 20 of 20 | 0 | 299 | none |
| `lfm3-omni:2b-q4_K_M` | 18, 18, 18 of 20 | 0 | 269 | V05, V10 |
| `bonsai-2:9b` | 17, 17, 17 of 20 | 0 | 1404 | V05, V17, V19 |
| `bonsai-2:27b` | waiting | | | |

## Code search

10 questions over FreeLlama's platform source; one round, because embeddings are deterministic.

| Model | Right file first | Right file in top 3 | Chunks | Index time | Missed |
|---|---:|---:|---:|---:|---|
| `nomic-embed-text:v1.5` | 6/10 | 8/10 | 881 | 41.6 s | resources/holders.rs (rank 5), warming.rs (rank 7) |
| `embeddinggemma-2:740m-q8_0` | 9/10 | 9/10 | 899 | 67.4 s | resources/mod.rs (rank 14) |

## Coding

25 small coding questions with known answers.

| Model | Right, each round | Changed between rounds | Median ms | Usually wrong |
|---|---|---|---:|---|
| `lfm3:1.7b-q4_K_M` | 21, 20, 20 of 25 | 1 | 579 | C11, C14, C18, C19, C25 |
| `bonsai-2:9b` | 19, 19, 17 of 25 | 3 | 475 | C01, C03, C08, C11, C14, C19, C20 |
| `bonsai-2:27b` | waiting | | | |
| `hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M` | 23, 22, 23 of 25 | 3 | 267 | C15, C23 |

## Research agent with Octocode

8 local and 6 GitHub questions; right means every expected fact and at least one tool call.

| Model | Local, each round (of 8) | GitHub, each round (of 6) | Changed between rounds | Calls per question | Median s |
|---|---|---|---|---:|---:|
| `lfm3-audio:2b-q4_K_M` | 3, 3, 3 | 3, 3, 3 | none | 3.2 | 20 |
| `lfm3:1.7b-q4_K_M` | 6, 5, 5 | 1, 1, 1 | L7 | 2 | 20 |
| `bonsai-2:9b` | 4, 5, 5 | 3, 5, 5 | L8, G1, G3 | 2.2 | 82 |
| `bonsai-2:27b` | waiting | | | | |
| `hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M` | 6, 6, 6 | 5, 5, 5 | L4, L7 | 2.2 | 96 |

## Cookbook recipes, run as written

Each recipe's calls ran through the real MCP server on real files, with placeholder paths replaced. Passing means the call succeeded and returned something, not that the answer is right; the sections above score quality.

| Recipe | Calls passed | First answer | Failures |
|---|---:|---|---|
| batch-text | 1/1 | ## v1 [bonsai-2:9b] Version 1 introduces login functionality and resolves a crash occurring during saves. ## v | none |
| classify | 2/2 | [strands-decider:2b-nvfp4] kind: bug (p 0.92, confidence 0.69) urgent: false (p(true) 0.39) | none |
| coding | 1/1 | [bonsai-2:9b · loaded 1.8 s] ```typescript import { validateEmbeddingVectors, norm, cosine, nearest, fileOf, s | none |
| duplicates | 1/1 | most similar pairs among 3 items: 0.977 /var/folders/nf/tf_qg9k56l97qnkcc2b_w5z40000gn/T/freellama-rivals/fixt | none |
| find-code | 1/1 | closest of 103 items to the query: 0.676 packages/rust-core/src/platform/resources/mod.rs:L198-236 0.671 packa | none |
| image-recognition | 1/1 | [lfm3-vl:2b-q4_K_M · loaded 1.6 s] C | none |
| image-search | 1/1 | closest of 3 items to the query: 0.745 /var/folders/nf/tf_qg9k56l97qnkcc2b_w5z40000gn/T/freellama-rivals/fixtu | none |
| memory-and-caching | 4/4 | classify: strands-decider:2b-nvfp4 (default, measured, installed, fits now); nimble-v3:9b-q4_K_M (stronger, pu | none |
| ocr | 1/1 | [hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M · loaded 2.3 s] Invoice 4821 Total due: $312.50 | none |
| research-agent | 2/2 | The function memory_holders is called in: - packages/rust-core/src/platform/execution/pipeline.rs (lines 247,  | none |
| speech-to-text | 2/2 | [lfm3-audio:2b-q4_K_M] The build failed because the test runner could not find the configuration file. | none |
| text-to-speech | 1/1 | [phonon:en] wrote 2.9 s of speech to /var/folders/nf/tf_qg9k56l97qnkcc2b_w5z40000gn/T/freellama-rivals/reply.w | none |

## Model profiles: context, memory, caching, speed

One run per model, one at a time, with a repeated 90-rule prefix of about 2,000 tokens. Timings are noisy on a busy machine.

| Model | Type | File GB | Max context | Load s | Resident GB at 8K | At 16K | At 64K | Keep-alive s | Prompt tokens | First prompt ms | Cached on repeat | Repeat ms | Output tok/s | Oversized input |
|---|---|---:|---:|---:|---:|---|---|---:|---:|---:|---:|---:|---:|---|
| `phonon:en` | speech | 0.1 | 8192 | — | 0.12 | — | — | 300 | — | — | — | — | — | — |
| `nomic-embed-text:v1.5` | embedding_only | 0.3 | 2048 | 0.5 | 0.34 | — | — | 28 | — | — | — | — | 62 | 422 embedding_item_exceeds_context |
| `lfm3:1.7b-q4_K_M` | generative | 1.1 | 262144 | 1 | 1.25 | 1.27 GB (reloaded) | ? GB (reloaded) | 71 | 2272 | 2112 | 1248 | 1220 | 45 | 400 upstream_error |
| `embeddinggemma-2:740m-q8_0` | embedding_only | 1.3 | 262144 | 2.4 | 0.37 | — | — | 24 | — | — | — | — | 27 | 422 embedding_item_exceeds_context |
| `strands-decider:2b-nvfp4` | decision | 1.3 | 4096 | 0 | 1.37 | — | — | 23 | 2516 | 4669 | 0 | 3942 | — | 413 upstream_error |
| `hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M` | multimodal | 1.4 | 262144 | 1.8 | 0.77 | 0.9 GB (reloaded) | 1.89 GB (reloaded) | 38 | 2464 | 1277 | 2446 | 32 | 83 | 400 upstream_error |
| `lfm3-vl:2b-q4_K_M` | multimodal | 1.5 | 128000 | 1.6 | 1.64 | 1.66 GB (reloaded) | 2.04 GB (reloaded) | 31 | 2272 | 2716 | 1248 | 1219 | 44 | 400 upstream_error |
| `lfm3-audio:2b-q4_K_M` | multimodal | 1.7 | 128000 | 1.6 | 1.91 | 1.92 GB (reloaded) | 2.73 GB (reloaded) | 45 | 2272 | 2926 | 1248 | 1314 | 60 | 400 upstream_error |
| `lfm3-omni:2b-q4_K_M` | multimodal | 2 | 128000 | 2.3 | 1.91 | 1.92 GB (reloaded) | 2.73 GB (reloaded) | 31 | 2272 | 2458 | 1248 | 1305 | 58 | 400 upstream_error |
| `glm-ocr:latest` | multimodal | 2.2 | 131072 | 1.8 | 2.32 | 2.91 GB (reloaded) | held (503) | 31 | 2370 | 932 | 2358 | 84 | 58 | 400 upstream_error |
| `bonsai-2:9b` | multimodal | 3.2 | 262144 | 3.1 | — | 3.09 GB (reloaded) | 5.13 GB (reloaded) | — | 2464 | 12935 | 0 | 12138 | 22 | 400 upstream_error |
| `nimble-v3:9b-q4_K_M` | decision | 5.6 | 32768 | 0 | 6.37 | — | — | 900 | 2587 | 11045 | — | 9805 | — | 413 upstream_error |
| `hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M` | waiting | | | | | | | | | | | | | |
| `bonsai-2:27b` | waiting | | | | | | | | | | | | | |

