# Choosing local models

Moved out of the README: this is reference material you consult once, not something a newcomer needs
in their first two minutes. The method is what transfers — the specific model names and numbers are
from one machine (M4 Pro, 52GB unified memory) and you should re-derive them for yours with
`freellama models` and `search_models`.


Measured on this machine, not inferred from model cards or download counts. Re-derive for your own
hardware with `npx freellama models` and `search_models` — the method transfers, the numbers may not.

### Text, code, and vision — one model covers all of it

**`qwen3.8:27b-mlx`** (18 GB). It handles coding, grounded research, image description, OCR, and
summarisation, and on this machine it scored highest for accuracy and lowest for latency among the large models tested.
Vision works properly: it described a UI mockup accurately and transcribed a terminal screenshot
including an identifier a dedicated OCR model got wrong.

`muse-glimmer:30b-mlx` is a credible alternative — it won the largest-sample benchmark in this repo
(96.7% over 90 trials) — but it is slower and does not add a capability qwen lacks. Running one
large model rather than two also removed memory contention: qwen's vision latency dropped from ~37s
to ~14s afterwards.

**Do not go small for research.** Accuracy collapses below roughly 12B: measured here at 7B 2/8,
3B 3/8, 0.5B 0/8 on grounded lookups. A fast wrong answer costs more than the tokens it saved,
which is why `delegate_research` refuses a model measured unusable instead of running it.

### Embeddings — the cheapest thing you can run locally

**`nomic-embed-text`** (274 MB). Benchmarked against the alternatives on real retrieval over this
repo — 152 chunks, 6 questions with known-correct files:

| Model | recall@3 | Index time | Dims | Size |
|---|---|---|---|---|
| **`nomic-embed-text`** | **5/6** | **4.2s** | 768 | **274 MB** |
| `embeddinggemma:300m` | 5/6 | 4.9s | 768 | 622 MB |
| `qwen3-embedding:0.6b` | 4/6 | 14.8s | 1024 | 639 MB |

**`qwen3-embedding` ranks first on ollama.com and came last here**, at 3.5x the indexing
cost. Site rank is not retrieval quality — `search_models` returns a `pulls` field precisely so you
can judge rather than trust position. `embeddinggemma` is a fine substitute; `nomic-embed-text` is
smaller, faster, and already the most-downloaded embedding model by a wide margin.

Embeddings are the strongest local play by a distance: indexing this repo's source cost **zero**
tokens returned to the orchestrator and under ten seconds. There is no sampling, so nothing to
hallucinate. Index once, query many times.

**But use them for the right thing.** For finding code by keyword, `grep` beat embedding search
here on accuracy, latency and cost simultaneously. Reach for embeddings when there is no keyword to
search for — grouping, deduplication, classification, semantic similarity.

