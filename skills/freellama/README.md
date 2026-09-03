# freellama skill

The orchestration playbook for any agent driving the
[FreeLlama](https://github.com/bgauryy/FreeLlama) MCP server or its `npx @octocodeai/freellama` CLI.

**Use it when you want to offload computation to free local models** — vision/OCR, grounded code
research over real files, embeddings, bulk transforms — instead of spending frontier-model context.

Start at [SKILL.md](SKILL.md). The flow line, the five flows (A-E), and the three-tier table are the
operating core; each flow ends in a signal you must read. Everything under `references/` is
evidence-dense detail loaded on demand:

The ownership table in `SKILL.md` is load-bearing: the agent owns decomposition/concurrency, the
operator owns topology and lifecycle approval, FreeLlama owns governed routing/admission, and
Ollama plus the operating system own actual model execution.

| File | Owns |
|---|---|
| `references/task-delegation.md` | what to offload, what is cheapest, how to read a result |
| `references/model-selection.md` | which model, and making `minConfidence:"medium"` reachable |
| `references/ollama-config.md` | memory arithmetic and the 11 memory-governing settings |
| `references/resource-routing.md` | guarded CPU/GPU preference, feedback, and per-backend admission |
| `references/context-management.md` | calibrated coding-agent budgeting, compaction, and pinned overflow |
| `references/troubleshooting.md` | symptom → cause → fix |
| `references/proxy-vs-serve.md` | which mode, which routes exist in each |
| `references/reliability.md` | retry, backoff, timeout, process restart |
| `references/disk-cleanup.md` | what may be deleted, and what must never be |
| `references/model-profile-qwen3.8-27b-mlx.md` | per-field evidence for one model |

The short references own decisions and workflows. `assets/evidence/` preserves the detailed trial
transcripts, incident histories, tables, and sources without loading them into every agent turn.

`examples/local-rag.sh` is runnable. `scripts/check.sh` is the human-facing health audit (exit 0 =
healthy); it is read-only and standalone — set `FREELLAMA_REPO` only if you want its optional
binary-freshness check. Run the Bash audit on macOS, Linux, or Windows through WSL; native Windows
operators can use `freellama doctor` for the same core diagnostics.

This folder is self-contained: it links to FreeLlama sources on GitHub rather than assuming it lives
inside a checkout.

Everything quantitative was measured on one machine (M4 Pro, 48 GB). No hardware name or measured
number is a routing default. `doctor` discovers host RAM on macOS, Linux, and Windows; `models`
reports observed runner placement. Re-derive admission, residency, and CPU/GPU choices on the
machine that runs the workload.
