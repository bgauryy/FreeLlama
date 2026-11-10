# `fllama`: the agent CLI and the virtual-model gateway

`fllama` runs local Ollama models the way `ollama run` does, but every request goes through
FreeLlama. It picks the tested model for the job that is installed and fits now, applies the job's
documented prompts and options, waits for managed admission and memory checks, and returns `check:`
lines that tell you what to verify. The same configuration is also served as **virtual models**
(`freellama/fix-bug`) over the Ollama, OpenAI Chat, OpenAI Responses and Anthropic Messages APIs.
That lets the `ollama` CLI, Continue, opencode, Codex, Claude Code, Open WebUI, LangChain and the
SDKs use it without MCP. [Every API](API.md) shows all of FreeLlama's surfaces on one page. The
design and its trade-offs are in [RFC 0001](rfcs/0001-fllama-agent-cli.md).

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
| `fllama list\|ls\|ps\|show\|pull\|push\|rm\|cp\|create\|stop\|serve\|signin\|signout\|launch …` | Passed to `ollama` unchanged |

Passthrough means `ollama` semantics: `fllama serve` starts **Ollama** (`ollama serve`), not
`freellama serve`. A pull, `rm` or `create` still needs the user's approval for the exact tag, and
so does `fllama tool ollama_pull` or `ollama_delete`
([agent invariants](../AGENTS.md#agent-facing-invariants)). `fllama tool delegate_research` reads
only workspaces under `FREELLAMA_MCP_ALLOWED_ROOTS`, the same as over MCP.

## `fllama run`

```bash
git diff | fllama run -w summarize-diff
fllama run -w fix-bug -f src/parser.ts "The parser drops the last token"
fllama run -j find-code -f 'src/**/*.ts' --top 3 "where are CLI flags parsed"
fllama run --ask "Is a refund requested?" < ticket.txt
fllama run --ask "Which team?" --choice billing="Payments" --choice technical="Software" < ticket.txt
fllama run gemma4 "Why is the sky blue?" --think=low --verbose
echo "Invoice 2291, Acme, 1,240.50 EUR" | fllama run -w extract-json --schema "invoice_id, vendor, total float, currency"
fllama run -w fix-bug -f src/a.ts "off by one in paging" -x > patch.txt
```

**Model or prompt.** The first word is a model only when no `--model`, `--job`, `--workflow` or
`--task` is given and it has no whitespace, because an Ollama tag cannot contain any. So
`fllama run "Why is the sky blue?"` routes by capability, `fllama run gemma4 "Why?"` pins a model,
and `fllama run nimble --ask …` asks `nimble`, as `ollama run` does. With no model, `--ask` runs on
the `classify` job's tested pick.

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

**Structured output and code.** `--schema` takes a JSON schema (`{…}` or `@file.json`) or llm's
short form, where every field is required: `--schema 'name, age int, tags list: up to five'`
(types `str`, `int`, `float`, `bool`, `list`). `-x/--extract` prints only the first fenced code
block, or the whole answer with a note when there is none.

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

### Protocols

| Endpoint | Clients | Virtual model handling |
|---|---|---|
| `POST /api/chat`, `/api/generate` (Ollama) | `ollama` CLI, Continue, Roo, Aider (`ollama_chat/`), Goose, Zed, Open WebUI, LangChain `ChatOllama` | Streamed by Ollama (its default); `stream:false` runs as a managed task |
| `POST /v1/chat/completions` (OpenAI) | opencode, Cline, LangChain `ChatOpenAI`, Open WebUI, OpenAI SDKs | `stream:true` streamed by Ollama; otherwise a managed task |
| `POST /v1/messages` (Anthropic) | Claude Code (`ANTHROPIC_BASE_URL`), Goose's anthropic engine, Zed | Streamed or not, by Ollama |
| `POST /v1/responses` (OpenAI Responses) | Codex CLI (`wire_api = "responses"`), opencode's Responses runtime | Streamed or not, by Ollama |
| `GET /api/tags`, `/v1/models`, `POST /api/show` | every client's model list | Virtual models first, then the installed tags |
| anything else | | Passed through to `serve` unchanged |

**Streamed requests and the Anthropic and Responses shapes.** The gateway resolves the virtual
model: it takes the job's pick that fits now, the workflow's system prompt, the documented options
and think setting, and the routed context size (`num_ctx` on the native API only; Ollama's OpenAI
layer has no field for it). It rewrites the request with these and forwards it through `serve`'s
passthrough to Ollama's own endpoint. Tokens and tool calls therefore stream exactly as Ollama
streams them, in each protocol's format, with `serve`'s raw admission and telemetry. On the native
API the done chunk gains a `freellama` field (`model`, `checks`, `job`, `num_ctx`), and
`fllama chat` adds the `check:` lines to the answer.

**Non-streaming Ollama and OpenAI chats** run as managed `run_task` calls. They get memory
reservation, eviction, a full receipt (`freellama.cached_prompt_tokens`, `job_profile`) and checks,
and arrive in one chunk.

**What the caller sends wins.** A caller's system message, think or reasoning setting, sampling
values and format are kept; job mode fills only what is missing. On one user turn with no system
message, the workflow may rewrite the turn (OCR prefixes, field lists). OpenAI and Anthropic
sampling map to `temperature`, `top_p` and `top_k`. Thinking off maps to `reasoning_effort: "none"`
for OpenAI, `thinking: {type: "disabled"}` for Anthropic, and `think` for Responses. Images must
be base64; the gateway fetches no URLs and reads no files.

A refusal that waiting can fix, such as no capacity, no memory, no Ollama or no pick that fits now,
is HTTP 503 with the human-readable message; other failures are 500. The gateway listens on
loopback only and refuses browser requests from non-loopback origins.

Local models are small. Pointing Claude Code or Codex at a virtual model works at the protocol
level, but a 2–9B model is weak at long agent loops, and Anthropic does not support routing
Claude Code to non-Claude models. The workflows are built for bounded tasks.
