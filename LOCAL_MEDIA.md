# LOCAL_MEDIA: audio and multimodal embeddings through MCP

serve http://127.0.0.1:11455; 2026-11-01T09:15:07.347Z; Apple M2 Pro 32 GB; Ollama 0.40.13

## 1. Text to speech, then speech to text (round trip)

| # | Speak phonon:en | Audio | Transcribe lfm3-audio:2b-q4_K_M | Word error |
|---|---:|---:|---|---:|
| 1 | 386 ms | 4.6 s | The build failed because the test runner could not find the configuration file. (3941 ms) | 0% |
| 2 | 169 ms | 4.1 s | Please restart the server after you update the environment variables. (241 ms) | 0% |
| 3 | 71 ms | 3 s | 42 tests passed and 3 were skipped. (201 ms) | 38% |
| 4 | 85 ms | 3 s | The memory limit was reached while loading the model. (216 ms) | 0% |
| 5 | 100 ms | 3.7 s | Open the pull request and assign it to the review team. (235 ms) | 0% |

Mean word error **7.5%** over 5 sentences; 18.4 s of speech transcribed in 4.8 s.

Translate sentence 2 to French: Structured result available (text, model, file, elapsed_ms). (313 ms)

## 2. Find a screenshot by describing it (multimodal embeddings)

| Query | Top match | Right |
|---|---|---|
| a sign-in form with email and password fields | login.png | ✅ |
| a bar chart of revenue over months | chart.png | ✅ |
| a compiler error message in a terminal | error.png | ✅ |
| a table listing people, their roles and cities | table.png | ✅ |
| a map with a route between two points | map.png | ✅ |

**5/5** screenshots found by description (1041 ms per query, 5 images embedded each time).

## 3. Code search: embeddinggemma-2 against nomic

| Query | nomic top 3 | embeddinggemma-2 top 3 |
|---|---|---|
| how a decision task request is built for /v1/systemone | ✅ execution/request.rs:L323-366<br>tasks_api.rs:L33-61<br>routing/mod.rs:L356-402 (27.5 s) | ✅ execution/request.rs:L323-366<br>routing/mod.rs:L1-54<br>execution/pipeline.rs:L130-161 (57.7 s) |
| which programs hold the most memory when a model load has to wait | ❌ residency.rs:L1-31<br>preload.rs:L287-343<br>resources/mod.rs:L131-166 (29.4 s) | ✅ preload.rs:L224-260<br>resources/holders.rs:L1-39<br>residency.rs:L1-31 (72.7 s) |
| unload idle models when the host is under memory pressure | ✅ sweep.rs:L74-114<br>residency.rs:L583-627<br>warming.rs:L103-149 (30.4 s) | ❌ {"code":"resource_admission_unavailable","error":"host resource admission deadline exceeded; retry when telemetry and ca (69.5 s) |

Right file in the top 3: nomic **2/3**, embeddinggemma-2 **2/3**.

