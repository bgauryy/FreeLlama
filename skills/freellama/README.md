# freellama skill

The orchestration playbook for any agent driving the
[FreeLlama](https://github.com/bgauryy/FreeLlama) MCP server or its `npx freellama` CLI.

**Use it when you want to offload computation to free local models** — vision/OCR, grounded code
research over real files, embeddings, bulk transforms — instead of spending frontier-model context.

Start at [SKILL.md](SKILL.md). The flow line, the five flows (A-E), and the three-tier table are the
operating core; each flow ends in a signal you must read. Everything under `references/` is
evidence-dense detail loaded on demand:

| File | Owns |
|---|---|
| `references/task-delegation.md` | what to offload, what is cheapest, how to read a result |
| `references/model-selection.md` | which model, and making `minConfidence:"medium"` reachable |
| `references/ollama-config.md` | memory arithmetic and the nine `OLLAMA_*` settings |
| `references/troubleshooting.md` | symptom → cause → fix |
| `references/proxy-vs-serve.md` | which mode, which routes exist in each |
| `references/reliability.md` | retry, backoff, timeout, process restart |
| `references/disk-cleanup.md` | what may be deleted, and what must never be |
| `references/model-profile-qwen3.8-27b-mlx.md` | per-field evidence for one model |

`examples/local-rag.sh` is runnable. `scripts/check.sh` is the human-facing health audit (exit 0 =
healthy); it is read-only and standalone — set `FREELLAMA_REPO` only if you want its optional
binary-freshness check.

This folder is self-contained: it links to FreeLlama sources on GitHub rather than assuming it lives
inside a checkout.

Everything quantitative was measured on one machine (M4 Pro, 52GB). The method transfers; re-derive
the numbers for yours with `doctor` and `models`.
