# Model forms and measured results

Moved from the root README. Dated measurements describe one machine and workload.

## Model forms

Managed forms get routing, admission and a receipt; passthrough forms reach Ollama through `serve`
with the raw stream cap and memory checks but no routing or receipt.

| Form | Ollama endpoint | Through FreeLlama | Measured here |
|---|---|---|---|
| Text, coding, reasoning | `/api/chat` | Managed: `completion`, `coding`, `code_repair`, `long_context`, `think` | `gpt-oss:20b` 4/4 |
| Tool calling | `/api/chat` with `tools` | Managed: `tools` | `gpt-oss:20b` 1/1 |
| Structured JSON | `/api/chat` with `format` | Managed: `format` | `gpt-oss:20b` 1/1 |
| Vision and OCR | `/api/chat` with `images` | Managed: `vision`, `stopOnRepetition` for looping readers | `glm-ocr` 4/4 phrases |
| Embeddings | `/api/embed` | Managed: `embedding`, batched input, vectors withheld unless asked | `nomic-embed-text:v1.5` retrieval 4/4 |
| Decisions (classify, route, score) | `/v1/systemone` | Managed: `decision`; the catalog lists the model as `model_type: "decision"` | `strands-decider:2b-nvfp4` 9/9, 3/3, 2/3 |
| Speech (text to speech) | `/v1/audio/speech` | MCP `audio`, through the passthrough's memory checks; Phonon needs Ollama 0.40.9+ on Apple silicon | `phonon:en` works |
| Transcription and translation | `/v1/audio/transcriptions`, `/v1/audio/translations` | MCP `audio`, through the passthrough's memory checks; LFM3 audio needs Ollama 0.40.12+ | `lfm3-audio:2b-q4_K_M` transcription 7.5% word error over 5 sentences; translation not measured |
| Image generation | removed from Ollama in 0.32.6 | not available | — |

Find any of these with `models {view:"library", capabilities:["decision"]}` (or `audio`,
`embedding`, `vision`...), or a Hugging Face GGUF build with a `query`.

### Local models, measured

One Apple M2 Pro (32 GB), Ollama 0.40.8, FreeLlama 0.6.1, 2026-10-30, while other builds kept the
one-minute load near 14 on 12 cores. Each check has a known answer; the batteries are small, so read
the ratings as a first screen for these exact tags, not a benchmark.

| Model | Download | Forms | Result | Speed (warm) | Rating | Use it for |
|---|---:|---|---|---|---:|---|
| `gpt-oss:20b` | 13.8 GB | text, tools, thinking, JSON | 7/7: exact output, arithmetic, bat-and-ball, code reading, JSON extraction, tool call, 9/9 ticket routing | 1.4 s per routed ticket; 12 s first call with load | 9/10 | General agent work and tool calls when quality matters |
| `nomic-embed-text:v1.5` | 274 MB | embedding | Retrieval 4/4 top-1; ticket routing 9/9 from two examples per team | 51 ms per item, batched | 9/10 | Search, deduplication, cheap classification |
| `strands-decider:2b-nvfp4` | 1.3 GB | decision | Ticket routing 9/9, return-policy yes/no 3/3, damage rubric 2/3 | 0.17 s per question | 8/10 | Fast routing and policy checks with probabilities; rubric scores run low at the top level |
| `glm-ocr:latest` | 2.2 GB | vision (OCR) | 4/4 phrases read from a dense report strip; 0/7 on text tasks | 11 s per strip at a 400-token cap | 8/10 OCR, 1/10 text | Reading screenshots and scans only |

### What they did for a coding agent

Measured 2026-10-31 on the same M2 Pro (Ollama 0.40.13, FreeLlama 0.8.0) while builds and editors
kept it swapping; rerun the [live model harnesses](TESTING.md#run-live-model-harnesses) for current tables.

| Agent chore | Model | Result | Cost |
|---|---|---|---|
| Find the code a real issue is about (octocode repo, 1,025 Rust files ≈ 1.9 M tokens) | `nomic-embed-text` over one "card" per file (path, module doc, item names) | 7/7 issues had the right area in the top 5 | 27 s to index once, ~50 ms per query; read 5 cards (~4 k tokens) instead of searching |
| Triage real GitHub issues (bug, feature, security, question) | `strands-decider` | 35/36 | 0.46 s per issue |
| Route tickets, yes/no policy checks, sentiment | `strands-decider` | 17/17 (severity scores: 1/3, don't use for rubrics) | ~0.1 s |
| Classify with three examples per class | `nomic-embed-text` centroids | 18/20; both misses were near-ties (margin < 0.02) | 18 ms per item |
| Find related issues | `nomic-embed-text` | 8/12 found the exact partner; the rest found a related one | ms |
| Read screenshots, receipts, logs, code, a live status page | `glm-ocr` | 27/27 pages read exactly when the image was cropped to the text | ~2 s per page warm |

What makes the difference: a stable prompt prefix (warm it once) cut prompt evaluation from 544 ms
to 17–38 ms, 1,251 of 1,266 tokens cached; embeddings on the CPU backend ran as fast as on the GPU
(200 items: 2.2 s vs 2.5 s) and keep the GPU free for OCR and chat; send OCR a tight crop, not a full
page with a line of text on it.
