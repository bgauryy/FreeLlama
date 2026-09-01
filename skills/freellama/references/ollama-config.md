# Ollama memory and KV configuration

Load before changing `OLLAMA_*`, co-residenting models, or diagnosing silent CPU spill. Why:
declared defaults differ from resolved defaults, and KV/concurrency settings multiply memory.

1. Run `doctor`, confirm CLI/server versions, and inspect `models{view:"resident"}`.
2. Treat partial GPU placement as a slowdown; unload another model or lower context.
3. For large GPU + small CPU helper, use two Ollama processes, exact `--cpu-model` assignments,
   and require observed CPU `size_vram:0`. Neither process isolation nor managed `num_gpu:0` is
   universal: Nomic obeyed it, while `qwen3.8:27b-mlx` remained 100% in VRAM on measured Metal.
4. Start with about 60% of host memory for one large model plus KV and other residents on unified
   memory. On a discrete-GPU host, check VRAM separately; host RAM is not a substitute.
5. Unset `OLLAMA_MAX_LOADED_MODELS` resolves to 3 × GPU count; use 1 when only one model per
   isolated backend should remain loaded.
6. `OLLAMA_NUM_PARALLEL` multiplies KV by context. At 1, one backend serializes; separate CPU/GPU
   processes can still overlap.
7. Rate `q8_0` KV cache **8/10** for long-context or parallel work: roughly half the `f16` memory
   with very small upstream-described precision loss. Qualify model quality before process-wide
   rollout. Reject `q4_0` as a default because its quality tradeoff is larger.
8. Research-adapter context is per-call and validated. Before changing its estimator, margins, or
   pinned-overflow policy, load `references/context-management.md`.

Apply variables to the environment of the Ollama process or service only with human intent, then
restart it. macOS Ollama.app commonly uses `launchctl`; Linux services and Windows use their own
service configuration. Full setting table, upstream resolution evidence, measured prefix-cache
reuse, and dual-backend trials:
`assets/evidence/ollama-config.md`.

Next: backend decision loop → `references/resource-routing.md`; symptom diagnosis →
`references/troubleshooting.md`.
