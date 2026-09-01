# Backend preference and feedback

Load when deciding whether an agent should prefer CPU or GPU, sizing FreeLlama admission, or
interpreting an `execution` receipt. Why: placement is adaptive within operator-owned boundaries,
not a free-form model migration system.

## Authority and precedence

The operator assigns exact CPU tags with `--cpu-model`; every other model stays on the primary
GPU-capable backend. `executionPreference` (`auto`, `prefer_cpu`, `prefer_gpu`) can only choose among
models already eligible after capability, context, policy, and installation filters.

`execution.backend` is the configured process. Compatibility field `execution.placement` is the
request; `execution.observation.processor` comes from post-run `/api/ps`. Use
`minPlacementEvidence:"observed"` to fail closed after one `"configured"` warm-up.

The agent owns decomposition/concurrent submission; FreeLlama owns qualification and admission;
Ollama owns runners/queues; the OS/driver schedules devices. A receipt reports requested and
observed placement, not direct control of accelerator kernels.

Explicit `model` and session affinity win. An unavailable or ineligible preference falls back and
returns `execution.preference_satisfied:false`; raw `/api/*` is never rewritten.

## Feedback loop

`auto` records verified resident latency by task/backend/model: decode time per output token for
generation and total time per embedding input token. It needs three samples
on both backends and more than a 10% advantage before steering `fastest` or `balanced`. Cold loads
do not count. `quality`, explicit models, and session-pinned work never follow latency feedback.
Capacity may choose a backend when the other pool is full.

Unknown, mixed, or assignment-mismatched placement is returned but never trains a backend bucket.
Changing the selected model resets that task/backend bucket rather than blending unlike models.
The CLI persists a bounded, versioned, prompt-free feedback snapshot atomically by default. An
unsupported or corrupt snapshot refuses startup; `--ephemeral-feedback` is an explicit disposable
mode. Inspect `/_freellama/v1/health.feedback`, including its persistence receipt, and preview
before a consequential task; never infer readiness from one fast call.
## Resource contract

Primary and CPU backends use independent weighted pools, so a GPU burst cannot consume a CPU
helper's permit. Defaults are primary 2 and CPU 1; costs are embedding 1, chat 2, vision 4 capped to
the pool. These defaults describe one unit of useful work, not the Mac used for measurements. Tune with
`--max-concurrent-tasks` and `--cpu-max-concurrent-tasks`. Ollama still owns within-process decode
parallelism through `OLLAMA_NUM_PARALLEL`, which multiplies KV memory.
Run `doctor` on the target host. `machine.memory_bytes` is total physical RAM on macOS, Linux, and
Windows; `unified_memory_bytes` is non-null only when system and accelerator memory are known to be
shared. On discrete-GPU hosts, use Ollama's `/api/ps` and vendor tools for VRAM rather than deriving
it from host RAM. Device-isolation variables also vary by backend; see the CPU/GPU routing guide.

Next: memory/KV → `references/ollama-config.md`; failures → `references/troubleshooting.md`.
