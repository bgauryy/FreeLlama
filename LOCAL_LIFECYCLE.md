# LOCAL_LIFECYCLE: how FreeLlama runs models on this machine

Measured 2026-10-31 17:08 UTC on an Apple M2 Pro (32 GB unified memory), Ollama 0.40.13,
FreeLlama 0.8.0 at http://127.0.0.1:11455, with other work running on the machine. Small models only
(`nomic-embed-text:v1.5`, `strands-decider:2b-nvfp4`, `glm-ocr:latest`); every step waited until FreeLlama said its model fit.
Single run, one machine: read the numbers as this machine's behaviour, not a benchmark.

## Cold load, warm calls and FreeLlama's own cost (nomic-embed-text)



## Adaptive keep-alive (how long an idle model stays loaded)

- **after 11 uses**: null s left
- **after 21 uses**: 76 s left

> FreeLlama sends keep_alive = clamp(60 s + 30 s per recent use + 2 x load seconds, 15..900 s), shortened 4x while the host is under pressure.

## Unload

- **idle resident model**: 9 ms · unloaded
- **model not loaded**: 5 ms · not_resident · nothing was loaded first
- **model busy with a 1,500-item batch**: 21113 ms · unloaded · batch took 21257 ms

> An unload waits for running tasks on that model (shared transition lock), then unloads.

## Switching models (embedding <-> decision)

- **call 1: nomic-embed-text:v1.5**: 200 · 319 ms · 262 ms load · resident: nomic-embed-text:v1.5
- **call 2: strands-decider:2b-nvfp4**: 200 · 2261 ms · resident: strands-decider:2b-nvfp4, nomic-embed-text:v1.5
- **call 3: nomic-embed-text:v1.5**: 200 · 197 ms · 1 ms load · resident: strands-decider:2b-nvfp4, nomic-embed-text:v1.5
- **call 4: strands-decider:2b-nvfp4**: 200 · 14 ms · resident: strands-decider:2b-nvfp4, nomic-embed-text:v1.5

> Small models stay co-resident; only the first call of each pays a load.

## Queue and priorities (8 managed embedding tasks at once)

- **background**: 200 · 43620 ms wall · 36003 ms queued
- **background**: 200 · 5724 ms wall · 1 ms queued
- **normal**: 200 · 23325 ms wall · 17052 ms queued
- **normal**: 200 · 29324 ms wall · 23352 ms queued
- **normal**: 200 · 35989 ms wall · 29353 ms queued
- **interactive**: 200 · 17108 ms wall · 11352 ms queued
- **interactive**: 200 · 11338 ms wall · 5734 ms queued
- **background**: 400 · 51638 ms wall · ? ms queued
- **4 raw /api/embed at once**: 200 200 200 200 · 233 ms slowest

> Completion order: 1:b 6:i 5:i 2:n 3:n 4:n 0:b 7:b (i = interactive, n = normal, b = background).

## Prompt prefix caching (glm-ocr as a text model, 4k context)

- **warm with the system prompt**: 200 · 43648 ms
- **same prefix, question 1**: 200 · 1266 prompt tokens · 1251 cached · 38 ms prompt eval
- **same prefix, question 2**: 200 · 1266 prompt tokens · 1257 cached · 17 ms prompt eval
- **different prefix**: 200 · 1266 prompt tokens · 7 cached · 544 ms prompt eval
- **learned token ratio**: 3669 milli-bytes/token · 4 samples

## Idle models under memory pressure (sweep)

- **host now**: pressure: ["high_cpu_load","low_available_memory"] · resident: glm-ocr:latest
- **after 75 s idle**: resident: none · swept: nomic-embed-text:v1.5, strands-decider:2b-nvfp4, glm-ocr:latest, glm-ocr:latest

> Under FreeLlama's own low-memory signal only idle-for-60 s runners are released; swapping or OS pressure releases any idle runner.

## GPU vs CPU (serve --spawn-cpu-backend)

- **GPU (Metal), 200 items**: 2485 ms · gpu
- **CPU backend, 200 items**: 2184 ms · cpu · cold load 261 ms
- **after serve exits**: CPU Ollama answering: false

## Edge cases

- **model not installed**: 422 · 27 ms
- **empty prompt**: 422 · 1 ms
- **embedding item far past the context**: 422 · 46 ms · embedding_item_exceeds_context
- **timeout_seconds 0**: 422 · 2 ms
- **unknown field**: 422 · 2 ms · invalid_task_request
- **unload a model that is not installed**: 200 · 2 ms · not_resident
- **warm an embedding model without task**: 200 · 32 ms · ready
- **preview a model that does not fit**: 200 · 65 ms · held_insufficient_capacity; largest: node x24, rustc x2, Code Helper (Plugin) x25, octocode x19, Code Helper (Renderer) x4
- **readiness**: 200 · 2 ms · ready

