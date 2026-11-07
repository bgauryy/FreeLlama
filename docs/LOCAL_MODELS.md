# Local model catalog

Which local model to use for each job on one machine, what was measured, and what is only
published. The method for choosing a model on other hardware is in
[Choose local models](MODEL_SELECTION.md). For live answers about your machine, use `models {view:"catalog"}`
or `freellama catalog`; for the exact calls, see the [cookbooks](cookbooks/README.md).

**Snapshot:** 2026-11-01. Apple M2 Pro, 32 GB unified memory, Ollama 0.40.13, FreeLlama 0.8.0. Other
work was running throughout, leaving about 4–14 GB available. Each model answered the same questions
in three rounds, one model at a time ([LOCAL_RIVALS.md](../LOCAL_RIVALS.md)). "Best" means best among
the models tried here, on small question sets, drawn images and generated speech; a difference of one
or two questions is noise.

Evidence labels used on this page:

- **Measured:** run here through FreeLlama, with known answers.
- **Published:** the publisher's or Ollama's figures, not reproduced here.
- **Untested:** no quality result yet.

## How to section local models

Group a model by the job it does, using the capability Ollama reports. FreeLlama shows the type as
`model_type`.

| Section | Ollama capability | `model_type` | Used for | How to call it |
|---|---|---|---|---|
| Text and code | `completion`, plus `tools`, `thinking` | `generative` | Chat, coding, tool-calling agents | `run_task {task:"coding"}` |
| Vision | `completion` + `vision` | `multimodal` | OCR, screenshots, images | `run_task {task:"vision", files:[path], prompt}` |
| Audio | `audio` | `multimodal` | Speech to text, translation | `audio {action:"transcribe", file}` |
| Speech | `speech` | `speech` | Text to speech | `audio {action:"speak", text}` |
| Embedding | `embedding` | `embedding_only` | Code search, duplicates, image search | `run_task {task:"embedding", files, nearest}` |
| Decision | `decision` | `decision` | Classify, triage, route, yes/no | `run_task {task:"decision", state, questions}` |
| Not runnable in Ollama | none | none | Rerankers, tiny classifiers, most dedicated speech models | Needs another runtime; FreeLlama cannot serve them |

## Tested picks for each job

### Use now: installed, and fit alongside normal work

| Job | Model | File | Memory needed | Evidence |
|---|---|---:|---:|---|
| Classify, triage, yes/no | `strands-decider:2b-nvfp4` | 1.3 GB | 3.8 GB | Measured: 53/56 in all 3 rounds, 7 ms median. `nimble-v3` scored 51/56 at 1.5 s and needs 11.5 GB |
| Find code for an issue | `embeddinggemma-2:740m-q8_0` | 1.3 GB | 1.4 GB | Measured: right file first 9/10, top 3 9/10; `nomic-embed-text` 6/10 and 8/10, but indexes in 42 s against 67 s |
| Find duplicates | `nomic-embed-text:v1.5` | 0.3 GB | 1.4 GB | Measured: 8/12 real issue pairs |
| Find an image by describing it | `embeddinggemma-2:740m-q8_0` | 1.3 GB | 1.4 GB | Measured: 5/5 screenshots, about 1 s per query |
| OCR, reading text in images | `hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M` | 1.4 GB | 3.8 GB | Measured: 30/30 in all 3 rounds, 257 ms median; `glm-ocr` also 30/30 at 1,885 ms |
| Image understanding | `lfm3-vl:2b-q4_K_M` | 1.5 GB | 4.1 GB | Measured: 20/20 drawn images in all 3 rounds, 299 ms; `lfm3-omni` 18/20, `bonsai-2:9b` 17/20 |
| Speech to text | `lfm3-audio:2b-q4_K_M` | 1.7 GB | 5.5 GB | Measured: 1.7% word error on 12 generated sentences, 276 ms median, French and Spanish translation worked; `lfm3-omni` the same |
| Text to speech | `phonon:en` | 0.1 GB | not previewed | Measured: 0.8–1.7% word error when transcribed back, 102 ms median; both LFM3 models 0% at 950 ms, but the judge is an LFM3 model |
| Small coding tasks | `lfm3:1.7b-q4_K_M` | 1.1 GB | 3.3 GB | Measured: 21, 20, 20 of 25, 579 ms median; `bonsai-2:9b` 19, 19, 17 |

