# Validate FreeLlama on real hardware

Use this procedure to measure how FreeLlama's management layer admits and executes offloaded
tasks on declared hardware. The physical validation matrix requires prepared Ollama and FreeLlama
services with exact installed tags. Synthetic load mode measures gateway lifecycle behavior only.

```bash
python3 benchmark/hardware/run_validation.py \
  --endpoint http://127.0.0.1:11435 \
  --auth-token-file ~/.local/share/freellama/auth.token \
  --gpu-model qwen3.8:27b-mlx \
  --cpu-model nomic-embed-text:latest \
  --output .octocode/hardware/apple-metal.json
```

The runner launches independent coding and embedding requests concurrently, requires verified
physical GPU and CPU receipts, validates admission and response shape, and records host and health
contracts. It first warms both workloads, then measures three matched sequential/parallel pairs
by default, alternating which mode runs first. The same task bodies run in each mode. Every trial
must retain the required placement and correct output, and median sequential wall time divided by
median parallel wall time must meet `--minimum-speedup` (default 1.05). Set `--trials` to increase
the repetition count; qualification requires at least three pairs. The default run sends fourteen
requests before optional vision: two warm-ups plus four requests per pair.

The receipt retains all trials, both median durations and successful task requests per minute.
An overlapping client request's duration includes waiting and cannot serve as an isolated baseline.
Resident GPU bytes prove placement, not GPU busy percentage, bandwidth efficiency or energy use;
device utilization remains explicitly unknown. This short workload gate does not establish
sustained throughput or latency under saturation. Measure those separately with realistic input
lengths, output budgets and external device/thermal telemetry before calling a deployment optimal.

Add `--vision-model`, `--vision-image`, and `--vision-expected-text` to require an exact
normalized OCR transcription rather than accepting any nonempty visual response. Repeat
`--vision-stop` for model-specific repetition guards; pass `--vision-stop '\n'` for a one-line OCR
fixture.

Run this command on each prepared Apple Metal, NVIDIA Linux, AMD Linux, and NVIDIA Windows host
you intend to support. The repository has no hardware-qualification GitHub workflow; its hosted
CI and release builds do not exercise those accelerators. Each host needs Python 3, the services,
exact installed model tags, drivers, and authentication configured. Pass the host's token path
with `--auth-token-file`. A missing host, token, or model is not a pass.

The qualification calculation and refusal to hide failed work have isolated regression checks:

```bash
python3 -m unittest discover -s benchmark/hardware -p test_validation.py
```

Promote a row only when the uploaded JSON has `verdict: "accept"`. Results are machine- and
workload-specific; do not copy one accelerator's receipt into another row.

## Measure a useful GPU workload

Use `run_workload.py` when qualifying a supplied generation or OCR workload, including hosts
without an installed embedding model. This creates a separate GPU workload receipt and leaves
the CPU/GPU promotion gate above unchanged.

Prepare a manifest with at least two distinct inputs and independently checked golden outputs:

```json
{
  "schema_version": 1,
  "model": "exact-installed-vision-tag",
  "task": "vision",
  "trials": 3,
  "concurrency": 2,
  "num_predict": 256,
  "num_ctx": 8192,
  "request_timeout_seconds": 45,
  "run_budget_seconds": 180,
  "keep_alive_seconds": 30,
  "stop": ["```"],
  "cases": [
    {
      "id": "receipt-a",
      "prompt": "Transcribe every visible line in reading order. Return plain text only.",
      "image": "receipt-a.png",
      "expected_text": "GOLDEN TEXT FROM RECEIPT A"
    },
    {
      "id": "receipt-b",
      "prompt": "Transcribe every visible line in reading order. Return plain text only.",
      "image": "receipt-b.png",
      "expected_text": "GOLDEN TEXT FROM RECEIPT B"
    }
  ]
}
```

Image paths resolve relative to the manifest; absolute paths work too. For text workloads, set
`task` to `coding`, `completion`, `code_repair`, or `long_context` and omit `image`. The runner
rejects cloned prompt/image contents, missing goldens, unknown fields, and invalid bounds before
any HTTP request. Golden outputs normalize whitespace but retain case, numbers, and punctuation.
Choose useful cases and representative prompt lengths, output limits and context sizes; repeating
a short task does not establish general quality, saturation throughput or optimal hardware use.

```bash
python3 benchmark/hardware/run_workload.py \
  --endpoint http://127.0.0.1:11435 \
  --auth-token-file ~/.local/share/freellama/auth.token \
  --workload .octocode/hardware/ocr-workload.json \
  --output .octocode/hardware/ocr-receipt.json
