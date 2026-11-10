# `fllama`: the agent CLI and the virtual-model gateway

`fllama` runs local Ollama models the way `ollama run` does, but every request goes through
FreeLlama. It picks the tested model for the job that is installed and fits now, applies the job's
documented prompts and options, waits for managed admission and memory checks, and returns `check:`
lines that tell you what to verify. The same configuration is also served as **virtual models**
(`freellama/fix-bug`) over the Ollama and OpenAI APIs. That way the `ollama` CLI, Open WebUI,
Continue, LangChain and OpenAI SDKs can use it without MCP. The design and its trade-offs are in
[RFC 0001](rfcs/0001-fllama-agent-cli.md).

`fllama` ships as the second binary of `@octocodeai/freellama-mcp-server`:

```bash
npx -p @octocodeai/freellama-mcp-server fllama --help
# in a checkout, after `yarn build`:
node packages/mcp/dist/agent-cli.js --help
```

## Commands

| Command | What it does |
|---|---|
| `fllama run [MODEL] [PROMPT...]` | One managed task (`run_task`). With no prompt on a terminal it opens a chat |
| `fllama chat [NAME]` | The real `ollama run` REPL on a virtual model (`review-code`, `freellama/auto`) or a tag |
| `fllama gateway [--port 11437]` | Keeps the virtual models available over `/api/*` and `/v1/*` for other clients |
| `fllama workflows` | Workflows and jobs, with what each expects |
| `fllama catalog [JOB]` | Tested picks per job: installed, fits now |
| `fllama job ID [--cancel]` | A deferred task's result; with no ID, the job list |
| `fllama tool NAME [JSON\|@FILE\|-]` | Any FreeLlama MCP tool, with its own input schema; `fllama tools` lists them |
| `fllama list\|ps\|show\|pull\|rm\|cp\|create\|stop\|push\|serve …` | Passed to `ollama` unchanged |

A pull, `rm` or `create` passed to `ollama` still needs the user's approval for the exact tag
([agent invariants](../AGENTS.md#agent-facing-invariants)).

## `fllama run`

```bash
git diff | fllama run -w summarize-diff
fllama run -w fix-bug -f src/parser.ts "The parser drops the last token"
fllama run -j find-code -f 'src/**/*.ts' --top 3 "where are CLI flags parsed"
fllama run --ask "Is a refund requested?" < ticket.txt
fllama run --ask "Which team?" --choice billing="Payments" --choice technical="Software" < ticket.txt
fllama run gemma4 "Why is the sky blue?" --think=low --verbose
```

**Model or prompt.** The first word is a model only when no `--model`, `--job`, `--workflow`,
`--task` or `--ask` is given and it has no whitespace, because an Ollama tag cannot contain any. So
`fllama run "Why is the sky blue?"` routes by capability, and `fllama run gemma4 "Why?"` pins.

**stdin.** Piped (`cmd | fllama run`) or redirected (`< file`) input is read: as the `state` for a
decision, one item per line for an embedding job, otherwise after the prompt. Any other stdin is
ignored, including the open socket many agent harnesses pass, so an agent's call never waits for
input that will not come.

**Ollama flags.** `--format json|SCHEMA`, `--think[=true|false|high|medium|low]`, `--hidethinking`,
`--keepalive`, `--verbose`, `--ask`/`--choice`/`--level`, `--nowordwrap` and `--insecure` keep their
meaning. `--dimensions`, `--truncate=false` and `--voice` change what a managed task can return, so
they are refused with the command that supports them.

**FreeLlama flags.** `-w/--workflow`, `-j/--job`, `-m/--model`, `-t/--task`, `-f/--file` (paths
or globs, repeatable), `--image`, `-s/--system TEXT|@FILE`, `-o/--option k=v`, `--ctx N`,
`--top N`, `--tools JSON|@FILE`, `--objective`, `--prefer cpu|gpu`, `--min-confidence`, `--session`,
`--priority`, `--timeout`, `--max-wait`, `--defer`, `--preview`, `--save-as NAME`, `--json`,
`-q/--quiet`, `--endpoint URL`. `fllama run --help` lists them.

**Output.** stdout carries only the answer. stderr carries `check:` lines (act on them before
relying on the answer), a `handoff_file:` line for `--save-as`, and refusals. `--verbose` adds the
model, the fields job mode filled in, timings and the cached prompt tokens. `--json` prints the
whole receipt. Exit codes: 0 success, 1 refused or failed, 2 usage, 130 interrupted. Ctrl-C cancels
the running job.

**serve.** When nothing answers on the default endpoint, `fllama` starts `freellama serve` in the
background once, logging to `~/.octocode/freellama/serve.log`. It prints the PID and how to stop it,
and later calls reuse it. `FREELLAMA_MCP_AUTOSTART_SERVE=0` turns this off.

## Virtual models and the gateway

| Name | Runs |
|---|---|
| `freellama/<workflow>` | `run_task {workflow}`: `fix-bug`, `review-code`, `explain-code`, `summarize-log`, `extract-json`, `describe-screen`, … |
| `freellama/<job>` | `run_task {job}` for the chat-shaped jobs: `coding`, `summarize`, `ocr`, `image-recognition` |
| `freellama/auto` | Capability routing for a general chat |
| any other tag | Passed through to `serve` unchanged (admission and telemetry apply) |

Decision and embedding jobs need fields a chat cannot carry, so `fllama run` serves them instead.

```bash
fllama gateway                                         # 127.0.0.1:11437
OLLAMA_HOST=127.0.0.1:11437 ollama run freellama/explain-code
OLLAMA_HOST=127.0.0.1:11437 ollama list                # virtual models, then installed tags
curl http://127.0.0.1:11437/v1/chat/completions -d '{"model":"freellama/review-code","messages":[{"role":"user","content":"..."}]}'
```

`fllama chat NAME` does the same on an ephemeral port for the length of one REPL, and adds `check:`
lines to each answer for the person reading it.

How a chat request maps:

- **One user turn with no system message** becomes `run_task {workflow, prompt}`, so job mode
  applies everything it applies over MCP.
- **A longer conversation without a system message** gets the workflow's system prompt prepended
  and runs on the job's pick.
- **A conversation that carries its own system message** is sent as is; only the model, options and
  think setting come from the job.
- **Field mapping.** `options` and `num_ctx`, `format`, `think`, `keep_alive` and `tools` map
  directly. From OpenAI: `max_tokens`/`max_completion_tokens` → `num_predict`, `response_format` →
  `format`, `reasoning_effort` → `think`, and base64 `image_url` parts → images. The gateway fetches
  no URLs and reads no files.

Responses keep each protocol's shape. Ollama answers stream NDJSON, with stats in the done chunk.
OpenAI answers return a `chat.completion`, or SSE with `stream:true`. Both carry a `freellama`
field with the real model, `checks`, `job_profile` and `cached_prompt_tokens`. A managed task is
not streamed token by token, so the answer arrives as one chunk.

A refusal that waiting can fix, such as no capacity, no memory, no Ollama or no pick that fits now,
is HTTP 503 with the human-readable message; other failures are 500. The gateway listens on
loopback only and refuses browser requests from non-loopback origins.