### Use when memory is free: installed

| Job | Model | File | Memory needed | Evidence |
|---|---|---:|---:|---|
| Coding | `hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M` | 6.2 GB | 13.5 GB | Measured: 23, 22, 23 of 25, 267 ms median: the best coder tried |
| Research agent with Octocode | `hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M` | 6.2 GB | 13.5 GB | Measured: 6/8 local and 5/6 GitHub in all 3 rounds: the best agent tried |
| Research agent, smaller | `bonsai-2:9b` | 3.2 GB | 10.3 GB | Measured: 4, 5, 5 of 8 local and 3, 5, 5 of 6 GitHub; slow, 82 s median per question |

"Memory needed" is FreeLlama's route preview on 2026-11-01, including its reserve.

### Weak or impractical here

| Model | File | Why |
|---|---|---|
| `lfm3-audio:2b-q4_K_M` as a research agent | 1.7 GB | Measured: 3/8 local, 3/6 GitHub in all 3 rounds. Guesses names and repeats calls |
| `bonsai-2:27b` | 8.3 GB | Installed. FreeLlama asks for 16.3 GB available; this machine never had more than 14.4 GB, so it is not measured here. Published: SWE-bench Verified 60.8%, LiveCodeBench 90.1% |
| `qwen3.6:27b-q4_K_M` | 17.4 GB | Needs 28.7 GB available, so only on a nearly empty 32 GB machine. Removed from this machine untested |

### Worth adding: not installed, untested here

Pull a tag only with approval for that exact tag and size.

| Job | Tag | File | Why |
|---|---|---:|---|
| Small classifier | `tev1:4b-nvfp4` | 3.1 GB | Published: Tev1 4B 73.3% against Jev's 76.0% |
| Text-only embeddings | `embeddinggemma-2:270m-q8_0-text` | 0.3 GB | Published: about 14% better on code than its predecessor |
| Small coding fallback | `qwen2.5-coder:7b` | 4.7 GB | Older; estimated about 11 GB needed |

## Models like Jev

Jev is a hosted, closed decision model from TypeSafe AI: it answers typed questions (yes/no, choice,
score) in one forward pass, with no generated text. Local decision models follow the same
`/v1/systemone` API, so they can be swapped without code changes.

| Model | JevBench (of 231) | Against Jev directly | File | Fit here |
|---|---:|---|---:|---|
| `kev:27b-nvfp4` | 205 | none found | 19 GB | No: 25.56 GiB when loaded |
| `pplx-decider` (27B) | none found | 85.7% vs 84.5%, Perplexity's own test | large | Almost certainly no |
| `nimble-v3:9b-nvfp4` | 201 | none found | 6.9 GB | With Chrome and VS Code closed |
| `nimble-v3:9b-q4_K_M` | 198 | none found | 5.6 GB | With Chrome and VS Code closed |
| `kev:9b-nvfp4` | 183 | none found | 6.7 GB | 11.16 GiB when loaded |
| `strands-decider:2b-nvfp4` | 182 | none found | 1.3 GB | Yes; installed |
| `kev:4b` (BF16) | 175 | none found | 8.4 GB | 12.47 GiB when loaded |
| `tev1:4b-nvfp4` | none found | Tev1 4B 73.3% vs 76.0%, 13 datasets | 3.1 GB | Probably, when memory is free (estimate) |
| `kev:0.8b` | 146 | none found | about 1 GB | Yes, but weak |

The original Nimble 9B (not v3) scored 75.7% against Jev 1.13's 76.0% on the same 3,880 human-labeled
decisions; its publisher ran that test.

**Recommendation:** keep `strands-decider` for everyday decisions, and trial `nimble-v3:9b-q4_K_M`
for harder ones. Compare them on your own labeled data before switching, since `strands-decider`
already scores 35/36 on real issues here.

**Limits of this comparison:**

