# Result checks and local tool-calling evidence

`check:` lines are review hints. A clean result does not establish factual correctness, a safe
edit, an appropriate tool choice, or authority to execute a call. This page keeps two separate
measurements: deterministic accuracy of those hints and observed tool-calling behavior of exact
installed local models. [Machine-local model calibration](MODEL_SELECTION.md#turn-measurements-into-routing-evidence)
measures another concern: which eligible model has local performance and policy evidence. Neither substitutes for the calling agent's verification.

## Accuracy of result checks

The versioned [30-case labeled corpus](../scripts/reliability/check-fixtures.mjs) tests human-labeled
review risks against the actual exported check functions. Every input, label, emitted check and
outcome from the original checker is in [the baseline report](assets/result-check-accuracy.json);
the repaired checker has a separate [numeric-tokens-v2 report](assets/result-check-accuracy-v2.json). These synthetic
regression cases include deliberate blind spots; their proportions do not estimate production
accuracy. The labels and detector thresholds were visible during implementation.

A positive means a specific warning about a review-worthy risk, rather than any nonempty `check:`
line. Generic spot-check reminders and structural success messages are not positive detections.
Unsupported tool-schema keywords produce an explicit unchecked warning and count as a review risk.

The original checker baseline:

| Review-risk family | Cases | True positives | False positives | True negatives | False negatives | Precision | Recall |
|---|---:|---:|---:|---:|---:|---:|---:|
| Source factual risk | 12 | 2 | 2 | 2 | 6 | 50% | 25% |
| Instructions in input data | 4 | 1 | 1 | 1 | 1 | 50% | 50% |
| Dropped extraction values | 4 | 1 | 0 | 1 | 2 | 100% | 33.3% |
| Tool structural risk | 10 | 8 | 0 | 2 | 0 | 100% | 100% |

Precision is `TP / (TP + FP)`; recall is `TP / (TP + FN)`. Empty denominators are unknown. Do not
combine the families into one reliability score: they ask different questions.

The 2026-11-15 numeric-token repair fixes two existing labeled cases: `8080` no longer matches a
source token `18080`, and comma grouping is normalized in both directions. Human risk labels and
the original report are unchanged. On the same 12 source-risk cases, the repaired checker has
TP 3, FP 1, TN 3, FN 5: precision 75%, recall 37.5%. Other family counts are unchanged. These cases are
now regression evidence after fixes, not held-out confirmation or production accuracy. The
remaining genuine misses stay visible in both reports. Exact token matching can also warn about
valid derived counts such as `×2` and `×1`; the checker does not verify arithmetic or counting.

The source check looks for short backtick spans and numeric tokens somewhere in the supplied
files. Reversed relationships, incorrect negation, unquoted invented names,
and quotes longer than 60 characters can escape it. Correct derived totals and other formatting changes can still trigger warnings. The extraction check recognizes certain identifiers left null, but does
not compare a wrong non-null value. The injection check catches selected instruction patterns;
benign quotations can trigger it and paraphrases can evade it. A warning is a reason to inspect
the answer, not proof of hallucination or a successful attack.

Tool checks reject malformed native `response.message.tool_calls` containers and nonstring
function names, and flag duplicate provided names as ambiguous. They validate known names, JSON-object arguments, nested properties, required fields,
additional properties, array items, JSON types, enums, constants, numeric bounds and string/array
length bounds. Objects allow additional properties unless their schema forbids them. Unsupported
keywords, including `$ref`, `pattern`, `format`, `oneOf` and `anyOf`, are explicitly unchecked.
Malformed values for supported validation keywords also produce unchecked warnings.
This is a bounded structural checker, not a complete JSON Schema implementation or a tool policy.
A structurally valid call can still name the wrong city or perform an unauthorized action.

Reproduce the report with no Ollama, downloads or inference:

```sh
node scripts/reliability/result-check-accuracy.mjs --out /tmp/result-check-accuracy.json
node --test scripts/reliability/*.test.mjs
```

The output file must not already exist. The offline test compares the generated report to the
versioned current artifact and checks that the baseline human labels remain unchanged and keeps known false positives and false negatives visible. After changing
a detector, review changed labels and outcomes, regenerate into a new file, then replace the
artifact deliberately. Other unit tests cover confidence bands, nearest-match reminders,
SEARCH/REPLACE matches, padding, fallback receipts and vision heuristics; these are contracts,
not measured factual accuracy of those families.

## Public tool-calling smoke suite

The [runner](../scripts/reliability/tool-calling.mjs) calls Ollama directly with the frozen
[tool schemas, prompts and deterministic grader](../scripts/reliability/tool-cases.mjs). It tests:

- Choosing a weather tool among alternatives and returning the tool result.
- Integer arguments and a second assistant turn after arithmetic output.
- Nested object and array arguments under closed schemas.
- Avoiding tools for a plain response and asking for a missing city.
- Declining an unavailable action without inventing a tool.
- Treating an instruction inside a simulated tool result as untrusted data.

No real tool action is performed. A simulated result is supplied only after the expected tool
and exact argument values match. The grader requires the requested final text, so inspect the raw
transcript when a semantically similar answer fails. Passing the seven public cases once is a
smoke result, not a population reliability estimate, a safety guarantee, or a model ranking.
The runner does not measure FreeLlama admission, routing, MCP transport, or physical CPU/GPU
placement. Tool choice and argument columns are first-turn outcomes on expected-call cases;
follow-up is conditional on an accepted first call. Complete-case success includes both turns.

[Ollama's tool-calling contract](https://docs.ollama.com/capabilities/tool-calling) leaves execution
to the caller: preserve the assistant message, append each `{role:"tool", tool_name, content}`
result, and send the updated conversation with the tools. It also documents parallel calls,
agent loops and streamed accumulation. The present suite measures only single-call first turns
and one follow-up; it has no live evidence for parallel calls, longer dependent loops, streamed
tool calls or real tool side effects. [The chat API](https://docs.ollama.com/api/chat) defines the
native `message`, `done` and tool fields used by the grader.

Each run records exact installed tags, manifest digests, sizes, capabilities, Ollama and Node
versions, harness/case SHA-256 hashes, Git commit and dirty files, host memory/load before and
after, residency, all request/response bodies, errors and every planned denominator. An Ollama
manifest digest identifies the tag manifest; it is not a GGUF file hash. No retry is hidden.
New runs also record an acquisition UUID and strict grader version. The `tool-strict-v2` grader
rejects malformed call containers and requires a question or request for the missing city rather
than accepting any city mention. It is still a bounded oracle, not full semantic verification.
Legacy reports replay their original v1 grader so historical scores are not silently rewritten.
Infrastructure errors and timeouts remain non-passes in complete-case denominators; cases not
attempted after a stop remain visible. HTTP abort does not prove inference has stopped.

The runner requires an exact installed tag that reports `tools` capability. It never pulls,
unloads, deletes, or starts a service. Requests are sequential with context 4096, temperature 0,
seed 42, `think:false`, at most 256 generated tokens and a 45-second request timeout by default.
`keep_alive:"60s"` lets Ollama expire the warmed model naturally. Response bodies are capped at
2 MiB. Inspect current available memory, residency and other workloads before starting; disk
size does not establish runtime fit. Check final residency before a subsequent run, especially
after a timeout.

```sh
# Read-only inventory; every model remains unmeasured.
node scripts/reliability/tool-calling.mjs --inventory --out /tmp/tool-inventory
# Explicit bounded inference on one exact installed tag; output directory must be new.
node scripts/reliability/tool-calling.mjs --model 'EXACT_INSTALLED_TAG' --out /tmp/tool-run
# A declared repeat count runs every case that many times without automatic retries.
node scripts/reliability/tool-calling.mjs --model 'EXACT_INSTALLED_TAG' --repeats 3 --out /tmp/tool-repeat
```

`--endpoint`, `--timeout-ms` and `--max-tokens` override the defaults and are preserved in the
manifest. Changing them changes the evaluation conditions. Keep all audit files when sharing a
result: `manifest.json`, `trials.jsonl`, `summary.json`, `TABLE.md`, and `final-state.json`; failed
preflight runs retain `fatal.json`. The offline runner tests use fake HTTP responses and prove
harness contracts only.

## Measured on 2026-11-14

Ollama 0.40.15 on macOS arm64, Node v26.4.0, 32 GiB RAM and 12 logical CPUs. Each selected model
ran the same seven public cases once, with identical harness/case hashes and the default request
settings above. Both started with no resident Ollama models. Estimated available memory was
11.9 GB before LFM3-VL and 6.7 GB before Bonsai; macOS pressure level was 1. Other development
work ran on the host and swap was already in use, so these runs support no latency or resource
comparison. Models expired naturally; no pull, explicit unload or deletion was issued.

| Exact installed tag | Complete strict cases | Tool choice | Exact arguments | Follow-up | Evidence |
|---|---:|---:|---:|---:|---|
| `lfm3-vl:2b-q4_K_M` | 5/7 | 4/4 | 4/4 | 3/4 | [Raw run](assets/tool-calling-2026-11-14-lfm3-vl/trials.jsonl), [manifest](assets/tool-calling-2026-11-14-lfm3-vl/manifest.json) |
| `bonsai-2:9b` | 6/7 | 4/4 | 4/4 | 3/4 | [Raw run](assets/tool-calling-2026-11-14-bonsai-9b/trials.jsonl), [manifest](assets/tool-calling-2026-11-14-bonsai-9b/manifest.json) |
| `bonsai-2:27b` | — | — | — | — | Unmeasured |
| `nimble-v3:9b-q4_K_M` | — | — | — | — | Unmeasured |
| `lfm3-omni:2b-q4_K_M` | — | — | — | — | Unmeasured |
| `hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M` | — | — | — | — | Unmeasured |
| `hf.co/nvidia/NVIDIA-Nemotron-3.5-Bolt-E4B-4GB:Q4_K_M` | — | — | — | — | Unmeasured |
| `embeddinggemma-2:740m-q8_0` | — | — | — | — | Unmeasured |
| `lfm3-audio:2b-q4_K_M` | — | — | — | — | Unmeasured |
| `phonon:en` | — | — | — | — | Unmeasured |
| `strands-decider:2b-nvfp4` | — | — | — | — | Unmeasured |
| `nomic-embed-text:v1.5` | — | — | — | — | Unmeasured |

The [generated full table](assets/tool-calling-table.md) includes every installed tag and its
manifest digest. A dash means no trial was run; installed status and advertised tools support
are not reliability evidence. Non-tool models remain unmeasured rather than receiving a score
for a task they do not advertise.

All four expected first tool calls and arguments were correct for both measured models. Both
also avoided unnecessary calls in all three non-call cases. Strict weather continuation failed
because LFM3-VL returned `WEATHER:18` and Bonsai returned `WEATHER:18°C`, while the prompt requested
exactly `WEATHER:18C`. LFM3-VL's other failure was an appropriate prose refusal to delete a file
instead of the requested exact `UNAVAILABLE`. These are format failures, not observed dangerous
tool execution or wrong weather values. The prompt-in-tool-result case passed once per model;
that does not establish general injection resistance. The small exposed suite cannot rank the
models or justify broad reliability percentages.

Regrade the raw responses and regenerate the combined table without inference:

```sh
node scripts/reliability/tool-report.mjs \
  --run docs/assets/tool-calling-2026-11-14-lfm3-vl \
  --run docs/assets/tool-calling-2026-11-14-bonsai-9b \
  --out /tmp/tool-calling-table.md
```

The reporter checks recorded initial prompts, tool schemas, model tags and options against the
frozen case definition and manifest, and verifies every assistant/tool-result continuation. A
manifest-recorded `--max-tokens` override is allowed; an unexplained changed request is rejected.
It replays the recorded strict grader, verifies summaries and rejects pooling incompatible
harness/case hashes, runtime versions, request limits or strict grader versions. Copied directory
paths cannot multiply evidence: duplicate acquisitions are rejected by recorded run identity.
These checks validate retained transcripts; hashes and captured requests do not independently
attest physical model execution, unchanged weights, host identity or an untampered acquisition.
Ordinary offline tests reproduce the checked-in table from the raw artifacts.

## Post-hoc interpretation of strict failures

The separate [`tool-format-adjudication-v1` report](assets/tool-outcomes-v1.json) applies narrow
deterministic rules to the same 14 retained responses. It preserves all 11 historical strict
passes and identifies the three failures described above as format-only: the reference Celsius
temperature with an omitted/degree suffix, or a whole-response explicit inability to delete files
without a tool call. Wrong temperatures, different units, extra calls, contrary claims or text
outside those rules remain unresolved. The rules were written after reading these failures;
this is explanatory regression evidence, not a new inference, an independent semantic quality
study, a replacement score or a finding that every strict pass is factually correct.

```sh
node scripts/reliability/tool-outcomes.mjs \
  --run docs/assets/tool-calling-2026-11-14-lfm3-vl \
  --run docs/assets/tool-calling-2026-11-14-bonsai-9b \
  --out /tmp/tool-outcomes-v1.json
```
