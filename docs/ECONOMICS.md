# Token economics

This reference keeps benchmark detail out of the README's "what, why, and run it" path. The
per-call rows come from one machine (M4 Pro, 48 GB unified memory); values marked with `~` and the
session projection are estimates derived from those calls.

## Per-call, measured

| Work | Source tokens | Returned tokens | Preserved |
|---|---:|---:|---:|
| 6 grounded code questions | 59,208 | 1,742 | **97.1%** |
| 4 text embeddings (307 ms) | 17,600 | ~200 | **98.9%** |
| 1 image, OCR, byte-exact | 1,970 | 37 | **98.1%** |
| Historical tool-schema surface, per turn | <3.2k | 0 | **100%** |

## Current MCP-envelope audit (2026-09-03)

The earlier table describes a historical workload on an M4 Pro. This audit measures the current
eight-tool MCP build on the current host. JSON tokenization varies by client and model, so it records
bytes first and gives `characters / 4` only as a deliberately rough token estimate.

| Current result | Canonical structured chars | Compact text chars | Duplicate chars avoided | Rough duplicate-token estimate |
|---|---:|---:|---:|---:|
| `doctor {view:"summary"}` | 3,099 | 35 | 3,064 | ~766 |
| `doctor {view:"scheduler"}` | 3,933 | 35 | 3,898 | ~975 |
| `doctor {view:"config"}` | 7,373 | 86 | 7,287 | ~1,822 |
| `doctor {view:"full"}` | 10,836 | 185 | 10,651 | ~2,663 |
| `models {view:"raw", limit:1}` | 447 | 43 | 404 | ~101 |

These are **not automatically billed-token savings**. They are the second serialized text copy
that FreeLlama does not emit. An MCP host that feeds both `structuredContent` and text into a model
can avoid roughly the listed amount; a host that already keeps only one representation does not.
The canonical structured payload still exists for deterministic clients that need it.

The current eight-tool schema plus initialization instructions measured 17,935 characters, or
~4,484 tokens using the same rough conversion. The integration suite enforces a ceiling below 4,500
estimated tokens. This is tool-schema **rent**, not a saving: clients pay it whenever they expose
the entire tool list to a model. Read the bundled documentation resource on demand and do not expose
unneeded tools in a client that supports tool filtering.

Delegated research has a different economics boundary. It keeps source files and full tool
observations out of the calling agent's active prompt, but it spends local Ollama input/output
tokens in the delegated agent. The historical research rows below remain useful workload evidence;
they are not a claim that the current installed model or host can reproduce those savings. Measure
the same question, source size, model, context policy, and client transcript before using a number
as a production budget.

## Scaled to a session

20 grounded questions, 200 turns, one embedding index over 322 chunks:

| Component | Tokens preserved |
|---|---:|
| Research offload | 191,553 |
| Embedding vectors withheld | 354,200 |
| Schema rent avoided, after ~90% prompt-cache discount | 59,740 |
| **Total** | **≈ 605,000 — about 3 × a 200K window** |

## Measurement and projection limits

Schema rent is nominally **597,400** tokens (200 turns × the former eight-tool schemas). Including
the nominal figure puts the headline at **1.14M — nearly double**. This report discounts it by ~90%
because prompt caching makes cached input far cheaper,
and because the rent only accrues at all if a *separate* small model holds the tool schemas. The
first two rows are the uncontestable part: that data physically never reaches the orchestrator.

**Do not lead with this number.** Providers, caching strategies, context windows, and pricing all
move, and a token count moves with them. The durable benefit is **context isolation** — the
orchestrator never receives whole files, raw vectors, OCR output, intermediate research, or
repetitive schemas. That improves context *quality* as well as cost, and it does not depend on
anyone's price list.

## The cost side

| | |
|---|---|
| Grounded question latency | **7–62 s** vs ~1 s for a frontier model that already holds the file |
| Where it goes | **51% prompt re-evaluation**, 42% generation, 7% model load (measured on a 4-turn run) |
| Scaling | turn-dominated: `seconds ≈ 9.8 × tool_calls`, correlation **0.811** over 60 runs |
| Break-even | past ~1k tokens of source the token maths wins; below that, read the file directly |
| Judgment ceiling | ~67% accurate, in the same confident tone as the ~99% case |

**Correction (measured same day): prefix-cache reuse WORKS on this path.** An earlier revision of
this file claimed the opposite by misreading `prompt_eval_count`, which counts *all* prompt tokens
whether or not they were served from cache. The durations tell the truth:

| Probe (raw `/api/chat`, 2,462-token prefix) | prompt_eval |
|---|---:|
| turn 1, cold | 18,631 ms |
| turn 2, warm — tiny suffix appended | **281 ms** |
| a *different* conversation interjected | 22,467 ms (its own cold prefill) |
| turn 3 of the original, after the interjection | **285 ms — cache survived** |

So the per-turn cost is **prefill of NEW tokens plus generation**, not re-reading old context, and
the cache even survives interleaved conversations. The `seconds ≈ 9.8 × tool_calls` relationship
still holds empirically — each turn adds a fresh observation to prefill (~130 tok/s measured) and a
generation (~40 tok/s) — but the mechanism is new-token cost, not context re-evaluation. Fewer,
better-aimed commands remain the lever; "the cache is broken" is not the reason.

**One consequence worth acting on: context compaction is cache-hostile.** `fit_to_context` rewrites
old observations when the window is nearly full, which changes the byte prefix and invalidates the
cache from that point — the next turn re-prefills everything after the edit. The cheap mitigation is
headroom: `OLLAMA_KV_CACHE_TYPE=q8_0` roughly halves KV memory (flash attention is already auto-on,
measured and documented in `doctor`), which buys double the context for the same memory and makes
compaction — and therefore invalidation — rare.
