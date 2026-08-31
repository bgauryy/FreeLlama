# freellama skill

The orchestration playbook for any agent driving the FreeLlama MCP server.

**Use it when you want to offload computation to free local models** — vision/OCR, grounded code
research over real files, embeddings, bulk transforms — instead of spending frontier-model context.

Start at [SKILL.md](SKILL.md): the flow line at the top, the three-tier table, and the
order-of-operations are the operating core. Everything under `references/` is evidence-dense detail
loaded on demand; `examples/local-rag.sh` is runnable; `scripts/check.sh` is the human-facing health
audit (exit 0 = healthy).

Everything quantitative in here was measured on one machine (M4 Pro, 52GB). The method transfers;
re-derive the numbers for yours with `doctor` and `models`.