- Kev's figures use JevBench v1.2.16, while Nimble and Strands use a specific commit. Whether those match was not checked.
- No verified JevBench score for Jev itself was found.
- Most figures come from the models' own publishers.

Other families:

- `d1`, `d1-omni` and `d3` publish other benchmarks, so they are not ranked here. `d1-omni` matters when decisions need images or audio.
- `jevos-v4` claims 28–130 ms on a laptop CPU with its own binary and also ships GGUF files. No JevBench score was found, and loading in Ollama was not checked.
- `openjev` is 27B and licensed CC-BY-NC, which bars commercial use.

## New models on Hugging Face and Ollama

Trending on 2026-10-31 and filtered to what could fit. Quality is the publishers' claim. **Path**:
Ollama is an official tag, HF GGUF pulls as `hf.co/<repo>:<quant>`, and "not via Ollama" needs another
runtime.

| Section | Model | Size | Path | Note |
|---|---|---:|---|---|
| Text, agents | LFM3-1.7B | about 1 GB | Ollama, HF GGUF | Tools and thinking |
| Text | Spark-X2.6-4B | 3.5 GB | HF GGUF | Custom architecture; Ollama may not load it |
| OCR | MiniCPM-OCR-1.0 | 1.4 GB with its vision part | HF GGUF | Official GGUF |
| OCR | Unlimited-OCR-2 | 2.0 GB | HF GGUF | Custom architecture; Ollama may not load it |
| OCR | Xiaomi-OCR-0 | 0.87B parameters | HF GGUF | Smallest OCR found |
| Vision | LFM3-VL-2B | 1.5 GB | Ollama | Image understanding, finding objects |
| Vision | WeVisOne-VL-4B | 2.7 GB | HF GGUF | Tagged for object detection |
| Vision | Mage-VL-Cowork | 2.7 GB | HF GGUF | Built for screen and UI work |
| Audio | LFM3-Omni-2B | 2.0 GB | Ollama, HF GGUF | Speech in and out, plus images |
| Speech to text | parakeet-tdt-0.6b-v3, granite-speech-5.0-2b, nemotron-3.5-asr-streaming-0.6b | small | Not via Ollama (inference) | Dedicated transcription |
| Text to speech | Kokoro-82M, Audio8-TTS 0.6b, maya2-native 2.5B, kitten-tts-2 | small | Not via Ollama (inference) | |
| Embeddings | pplx-embed-v2-context-0.6b | 0.6B parameters | Not via Ollama | |
| Embeddings | Qwen3-VL-Embedding-2B | 2.1B parameters | HF GGUF | Images and text |
| Reranker | bge-reranker-v2-m3 | 0.57B parameters | Not via Ollama | FreeLlama has no rerank task |
| Classifier | vela3-fast, GLiNER2.5-Decide, gliclass-v4-edge | under 0.5B parameters | Not via Ollama | Tiny and fast |
| Safety | Llama-Prompt-Guard-2-86M | 86M parameters | Not via Ollama | Prompt-injection classifier |

Too large for this machine alongside normal work: Qwen3.8-27B, Nanbeige4.3-33B, flint 40B,
Clover-35B-A3B, Tiel-Coder-35B-A3B, qwen3-coder:30b (19 GB), qwen3-coder-next (52 GB),
Nemotron-3.5-Transcribe-30B, Apriel-Voice-27B, drama-3.

## Using Octocode from a local model

[Local tools](octocode/LOCAL.md) and [GitHub tools](octocode/GITHUB.md) are the instructions, each
within 30 lines. They are also the prompts for `delegate_research {adapter:"octocode"}` and
`{adapter:"github"}`, which run the installed `octocode` CLI's read-only tools. All 23 examples ran
through the real CLI.

| Model | Local questions | GitHub questions | Notes |
|---|---:|---:|---|
| `lfm3-audio:2b-q4_K_M` | 3, 3, 3 of 8 | 3, 3, 3 of 6 | Guessed names and repeated calls; 3.2 calls per question |
| `lfm3:1.7b-q4_K_M` | 6, 5, 5 of 8 | 1, 1, 1 of 6 | Good on local lookups, weak on GitHub; 2 calls per question, 20 s median |
| `bonsai-2:9b` | 4, 5, 5 of 8 | 3, 5, 5 of 6 | 2.2 calls per question; 82 s median per question |
| Nemotron-3.5-Bolt-E4B | 6, 6, 6 of 8 | 5, 5, 5 of 6 | 2.2 calls per question; 96 s median per question; needs 13.5 GB |