```

The runner warms every case once, then measures identical serial and parallel batches, alternating
the order across at least three matched trials. Two cases and three trials schedule at most fourteen
task requests plus one health request. Concurrency caps outstanding requests; transport failure
stops further submissions. Initial health contracts and an explicitly clear host resource hold
are required before any warmup. Each phase must contain every supplied case exactly once, and the
receipt must retain the configured number of alternating matched trials. Every response must
complete, match its golden, select the exact model,
carry a valid admission receipt, and report verified physical GPU placement. Any failed case,
including warmup, rejects the run and removes qualified request/token goodput. Speedup is measured
against the separate serial baseline. `minimum_speedup` is an optional positive manifest field;
the workload mode has no universal gain requirement.

Receipts retain input hashes, goldens, task responses, every trial, and errors. Per-mode summaries
include request wall latency, queue and total admission wait, load, prefill, decode, reported token
counts and qualified goodput. p95 uses nearest rank and includes sample counts; small runs provide
descriptive percentiles. Batch wall time includes client scheduling and checkpoint writes;
individual request latency times HTTP calls. Cached prompt tokens are reported counts, with missing measurements
remaining missing. Cache hit rate, device utilization, and time to first token remain unmeasured.
Inputs intentionally repeat after warmup; comparable cache reuse is part of this workload receipt.

Each managed task receives a finite server deadline. A disposable measurement process lets the
parent enforce the whole-run client budget even when a socket trickles data or a worker hangs.
Atomic checkpoints preserve completed results and identify submitted requests lacking a response.
Deadline expiry yields `reject` with an incomplete receipt. Process termination has up to 0.4 seconds
of grace; final receipt serialization follows measurement termination. Killing the client does not
prove upstream cancellation. The server deadline limits task ownership, and finite model keepalive
expires afterward; explicitly unload the exact model if the operation requires immediate cleanup.

Run all hardware contract suites without services or inference:

```bash
python3 -m unittest discover -s benchmark/hardware -p 'test_*.py'
```

## Compare governed execution with direct Ollama

Use `run_comparison.py` to measure FreeLlama's overhead on the same engine and workload. Prepare
owned loopback services and an exact installed GPU-resident model first. This runner does not
start services, download models, or load a cold model. It requires observed endpoint-attributed
process settings, the same configured GPU upstream, matching installed and resident digests,
explicit context, and an idle gateway before submitting work.

A comparison file references the independently graded workload described above:

```json
{
  "schema_version": 1,
  "workload": "ocr-workload.json",
  "repeats_per_phase": 20,
  "minimum_measured_seconds": 120,
  "maximum_overhead_ratio": 1.15,
  "latency_slo_ms": 500
}
```

Choose these workload-specific limits before inference. The workload supplies trials, concurrency,
context, output limits, transport deadline, and whole-run budget. The comparison rejects more
than 10,000 planned calls, uses a bounded worker process, and retains partial failures. Distinct
cases repeat in a fixed order; repeats increase timing samples, not the number of independent
quality cases.

```bash
python3 benchmark/hardware/run_comparison.py \
  --endpoint http://127.0.0.1:11438 \
  --ollama-endpoint http://127.0.0.1:11439 \
  --auth-token-file ~/.local/share/freellama/auth.token \
  --comparison .octocode/hardware/comparison.json \
  --output .octocode/hardware/comparison-receipt.json
