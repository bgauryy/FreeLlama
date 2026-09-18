# Bounded mixed-workload probe

This probe measures successful correct tasks per second while a local FreeLlama server
handles short extraction, strict JSON, longer archived context, and independent tasks.
The primary score counts every planned task in the quality denominator. Admission refusals,
transport errors, incorrect answers, incomplete responses, and undispatched tasks cannot
improve correctness by disappearing from the report.

The public synthetic cases are development probes. They do not establish general semantic
quality or authorize release promotion. The runner performs no model download, deletion,
server reconfiguration, model judging, or automatic optimization.

## Run

Use an already running FreeLlama control-plane endpoint and an exact installed model tag.
The runner reads health and the managed model catalog before generating. The selected model
must have a digest and must not already be resident: each request uses `keep_alive:"0"`,
which could otherwise unload a model retained by another session. This is a bounded cold-load
probe, including loading/unloading costs, rather than a warm interactive-session benchmark.
Do not run alongside unrelated callers using the same selected model; residency can change
after the initial observation and the benchmark has no exclusive ownership of the backend.

From the repository root, first run a smoke check:

```sh
python3 benchmark/local/scripts/mixed_workload.py run \
  --endpoint http://127.0.0.1:11435 --model EXACT_INSTALLED_TAG \
  --smoke --output .octocode/octocode-eval-benchmark/mixed-smoke.json
```

For repeatability, capture baseline and candidate under the same workload and host conditions:

```sh
python3 benchmark/local/scripts/mixed_workload.py run \
  --endpoint http://127.0.0.1:11435 --model EXACT_INSTALLED_TAG \
  --include-compacted --output .octocode/octocode-eval-benchmark/mixed-baseline.json

# After the independently reviewed subject change, repeat with the same flags:
python3 benchmark/local/scripts/mixed_workload.py run \
  --endpoint http://127.0.0.1:11435 --model EXACT_INSTALLED_TAG \
  --include-compacted --output .octocode/octocode-eval-benchmark/mixed-candidate.json

python3 benchmark/local/scripts/mixed_workload.py compare \
  --baseline .octocode/octocode-eval-benchmark/mixed-baseline.json \
  --candidate .octocode/octocode-eval-benchmark/mixed-candidate.json \
  --output .octocode/octocode-eval-benchmark/mixed-comparison.json
```

An optional `--cpu-model EXACT_INSTALLED_CPU_TAG` assigns only the independent task to a model
already configured on the server's CPU backend. This does not configure or prove physical CPU
placement. Full execution observations are retained for checking that separately. Without it,
all tasks use the primary selected model. Explicit model pins plus the `fastest` route objective
avoid requiring a separate quality-ranking policy; deterministic output grading still decides
task correctness. The objective does not automatically select a different model.

`--include-compacted` adds the long-context case after processing its archived observations
through the existing `agent_context.fit_to_context`. The target record remains a recent message;
the question and output contract stay pinned. Original and compacted variants share the same
execution window (`--context-tokens`, default 4096) and output cap. Compaction targets 2048 tokens using the existing
estimate, not an exact tokenizer. Each task stores original/sent character counts and whether
compaction occurred. Comparisons require this option to match, so within-report variants provide
exploratory evidence rather than an automatic claim about compaction benefits.

## Budget and safety

Defaults are three trials, four tasks per trial (five with compaction), 180 seconds overall,
45 seconds per request, 96 output tokens per task, and at most two concurrent inference requests.
`--context-tokens` accepts 2048 through 32768 and is frozen in the workload configuration;
use the same value for both sides of an experiment. A model whose tokenizer exceeds the
default window can be measured in a new experiment with `--context-tokens 8192`; preserve
the earlier results and rerun both baseline and candidate with that identical budget.
Changing the context budget cannot make an earlier run comparable.
The maximum planned generation is 1152 output tokens by default or 1440 with compaction;
these are request caps, not a claim of measured token usage. No retries are issued by the runner;
the existing server can still have its own bounded retry policy. There is no warm-up inference.
The total elapsed primary denominator includes preflight, polling, loading and unloading.
Limits are bounded by the CLI: five trials, 600 seconds overall, 90 seconds per request,
256 output tokens, and two concurrent requests. One or two trials require `--smoke`;
smoke uses one trial and cannot support a positive comparison verdict.

