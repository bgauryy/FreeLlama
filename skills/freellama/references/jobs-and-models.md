# Jobs, workflows and models

Load when choosing what to send, which model runs it, or whether to install one. Picks change with the machine: read `models {view:"catalog", job}` (CLI `freellama catalog`) before relying on any name below. Recipes: `freellama://docs/cookbooks/<job>`.

## Pick the request shape

| Work | Send | Notes |
|---|---|---|
| Fix, change, implement, test, review, explain code | `workflow`: `fix-bug`, `change-code`, `implement`, `write-tests`, `review-code`, `explain-code` | Answers are SEARCH/REPLACE blocks: apply one only where its SEARCH text matches exactly once, then run the tests. Verify every review finding. |
| Summarize a log, text or diff | `workflow`: `summarize-log`, `summarize-text`, `summarize-diff` | Name the file with `files`; the prompt says what you are investigating. |
| Fields to JSON | `workflow:"extract-json"` with a schema (`format` / `--schema`) | A schema is the cheapest reliability upgrade; it held on the smallest model in field tests. |
| Text, tables, fields from images | `workflow`: `read-text`, `read-table`, `read-fields` (job `ocr`) | `read-table` returns a Markdown table (the raw OCR markup stays in `structuredContent`). Check every number you use against the image. |
| A question about an image or screenshot | `workflow`: `answer-image`, `describe-screen` (job `image-recognition`) | Ask a specific question, never "describe this". |
| Yes/no, choose a category, route, policy check | `task:"decision"` with `state` and `questions` (`noul`, `choice`), job `classify`; CLI `freellama run --ask "Q?" [--choice k=desc …] < state` | Probabilities come back. Act above ~0.9 or below ~0.1. An answer marked `[unsure: treat as unknown]` is a coin toss: send it to a second model or keep it. Do not use `score` answers for severity. Give choice questions an `other` option. |
| Find code by meaning | job `find-code`: `nearest:{query, top}` over `files` | Read only the files it names. Prefer `grep`/LSP when the identifier is known. |
| Near-duplicates, clustering | job `duplicates` (omit `query` for the most similar pairs) | A high cosine score is a candidate, never an equivalence or deletion verdict. |
| Images by meaning | job `image-search` | Images are embedded bare. |
| Bulk classification | embeddings with three labeled examples per class; when the top two classes are within 0.02, ask the decider or keep the item | |
| Speech | `audio {action:"transcribe"\|"translate"\|"speak", model, file\|text}` | `translate` outputs English from the spoken `language`. |
| A large divisible check (code tree, logs, docs site) | `sweep` | See [advanced flows](advanced-flows.md#sweep). |
| Read-only lookup across files | `delegate_research` | See [advanced flows](advanced-flows.md#grounded-research). |

`run_task {workflow}` or `{job}` opts into FreeLlama's model pick and local prompts; any field you send wins. Plain chat can omit `task` (completion). Other routing profiles: `coding`, `code_repair`, `tools`, `browser`, `vision`, `embedding`, `long_context`.

## Field-tested model behavior

Measured on one 32 GB Apple M2 Pro, November 2026, ~280 runs. Treat as evidence for this class of model, not a default for another machine.

- **Question scores do not predict workflow answers.** `lfm3:1.7b` scored best on 25 small coding questions but answered real workflows with tool calls nobody asked for; it and `glm-ocr` were removed as picks. The coding and summarize default is now `bonsai-2:9b` (about 10.3 GB available): when memory is truly short, serve's gate refuses with exit 75 rather than running a weak model.
- **A downgrade is announced.** When a capable workflow (fix, implement, review, summarize) runs below a stronger installed pick that had no room, a `check:` line says "ran on X because the stronger installed pick Y did not fit now". If the answer matters, rerun when memory frees or pass `model`.
- **A tool call is not an answer.** If a model replies with a tool call although no tools were sent, the CLI exits 1 with nothing on stdout, and MCP adds the `check:` "the model answered with a tool call although no tools were sent". Discard it and rerun on a stronger pick.
- **Keep thinking off for bounded jobs.** Thinking on a small model took 80–140 s per summary and sometimes spent the whole budget (empty answer, exit 1). Turn it on only for one hard question on a capable model, with a large `num_predict`.
- **Small models follow structure, not prose.** `--schema`/`format` was right every time, even on a 1.7B model; a persona system prompt was ignored by the 1.7B model and followed by the 9B one. Tell a model what to do, never what it lacks: a "you have no tools" line made a 1.7B model call tools 6 times in 6.
- **OCR models are not chat models.** An OCR-only model asked a chat question runs on to the output limit; use it through job `ocr` with its native prompt (`Text Recognition:`), and `stopOnRepetition:true` for one known to loop. Crop to the text first.
- **Decision models are the fastest win.** A 2B decision model answered in 0.4–2 s with calibrated probabilities; a 9B decision model was sharper but cost ~12 GB for no measured gain.
- **Embedding models**: gemma-family for code (right file first 9/10) and images; nomic for short text and speed. `--dimensions N` shortens vectors on both.
- **Large models need a quiet machine.** 27B models needed 14.5–26 GB with reserve and were refused while editors, agents and a build were open. `bonsai-2:27b` answered correctly at about 14 tokens/s: one hard question, not a batch.
- Put embedding models on the CPU backend when the operator configured one (`serve --spawn-cpu-backend --cpu-model <embed tag>`): as fast as the GPU here, and they stop queuing behind chat.

## Choose or install a model

1. Ask for the missing constraints: modality, quality, latency, context, privacy, download, disk and memory.
2. Prefer a qualified installed model (catalog `installed`, `fits_now`). Never infer inventory, capability or fit from a family name or from this file.
3. Search `models {view:"library", query}` (ollama.com and Hugging Face GGUF; `capabilities` filters such as `embedding`, `audio`, `decision`), then list pullable tags and `fitsInMemory` with `models {view:"library", model:"FAMILY"}` or `model:"hf.co/<owner>/<repo>"`. With serve down, do not guess a tag. `models {view:"detail"}` inspects an installed model.
4. Present at most two evidence-backed candidates. Ask approval for **one exact tag and its reported size**, then `ollama_pull {model, approvedSizeBytes}`. Search results never grant installation.
5. `objective:"fastest"` without evidence is only a capability filter and can pick a tiny model for code repair. For quality-sensitive work preview with `minConfidence:"medium"`; it needs a task policy from correctness evaluations (`freellama policy from-eval`) and a local `bench-all` report. Never manufacture a policy from throughput.

See [model selection](https://github.com/bgauryy/FreeLlama/blob/main/docs/MODEL_SELECTION.md).