```

Both paths send the same logical chat payload and explicit options. Each gets one full warmup
pass, then at least three alternating direct/managed pairs at the same rolling concurrency.
They share the resident runner and its prefix cache; no reset establishes independent cache
states. Profile reads bracket phases and stay outside measured batch wall time. Direct GPU
evidence comes from phase-boundary `/api/ps` observations; managed requests retain their returned
post-execution placement evidence.

Every attempt must complete on the exact model and match its unchanged golden. The primary ratio
is median managed batch wall divided by median direct batch wall: one means equal duration,
and a ratio above one means gateway overhead. Optional duration and overhead gates reject a
run without hiding its responses. A declared end-to-end latency objective additionally reports
late but correct requests, its pass rate, and latency-qualified goodput. This follows the
[vLLM serving benchmark's separation of latency objectives and concurrency](https://docs.vllm.ai/en/latest/cli/bench/serve/).

The receipt archives logical payload hashes, process/model/runtime identities, every response,
cache-token counts, descriptive latency percentiles and sample counts, and incremental receipt
overhead. Full report serialization happens between phases. Energy, per-task thermal attribution,
cache hit rate, and time to first token remain unmeasured. A closed-loop test does not qualify
open-loop arrival bursts, another model, another accelerator, or a competing engine.


## Check fixed arrival load and queue ownership

Use [run_load.py](run_load.py) with a predeclared workload and load plan.
[load_contract.py](load_contract.py) owns the supported fields and bounds.
Freeze arrival rate, duration, deadlines, outstanding limit, priorities, allowed errors, and cancellation indices before execution.
The runner schedules arrivals independently of task completion and retains every planned item.
Client-cap drops, server refusals, cancellation, transport failures, and incomplete ownership stay visible.

```bash
python3 benchmark/hardware/run_load.py run \
  --plan .octocode/hardware/load-plan.json \
  --endpoint http://127.0.0.1:11438 \
  --upstream http://127.0.0.1:11439 \
  --output .octocode/hardware/load-receipt.json
```

Use `mode: "resident"` only for an exact, already resident, physically verified model.
Use `mode: "controlled"` only with the explicitly synthetic upstream below.
Controlled mode supplies no inference goodput or physical placement qualification.
Its `control_scenario` must match the mock server's required scenario.
`resource_hold` exposes an installed, cold model; `capacity` exposes one synthetic resident.
The default resource policy remains active in both scenarios.
A capacity run rejects an observed resource hold.

```bash
python3 benchmark/hardware/run_load.py mock-upstream \
  --workload .octocode/hardware/load-workload.json \
  --port 11541 --delay-seconds 1 --scenario resource_hold
```

Connect a separately owned gateway to this synthetic endpoint before running its controlled plan.
The mock marker states `nonphysical_synthetic_fixture` and `inference: false`.
Keep its ports, feedback, and usage files separate from real Ollama services.
A controlled cold request uses configured placement intent so admission can assess the cold load.
Resident mode requires observed placement evidence.

Receipts retain raw submission, poll, cancellation, status, and final permit observations.
Latency measures scheduled arrival to observed terminal state, including polling delay.
Priority summaries describe this sample; they do not establish statistical weighted fairness.
There is no open-loop latency SLO gate or time-to-first-token measurement.
A hard worker deadline retains partial ownership; client termination cannot prove upstream cancellation.
Inspect accepted job handles and release owned work before stopping a gateway.

## Observe host and owned services

Use [sample_host.py](sample_host.py) for bounded, read-only host observations.
Repeat `--process PID=/absolute/executable` for each exact operator-owned process.
The sampler checks the executable and process start identity before reporting CPU or RSS.

```bash
python3 benchmark/hardware/sample_host.py \
  --process 12345=/absolute/path/to/freellama \
  --duration-seconds 60 --interval-seconds 1 \
  --command-timeout-seconds 1 \
  --output .octocode/hardware/host-observations.json
```

Observations are sequential and do not form an atomic snapshot.
Process CPU and RSS exclude child processes; RSS is not the physical footprint.
The memory receipt retains compressor occupancy and counter deltas, rather than treating swap occupancy as current swapping.
Apple GPU activity uses the driver's observation window across all processes.
It cannot attribute activity to a task or exclude external clients.

Missing thermal, power, and energy observations remain unknown, with probe failures retained.
The sampler does not elevate permissions or change OS settings.
Power observations, when available, cover host subsystems and do not establish per-task energy.
Command time, output size, and whole-worker bounds preserve partial receipts on failure.