Before dispatch and once per second during work, health sampling stops new dispatch on holding,
an unhealthy or unavailable control plane, missing required available-memory telemetry, memory
below 2 GiB, any swap-out growth since the initial sample, load above 1.5 per logical CPU, memory pressure,
or observed thermal throttling. Existing requests are allowed to finish within their deadlines.
These conservative guards are measurement safeguards, not empirically optimal runtime defaults.
Sampling can miss events between observations. Missing optional signals remain null/unknown,
and prevent a positive comparison verdict.

Absolute deadlines bound the client controller even if a server stalls. The process cannot prove
upstream cancellation after a client timeout or interruption. Every dispatched managed request
already carries `keep_alive:"0"`, asking the server to observe placement and unload that exact
model when it finishes. The full lifecycle receipt is retained; missing or failed receipts must
not be interpreted as a verified unload. The runner sends no blanket unload command. After a
timeout, inspect server health and residency before another run. It records unfinished work and
stops dispatch on transport failures or an incomplete response whose upstream completion is
unknown. A typed `resource_admission_unavailable` refusal also stops dispatch immediately,
even if the latest cached health sample was healthy. Ordinary queue refusals remain distinct
refusal outcomes and do not alone stop the planned workload. SIGINT during dispatch produces an interrupted receipt.
Outputs use exclusive creation and never overwrite an earlier report.

## Evidence and decisions

Raw JSON includes every request and bounded full response (maximum 1 MiB per response), task
identity and trial, grade and HTTP status, model catalog/digests, health snapshots, wake samples,
budget, fixture/contract hashes, and the runner plus context-fitter source hash. Hashes freeze
the benchmark contract during comparisons. Keep source, fixtures, graders, parameters, models,
and host conditions fixed between baseline and candidate. Do not edit cases to improve a score.

The report contains task latency p50/p95, control-plane health latency p95, local Python process
wake jitter p95, minimum observed available RAM, swap-out growth, load, and thermal observations.
Health latency measures the control plane, not desktop responsiveness. Wake jitter measures this
process's scheduling delay, not an application's event loop, rendering, input latency, or UI.
Neither metric is evidence that the desktop remained responsive. Full semantic quality needs a
separate evaluation. Three trials improve repeatability but do not prove statistical significance;
short-run tail quantiles and timer jitter are particularly noisy.

The comparator rebuilds summaries and deterministic grades from raw evidence, requires complete
case/trial coverage, and rejects mismatched fixtures, contracts, harnesses, model tags/digests,
host identities, or workload budgets. A positive logical CPU count is required to compare the
load-per-CPU guard; missing or zero denominators cannot turn high load into a passing guard.
Endpoint origins may differ to compare two builds on the
same host. Missing telemetry, smoke, partial runs, and malformed metadata yield
`INSUFFICIENT_EVIDENCE`; incompatible inputs yield `NOT_COMPARABLE`. A measured improvement
requires at least 10% higher correct throughput, 100% candidate correctness without regression,
task p95 no worse than 10%, and health/wake p95 no worse than 25%, with resource guards passing.
These thresholds are declared in the frozen KPI contract, not inferred from the results.

`MEASURED_IMPROVEMENT` describes only this probe; it never means `ACCEPT`. An independent,
held-out quality evaluation and actual UI measurements are needed for broader claims. The
comparison reports `NO_MEASURED_IMPROVEMENT` when a comparable result misses these gates.

Validate the runner without live inference:

```sh
python3 benchmark/local/scripts/test_mixed_workload.py
```

Fixture and mock-server tests cover payload limits, context compaction, strict JSON grading,
malformed metadata, partial reports, refusal penalties, pressure stops, deadlines, model identity,
preexisting residency, and comparison guards.
