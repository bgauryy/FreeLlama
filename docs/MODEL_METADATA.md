# Ollama model metadata

The MCP `models` tool gives an agent the inventory and capability metadata it needs before
offloading work. It reads your Ollama instance and can enrich results with descriptions from
Ollama's public model library. Supported features and advertised use cases are metadata;
exact-model benchmarks and policy supply quality evidence. Public descriptions do not change
routing confidence, and a successful response still needs task-specific verification.

## Requests

| Request | Result | External access |
|---|---|---|
| `models {view:"installed"}` | Managed inventory with a `knowledge` object for each model | Local backend data only |
| `models {view:"installed", includeLibrary:true}` | Inventory enriched with public guidance for verified local tags | Fetches model family and tag pages from ollama.com |
| `models {view:"detail", model:"gemma3:1b"}` | Ollama details, maximum context, and supported-use explanations | Selected Ollama endpoint only |
| `models {view:"detail", model:"gemma3:1b", includeLibrary:true}` | Details plus public guidance after a tag and digest check | Selected Ollama endpoint and ollama.com |
| `models {view:"library", query:"coding"}` | Public family search with descriptions and advertised feature chips | ollama.com and optional local inventory/profile reads |
| `models {view:"library", model:"gemma3"}` | All public tags with size, digest prefix, context, modalities, and cloud status | ollama.com and optional local inventory/profile reads |

These are MCP tool requests. The CLI's `models` command reports the core catalog and does not expose these enrichment flags. For CLI setup and routing, see the [CLI reference](CLI.md).

| Option | Valid views | Default | Meaning |
|---|---|---|---|
| `includeLibrary` | `installed`, `detail` | `false` | Requests public metadata; the default makes no website calls |
| `includeReadme` | Library step 2, or `detail` with `includeLibrary:true` | `false` | Returns README text, capped at 65,536 characters, with its format and truncation flag |
| `includeVerbose` | `detail` | `false` | Returns Ollama's license and Modelfile fields independently of README enrichment |
| `limit`, `cursor` | `raw`, library tag listings; `limit` also applies to library search | Tag/raw page size 20; search limit 10 | Pages large lists; the tag cursor rejects a changed tag list |
| `ollamaEndpoint` | Direct Ollama views and library inventory reads | Configured Ollama endpoint | Chooses the Ollama instance; enriched managed inventory uses each model's assigned backend |

An incompatible option returns an error before any lookup. Installed enrichment omits full READMEs to keep inventory responses bounded; request one model's detail to read its README.

## Supported features

`knowledge.supportedFeatures` preserves the feature strings available from the selected source. `supportedFeaturesSource` is `ollama_api` for direct Ollama metadata and `freellama_catalog` for the managed catalog. The catalog contains normalized routing capabilities; unknown future strings remain visible in `detail`, `raw`, or enriched installed inventory when Ollama's tag response supplies them. Unknown strings do not create new routing requirements.

`knowledge.supportedUseCases` explains each recognized feature:

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

Each explanation contains its `capability`, `use`, `reason`, and `basis:"supported_feature"`. Ollama runtime/version support still applies. The inventory's existing benchmark and policy fields remain separate from `knowledge.quality:"metadata_only"`.

## Public guidance and provenance

For a verified installed model, `knowledge.library` contains the source page, full tag-page URL, `fetchedAt`, `expiresAt`, `cached`, and exact public tag metadata. It also contains:

| Field | Meaning |
|---|---|
| `description` | The public family description |
| `advertisedFamilyCapabilities` | Family feature chips; these do not extend the local tag's capabilities |
| `advertisedUseCases` | Recognized coding, tool-calling, image-understanding/OCR, reasoning, embedding, summarization, question-answering, translation, and long-context claims |
| `advertisedUseCases[].evidence` | A bounded source excerpt, source URL, and fetch timestamp |
| `usageNotes` | Bounded excerpts about requirements, restrictions, context, memory, languages, or query prefixes |
| `readmeAvailable` | Whether a README was extracted |
| `readme` | Requested text with `format` (`markdown` or `text`) and `truncated` |

Advertised uses have `basis:"advertised"`, `scope:"family"`, and `quality:"unmeasured"`. Extraction recognizes positive prose mentions and skips fenced code and explicit negative statements. It is a discovery aid, not a complete classification: an omitted use means extraction found no recognized claim. Usage notes also remain family-scoped and can describe other variants. Read the source for the full instructions. These use labels are not the `run_task` task enum; choose a supported routing profile with the capabilities and context your workload requires.

Local capabilities filter advertised uses. For example, `gemma3:1b` accepts text, while larger Gemma 3 variants accept images. A family-level vision chip cannot make the 1B tag a vision candidate. Likewise, an embedding-only tag does not acquire text-generation workflows from its family description.

Installed enrichment requires the exact public tag and a matching digest prefix of at least 12 hexadecimal characters against the local 64-character digest. Custom names are never mapped by architecture or guessed base model. Community names use their exact Ollama namespace. Without a verified match, public guidance is withheld. A local modification can therefore prevent enrichment even when its name resembles a public model.

Library step 2 describes public models before installation, so its top-level `library` guidance remains family-scoped without claiming a local digest match. Each `tags[]` entry carries its own modalities, context, digest prefix, and `cloud` flag. Search returns `cloudAvailable` for the hosted-access badge and `cloudOnly:null`: a family badge alone does not establish that local downloads are absent.

## Lookup states

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

`localModel:false` flags detected remote entries. Known remote entries skip public enrichment, and cloud tags never become local trial recommendations. This metadata is not processor-placement evidence; inspect the execution observation for CPU/GPU placement.

## Cache and request bounds

Each MCP process caches at most 128 families. Successful lookups expire after one hour, failures after one minute; concurrent requests for a family share one lookup. Expired content is not served as current metadata. Restarting the MCP process clears this in-memory cache.

Installed enrichment runs at most four family lookups concurrently. Each lookup requests the family page and complete `/tags` page in parallel. Requests use GET, refuse redirects, cap each page at 2 MiB, and time out after the smaller of 10 seconds and `FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS`. These requests retrieve metadata without loading or executing a model. They disclose the requested public family name to ollama.com.

The local inventory is not cached by the enrichment layer. A changed backend inventory or failed lookup leaves the managed model and its existing evidence intact. A size-based library recommendation is only a download trial candidate: download size does not establish runtime memory fit, CPU/GPU placement, or task quality. Model discovery never authorizes a pull.

## Sources and related documentation

- [Ollama model details API](https://docs.ollama.com/api-reference/show-model-details) provides runtime capability and architecture metadata.
- [Ollama model-list API](https://docs.ollama.com/api/tags) provides installed tags and digests.
- [Ollama Gemma 3 page](https://ollama.com/library/gemma3) illustrates variant-specific modalities.
- [MCP tool reference](../packages/mcp/README.md#tools) documents the complete tool surface.
- [Choose local models](MODEL_SELECTION.md) explains quality qualification and measured results.
- [Test FreeLlama](TESTING.md) lists the parser, enrichment, and protocol checks.
