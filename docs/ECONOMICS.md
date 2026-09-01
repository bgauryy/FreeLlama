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
| Tool schemas, per turn | ~3k (measured on the former eight-tool surface; six tools today) | 0 | **100%** |

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
