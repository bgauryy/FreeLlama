# Choose local models

To offload a task, select an installed model with the required capabilities and measured quality,
then check its memory fit under the intended concurrency. FreeLlama applies model eligibility
and admission; the calling agent still verifies the output.

The measurements in this guide come from single machines and are not compiled into routing:
[Recommended starting points](#recommended-starting-points-by-form) from an Apple M2 Pro with 32 GB,
the sections after it from an Apple M4 Pro with 48 GB unified memory. Re-run
`npx @octocodeai/freellama doctor`, `models`, `bench-all`, and your quality suite on the target hardware.
`doctor.machine.memory_bytes` is host RAM; a discrete GPU's usable VRAM must be observed separately.

For supported features and sourced publisher use-case guidance, inspect
`models {view:"installed", includeLibrary:true}` through MCP. [Model metadata](#model-metadata) explains digest matching, variant capabilities,
custom-model fallbacks, and how to read family claims without treating them as quality measurements.

```mermaid
flowchart TD
    T{"What must the task do?"}
    T -->|"Generate text, code, or inspect images"| L["Start with one qualified large multimodal model"]
    T -->|"Create retrieval vectors"| E["Use a small embedding model"]
    L --> Q{"Passes held-out quality checks?"}
    E --> Q
    Q -->|"No"| N["Reject or evaluate another model"]
    Q -->|"Yes"| M{"Fits with workload concurrency?"}
    M -->|"No"| N
    M -->|"Yes"| B["Benchmark locally and add policy evidence"]
```

## Recommended starting points by form

Measured on an Apple M2 Pro with 32 GB, Ollama 0.40.8 and FreeLlama 0.6.1 on 2026-10-30, while
other builds kept the one-minute load near 14 on 12 cores. Every check had a known answer; the
batteries are small, so treat these as first picks to re-check on your machine, not rankings.

| Form | Model (download) | Measured | Speed (warm) | How an agent calls it |
|---|---|---|---|---|
| Text, tools, JSON, reasoning | `gpt-oss:20b` (13.8 GB) | 7/7: exact output, arithmetic, bat-and-ball, code reading, JSON extraction, tool call, 9/9 ticket routing | 1.4 s per routed ticket | `run_task {model:"gpt-oss:20b", prompt, think:"low"}`; add `tools` or `format:"json"` |
| Embeddings | `nomic-embed-text:v1.5` (274 MB) | Retrieval 4/4 top-1; ticket routing 9/9 from two examples per team | 51 ms per item, batched | `run_task {task:"embedding", input:[...], returnEmbeddings:true}` |
| Decisions (classify, route, score) | `strands-decider:2b-nvfp4` (1.3 GB) | Ticket routing 9/9, return-policy yes/no 3/3, damage rubric 2/3 | 0.17 s per question | `POST <serve>/v1/systemone {model, state, questions}`: passthrough, no MCP tool yet |
| OCR | `glm-ocr:latest` (2.2 GB) | 4/4 phrases from a dense report strip; 0/7 on text tasks | 11 s per strip at a 400-token cap | `run_task {task:"vision", model:"glm-ocr", prompt:"Text Recognition:", images:[...], stopOnRepetition:true}` |

`gpt-oss:20b` reasons before answering; the battery ran it with `think:"low"` and a 2,048-token
output cap, so leave room for the reasoning when you set `num_predict`. `glm-ocr` reads text from images only and keeps generating
after the visible text ends, so cap its output or set `stopOnRepetition`. The decision model needs
Apple silicon and answers only through Ollama's decision API; reach it through `serve` so the call
still passes FreeLlama's memory checks and stream cap (from 0.6.2 a busy CPU no longer refuses it).

### Classify text three ways

The same nine support tickets, routed to billing, technical or returns:

| Approach | Model | Correct | Per item | Use when |
|---|---|---:|---:|---|
| Embeddings and per-class centroids | `nomic-embed-text:v1.5` | 9/9 | 51 ms | Many items, stable classes, a few labeled examples per class |
| Decision model | `strands-decider:2b-nvfp4` | 9/9 | 0.5 s (first call includes the load) | You need probabilities, yes/no policy checks or rubric scores |
| Instruction model with JSON output | `gpt-oss:20b` | 9/9 | 1.4 s | Classes change per request or need explanation |

A decision API request names its questions; each answer comes back under the same name:

```bash
curl -s http://127.0.0.1:11435/v1/systemone -H 'content-type: application/json' -d '{
  "model": "strands-decider:2b-nvfp4",
  "state": "Why did my credit card get billed two times?",
  "questions": {"team": {"type": "choice", "instructions": "Which team handles this ticket?",
    "criteria": {"billing": "Charges and invoices", "technical": "App errors", "returns": "Returns and refunds"}}}
}'
```

## Text, code, and vision

`qwen3.8:27b-mlx` was the strongest general model in the local evaluations. It handled coding,
grounded research, image description, OCR, and summarization. Using one large multimodal model also
avoided contention between separate heavy text and vision runners.

In those evaluations it was strong at grounded lookup, long-context retrieval and strict instruction
following, needed `think:true` and a generous output budget for multi-step reasoning, and about a
third of its unsupervised code-review findings were false or context-limited.
These examples do not establish that a tag is installed or suitable on your host.

Dedicated OCR models answer best to the prompts their model card documents; send them with base64
`images` in `run_task`. A model that repeats its answer until the output cap can run with
`stopOnRepetition:true`. See [Route images](https://github.com/bgauryy/FreeLlama/blob/main/packages/mcp/README.md#route-images)
for the early stop and the `output_check` receipt.

`muse-glimmer:30b-mlx` was a credible alternative and answered 29 of 30 questions correctly
(96.7%) with the Bash adapter, but it was slower and did not add a required capability in this
workload.

Do not assume small research models are adequate. The local grounded-lookup sample fell to 2/8 at
7B, 3/8 at 3B, and 0/8 at 0.5B. `delegate_research` can refuse models whose runtime evidence marks
them unusable rather than spending tokens on a known-poor route.

## Embeddings

`nomic-embed-text` matched the highest recall with the lowest measured index time and model size in
the repository retrieval sample:

| Model | Recall at 3 | Index time | Dimensions | Size |
|---|---:|---:|---:|---:|
| `nomic-embed-text` | 5/6 | 4.2 seconds | 768 | 274 MB |
| `embeddinggemma:300m` | 5/6 | 4.9 seconds | 768 | 622 MB |
| `qwen3-embedding:0.6b` | 4/6 | 14.8 seconds | 1,024 | 639 MB |

Use embeddings for semantic similarity, grouping, deduplication, and classification. For exact code
keywords, repository search was both faster and more accurate in the measured sample.

## Place helper models on CPU when useful

A small embedding or helper model can be assigned to a second CPU Ollama process while a large
generation model remains resident on the primary GPU-capable process. This can overlap independent
work, but it does not make CPU inference fast. Apple silicon shares memory bandwidth; discrete-GPU
systems have different transfer and contention costs and need their own benchmark.

The measured warmed embedding-plus-completion workload improved from a 37.997-second sequential
median to a 28.233-second parallel median, a 1.346-times speedup. Treat that result as evidence for
this machine and workload, not a universal model-placement rule. Follow
[CPU and GPU model routing](CPU_GPU_ROUTING.md) to reproduce it.

## Turn measurements into routing evidence

`bench-all` measures local performance. A quality benchmark measures correctness. FreeLlama needs
both the generated task policy and local benchmark data before grading a route medium confidence.
See the [CLI reference](CLI.md#earn-medium-routing-confidence) for the commands.

## Model metadata

The MCP `models` tool reads your Ollama instance and can enrich results with Ollama's public model
library. Supported features and advertised use cases are metadata: they do not change routing
confidence, and exact-model benchmarks and policy remain the quality evidence. The CLI's `models`
command reports the core catalog and does not expose these enrichment flags.

| Request | Result | External access |
|---|---|---|
| `models {view:"installed"}` | Managed inventory with a `knowledge` object for each model | Local backend data only |
| `models {view:"installed", includeLibrary:true}` | Inventory enriched with public guidance for verified local tags | Fetches model family and tag pages from ollama.com |
| `models {view:"detail", model:"gemma3:1b"}` | Ollama details, maximum context, and supported-use explanations | Selected Ollama endpoint only |
| `models {view:"detail", model:"gemma3:1b", includeLibrary:true}` | Details plus public guidance after a tag and digest check | Selected Ollama endpoint and ollama.com |
| `models {view:"library", query:"coding"}` | Public family search with descriptions and advertised feature chips | ollama.com and optional local inventory/profile reads |
| `models {view:"library", model:"gemma3"}` | All public tags with size, digest prefix, context, modalities, and cloud status | ollama.com and optional local inventory/profile reads |


| Option | Valid views | Default | Meaning |
|---|---|---|---|
| `includeLibrary` | `installed`, `detail` | `false` | Requests public metadata; the default makes no website calls |
| `includeReadme` | Library step 2, or `detail` with `includeLibrary:true` | `false` | Returns README text, capped at 65,536 characters, with its format and truncation flag |
| `includeVerbose` | `detail` | `false` | Returns Ollama's license and Modelfile fields independently of README enrichment |
| `limit`, `cursor` | `raw`, library tag listings; `limit` also applies to library search | Tag/raw page size 20; search limit 10 | Pages large lists; the tag cursor rejects a changed tag list |
| `ollamaEndpoint` | Direct Ollama views and library inventory reads | Configured Ollama endpoint | Chooses the Ollama instance; enriched managed inventory uses each model's assigned backend |

An incompatible option returns an error before any lookup. Installed enrichment omits full READMEs;
request one model's detail to read its README.

### Supported features

`knowledge.supportedFeatures` preserves the feature strings from the selected source
(`supportedFeaturesSource`: `ollama_api` or `freellama_catalog`). Unknown future strings stay visible
but do not create routing requirements. `knowledge.supportedUseCases` explains each recognized
feature with `capability`, `use`, `reason`, and `basis:"supported_feature"`;
`knowledge.quality` stays `"metadata_only"`.

| Feature | Supported workflow | What to evaluate separately |
|---|---|---|
| `completion` | Text generation | Coding, summarization, reasoning, and instruction-following accuracy |
| `tools` | Tool calls | Argument correctness, tool selection, and agent-loop reliability |
| `vision` | Image input | OCR and visual understanding accuracy |
| `audio` | Audio workflows advertised by the model | Supported audio task and accuracy |
| `embedding` | Retrieval and similarity vectors | Retrieval quality on your documents |
| `thinking` | A thinking mode | Reasoning accuracy, latency, and token cost |
| `insert` | Suffix-based text insertion | Completion accuracy in the target code or text |
| `image` | Image generation | Output quality and supported controls |


### Public guidance and provenance

For a verified installed model, `knowledge.library` contains the source page, tag-page URL,
`fetchedAt`, `expiresAt`, `cached`, exact public tag metadata, and:

| Field | Meaning |
|---|---|
| `description` | The public family description |
| `advertisedFamilyCapabilities` | Family feature chips; these do not extend the local tag's capabilities |
| `advertisedUseCases` | Recognized coding, tool-calling, image-understanding/OCR, reasoning, embedding, summarization, question-answering, translation, and long-context claims |
| `advertisedUseCases[].evidence` | A bounded source excerpt, source URL, and fetch timestamp |
| `usageNotes` | Bounded excerpts about requirements, restrictions, context, memory, languages, or query prefixes |
| `readmeAvailable` | Whether a README was extracted |
| `readme` | Requested text with `format` (`markdown` or `text`) and `truncated` |


Advertised uses have `basis:"advertised"`, `scope:"family"`, and `quality:"unmeasured"`. Extraction
skips fenced code and negative statements; an omitted use means no recognized claim, not absence.
These labels are not the `run_task` task enum. Local capabilities filter advertised uses: a family
vision chip cannot make `gemma3:1b` a vision candidate.

Installed enrichment requires the exact public tag and a digest-prefix match of at least 12 hex
characters. Custom names are never mapped by architecture or guessed base model; without a verified
match, public guidance is withheld. Library step 2 describes public models before installation, so
its guidance stays family-scoped; each `tags[]` entry carries its own modalities, context, digest
prefix, and `cloud` flag. Search's `cloudAvailable` badge does not prove local downloads are absent.

### Lookup states

| `library.status` | Meaning | Next action |
|---|---|---|
| `not_requested` | Website enrichment was not requested | Set `includeLibrary:true` if external lookup is useful |
| `matched` | Installed tag and public digest prefix match | Read the sourced guidance, then evaluate the exact tag |
| `available` | Public family and tag pages were parsed in library step 2 | Inspect the exact tag before recommending a download |
| `unverified` | Digest or current backend inventory could not be verified | Recheck local inventory; retain local feature information |
| `digest_mismatch` | The public tag differs from the installed digest | Treat public guidance as unavailable for this installed model |
| `tag_not_found` | The parsed public tag list has no exact matching tag | Check the source and custom-model provenance |
| `unmatched` | The name cannot resolve to an exact public Ollama name | Keep use-case suitability unknown until provenance is verified |
| `not_found` | A requested public page returned HTTP 404 | Check the source URL or custom name |
| `unavailable` | Transport, timeout, HTTP refusal, or page-size limit prevented lookup | Retry later; local results remain available |
| `parse_error` | HTML content or tag rows were not recognized | Inspect the source; do not conclude the model is absent |
| `remote_model`, `remote_tag` | Remote Ollama fields or a hosted tag identify cloud execution | Select a local download tag for local execution |


`localModel:false` flags detected remote entries; cloud tags never become local trial
recommendations. This metadata is not processor-placement evidence.

### Cache and request bounds

Each MCP process caches at most 128 families: successes for one hour, failures for one minute;
concurrent requests share one lookup and a restart clears the cache. Installed enrichment runs at
most four family lookups concurrently. Requests use GET, refuse redirects, cap each page at 2 MiB,
and time out after the smaller of 10 seconds and `FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS`. They disclose
the requested public family name to ollama.com. Download size does not establish runtime memory
fit, placement, or quality, and model discovery never authorizes a pull. Sources:
[model details API](https://docs.ollama.com/api-reference/show-model-details),
[model-list API](https://docs.ollama.com/api/tags).