Three rounds each. An answer counts as right only if it contains every expected fact and rests on at
least one tool call.

## What runs in FreeLlama

- **Managed, with routing, memory checks and receipts:** chat, coding, vision, embeddings, decisions.
- **Through serve's passthrough, with memory checks:** speech and transcription (`audio`).
- **Not supported:** a second, non-Ollama runtime; separate images per question in one decision, because Ollama shares images across all questions.
- **Hugging Face:** GGUF repositories work in discovery and pull through Ollama as `hf.co/<repo>:<quant>`. Multimodal GGUF needs an Ollama-supported architecture.

## Still open

- Measure `bonsai-2:27b` when 16.3 GB is available.
- Measure audio embeddings through `files`; image embeddings are measured, audio ones are not.
- Run the candidates under "Worth adding".
- Test `embeddinggemma-2` on duplicates; only `nomic-embed-text` is measured there.

## Detailed results

- `LOCAL_TEST.md`: 100 graded questions by group.
- `LOCAL_LIFECYCLE.md`: load, unload, CPU and GPU, queues, caching.
- `LOCAL_ASSIST.md`: issue to code, duplicates, triage, OCR on a real repository.
- `LOCAL_OFFLOAD.md`: decisions, ranking and OCR through MCP.
- `LOCAL_MEDIA.md`: speech round trip, image search, code search comparison.
- `LOCAL_OCTOCODE.md`: local models using Octocode.

## Sources

- [Ollama: strands-decider](https://ollama.com/library/strands-decider), [kev](https://ollama.com/library/kev), [nimble-v3](https://ollama.com/library/nimble-v3), [nimble](https://ollama.com/library/nimble), [tev1](https://ollama.com/library/tev1), [pplx-decider](https://ollama.com/library/pplx-decider)
- [Ollama: d1](https://ollama.com/library/d1), [d1-omni](https://ollama.com/library/d1-omni), [d3](https://ollama.com/library/d3)
- [Ollama: lfm3](https://ollama.com/library/lfm3), [lfm3-vl](https://ollama.com/library/lfm3-vl), [lfm3-omni](https://ollama.com/library/lfm3-omni), [lfm3-audio](https://ollama.com/library/lfm3-audio), [phonon](https://ollama.com/library/phonon), [bonsai-2](https://ollama.com/library/bonsai-2), [embeddinggemma-2](https://ollama.com/library/embeddinggemma-2)
- [Ollama: qwen3.6](https://ollama.com/library/qwen3.6), [qwen2.5-coder](https://ollama.com/library/qwen2.5-coder), [qwen3-coder](https://ollama.com/library/qwen3-coder), [qwen3-coder-next](https://ollama.com/library/qwen3-coder-next)
- [JevBench](https://github.com/fstandhartinger/jevbench), [openjev](https://huggingface.co/openjev/openjev), [autotrust/JEV](https://huggingface.co/autotrust/JEV), [feder-cr/jev](https://huggingface.co/feder-cr/jev)
- [Nemotron-3.5-Bolt-E4B](https://huggingface.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB), [MiniCPM-OCR-1.0 GGUF](https://huggingface.co/openbmb/MiniCPM-OCR-1.0-gguf), [Unlimited-OCR-2 GGUF](https://huggingface.co/AtomicChat/Unlimited-OCR-2-GGUF), [Spark-X2.6-4B GGUF](https://huggingface.co/XHToken/Spark-X2.6-4B-GGUF), [WeVisOne-VL-4B GGUF](https://huggingface.co/prithivMLmods/tencent-WeVisOne-VL-4B-GGUF), [Mage-VL-Cowork GGUF](https://huggingface.co/Hansda/Mage-VL-Cowork-GGUF)
- [Hugging Face trending models](https://huggingface.co/api/models?sort=trendingScore), fetched 2026-10-31
