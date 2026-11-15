# Managed llama.cpp text tasks

FreeLlama can run bounded text tasks on one local, operator-run `llama-server`. Configure the
server in `upstreams.toml`, then explicitly select `name/model` through `freellama task` or the
managed HTTP API. The Rust core owns weighted admission, host memory reservations, total deadlines,
usage accounting and result normalization. llama.cpp owns model loading and physical execution.

Mock HTTP contracts verify admission, deadlines, refusal paths and result normalization. One live
managed completion also passed on 2026-11-14 with llama.cpp 0.8.0, build 11960, commit `8642541b2`,
using an existing 1.1 GB GGUF: preview returned HTTP 200 without inference, then the managed task
returned `hello` with 17 prompt tokens and 4 output tokens. The primary Ollama endpoint was
intentionally unreachable. Both temporary processes exited cleanly. See the
[configuration and raw receipts](assets/llamacpp-managed-smoke-2026-11-14.json).

That single smoke test establishes this basic protocol path on that build. It does not qualify
throughput, task accuracy, physical placement or the external server's cancellation behavior.

## Configure a server

Start a single-model llama-server using a GGUF file you already have. For example, after checking
your installed llama.cpp build's help:

```bash
llama-server --model /absolute/path/model.gguf --alias text-model \
  --host 127.0.0.1 --port 8080 --ctx-size 4096 --parallel 1 --n-gpu-layers 0
```

This example selects CPU execution. FreeLlama does not run this command or download a model.
Keep the server configuration stable while tasks are active. Use a separate endpoint for each
model; router mode and automatic model switching are outside this adapter's supported contract.

Add this to `upstreams.toml` in the FreeLlama home, or pass its path with `serve --upstreams`:

```toml
[[upstream]]
name = "cpp"
url = "http://127.0.0.1:8080/v1"
# api_key_file = "keys/llamacpp.key"

[upstream.llamacpp]
model = "text-model"
processor = "cpu"
context_tokens = 4096
max_output_tokens = 256
memory_bytes = 4294967296
```

Choose `memory_bytes` for your actual model and server configuration. The example's 4 GiB is an
illustration, not a sizing recommendation. It is a conservative reservation for each active
request, covering a possible model load and runtime buffers. FreeLlama takes no residency credit,
so an already loaded model can be counted twice. The operator must choose sufficient headroom;
this reservation does not prove fit or measure llama.cpp allocation. Concurrent requests each
reserve this amount. Host pressure and memory policy apply through the same governor as Ollama.
A request blocked on resources releases its execution units after the initial bounded telemetry
check. It keeps one ticket in the shared bounded queue, then reacquires execution capacity after
reserving memory. Cancellation releases the ticket and any reservation; a full queue refuses new
waiting work.

`processor` selects the existing CPU or GPU admission pool and pressure policy. It never changes
the server's placement. `observed_processor` stays null and physical placement stays unknown.
`min_placement_evidence:"observed"` refuses the request.

The URL must be loopback HTTP(S), end in `/v1`, and contain no URL credentials, query or fragment.
An optional key file follows the existing owner-only secret-file rules. FreeLlama sends only that
upstream key to llama.cpp; it never forwards the caller's FreeLlama token.

## Preview and execute

```bash
freellama serve --upstreams ./upstreams.toml
```

In another terminal:

```bash
freellama route --model cpp/text-model --task completion
freellama task --model cpp/text-model --task completion "Explain this function in one sentence."
```

Equivalent HTTP calls to a default local server:

```bash
curl http://127.0.0.1:11435/_freellama/v1/routes \
  -H 'Content-Type: application/json' \
  -d '{"model":"cpp/text-model","task":"completion"}'

curl http://127.0.0.1:11435/_freellama/v1/tasks \
  -H 'Content-Type: application/json' \
  -d '{"model":"cpp/text-model","task":"completion","prompt":"Say hello.","timeout_seconds":30,"request_options":{"options":{"num_predict":32}}}'
```

Add your bearer token if the FreeLlama listener requires authentication. Preview verifies readiness
and the model listing, returns a capacity snapshot, and reserves nothing. Execution repeats the
observation, acquires the shared weighted pool and memory reservation, and sends one generation
POST. Explicit llama.cpp routes and tasks do not require Ollama to be reachable. The general model
catalog, auto-selection, benchmark qualification and overall `/ready` health remain Ollama-oriented.
Use the explicit route preview to check this engine.

The result retains `response.message.content`, `response.done_reason`, usage token counts and
`execution.engine:"llamacpp"`. `finish_reason:"length"` is retained as `done_reason:"length"`;
callers must treat it as a potentially truncated answer. Repetition is checked after completion.
Failures and uncertain POST outcomes are never automatically replayed.

Deferred jobs and text-only scopes use the core's [job](DEFERRED_JOBS.md) and
[history](SCOPES_AND_WARMING.md) contracts. They do not add engine capabilities: retained messages
must still be text-only and the fixed context restriction still applies. Built-in MCP jobs or
workflows can add Ollama-specific options such as thinking or structured-output controls, so use a
plain explicit text task unless every resulting field is supported. Automatic job selection does
not discover this engine.

## Supported contract and limits

| Surface | Behavior |
|---|---|
| Task kinds | `completion`, `coding`, `long_context`; explicit configured model only |
| Input | Nonempty `prompt`, or text-only `messages` with system/user/assistant roles |
| Output bound | Positive `max_output_tokens` is mandatory; `num_predict` can lower it |
| Sampling | `request_options.options.temperature`, `top_p`, `seed`, `stop` |
| Context | Fixed operator context; an explicit `context_tokens` must match it; exact input token fit is not verified by FreeLlama |
| Deadlines | Shared task deadline and queue cap; cancellation closes the upstream connection and releases core permits; the external server owns physical cancellation |
| Response buffering | Discovery is capped at 64 KiB; generation uses `FREELLAMA_MAX_REQUEST_BYTES` as its response cap too |
| Tool calling, `code_repair`, images, embeddings, decision tasks | Refused before dispatch |
| Streaming, early repetition stop, format/schema, thinking/logprobs controls | Refused before dispatch; output always has a finite token cap |
| Warm/unload, model download/delete, identity pins | Unsupported on the managed adapter; server lifecycle remains operator-owned |
| Placement and confidence | Configured assignment only; observed placement and benchmark confidence requirements refuse |
| Automatic routing and learning | No auto-selection; llama.cpp results do not train Ollama preload/residency/placement facts |

An upstream without `[upstream.llamacpp]` remains OpenAI-compatible passthrough only. Raw `/v1/*`
requests still use that passthrough contract and do not inherit this adapter's managed task limits.

The wire behavior follows the [llama.cpp server reference](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md):
`/v1/health`, `/v1/models`, and nonstreaming `/v1/chat/completions`. The adapter requires a ready
server listing exactly one model with the configured ID; a reported model status must be `loaded`.
