# `fllama`: the agent CLI and the virtual-model gateway

`fllama` runs local Ollama models the way `ollama run` does, but every request goes through
FreeLlama. It picks the tested model for the job that is installed and fits now, applies the job's
documented prompts and options, waits for managed admission and memory checks, and returns `check:`
lines that tell you what to verify. The same configuration is also served as **virtual models**
(`freellama/fix-bug`) over the Ollama, OpenAI Chat, OpenAI Responses and Anthropic Messages APIs.
That lets the `ollama` CLI, Continue, opencode, Codex, Claude Code, Open WebUI, LangChain and the
SDKs use it without MCP. [Every API](API.md) shows all of FreeLlama's surfaces on one page. The
design and its trade-offs are in [RFC 0001](rfcs/0001-fllama-agent-cli.md).

`fllama` ships as the second binary of `@octocodeai/freellama-mcp-server` from release 0.11.0.
Until that is published, run it from a checkout:

```bash
npx -p @octocodeai/freellama-mcp-server fllama --help      # 0.11.0 and later
# from a checkout:
yarn install && yarn build && node packages/mcp/dist/agent-cli.js --help
```

## First five minutes

1. `fllama catalog`: the tested pick for each job, and whether it is installed and fits now.
2. `fllama workflows`: what each workflow does and what it expects.
3. Your first task: `git diff | fllama run -w summarize-diff`. The answer goes to stdout.
4. Read stderr. Act on any `check:` line before you rely on the answer. Exit code 1 means the task
   was refused or failed, or the answer came back empty.
5. Optional: `fllama gateway`, then `OLLAMA_HOST=127.0.0.1:11437 ollama run freellama/explain-code`.

## Commands

| Command | What it does |
|---|---|
| `fllama run [MODEL] [PROMPT...]` | One managed task (`run_task`). With no prompt on a terminal it opens a chat |
| `fllama chat [NAME]` | The real `ollama run` REPL on a virtual model (`review-code`, `freellama/auto`) or a tag |
| `fllama gateway [--port 11437]` | Keeps the virtual models available over `/api/*` and `/v1/*` for other clients |
| `fllama workflows` | Workflows and jobs, with what each expects |
| `fllama catalog [JOB]` | Tested picks per job: installed, fits now |
| `fllama job ID [--wait\|--cancel] [--endpoint URL]` | A deferred task's result (`--wait` blocks until it ends and prints only the answer); with no ID, the job list |
| `fllama tool NAME [JSON\|@FILE\|-]` | Any FreeLlama MCP tool, with its own input schema; `fllama tools` lists them. `fllama tool sweep` with `start` or `resume` stays until the sweep ends (a sweep runs in the process that started it) |
| `fllama list\|ls\|ps\|show\|pull\|push\|rm\|cp\|create\|stop\|serve\|signin\|signout\|launch …` | Passed to `ollama` unchanged |

Passthrough means `ollama` semantics: `fllama serve` starts **Ollama** (`ollama serve`), not
`freellama serve`. A pull, `rm` or `create` still needs the user's approval for the exact tag, and
so does `fllama tool ollama_pull` or `ollama_delete`
([agent invariants](../AGENTS.md#agent-facing-invariants)). `fllama tool delegate_research` reads
only workspaces under `FREELLAMA_MCP_ALLOWED_ROOTS`, the same as over MCP.

## `fllama run`

```bash
git diff | fllama run -w summarize-diff
fllama run -w fix-bug -f path/to/parser.ts "The parser drops the last token"
fllama run -j find-code -f 'path/to/src/**/*.ts' --top 3 "where are CLI flags parsed"
fllama run --ask "Is a refund requested?" < ticket.txt
fllama run --ask "Which team?" --choice billing="Payments" --choice technical="Software" < ticket.txt
fllama run "Why is the sky blue?" --verbose            # the chat pick that fits now
fllama run lfm3:1.7b-q4_K_M "Why?" --think=false        # a tag from `fllama catalog`
echo "Invoice 2291, Acme, 1,240.50 EUR" | fllama run -w extract-json --schema "invoice_id, vendor, total float, currency"
fllama run -w explain-code -f path/to/a.ts -x > answer.txt   # the first code block only
```

**Streaming.** On a terminal, tokens stream as they are generated, as in `ollama run`, while the
request still gets the job's pick and prompts, the loaded runner's window and the memory gate for
a cold load. `check:` lines and `--verbose` stats follow the answer. In a pipe, or with
`--no-stream`, one managed answer comes back with its full receipt; `--stream` forces streaming.
Decisions, embeddings, tools, batches and deferred runs always use the managed path.

**Model or prompt.** The first word is a model only when no `--model`, `--job`, `--workflow` or
`--task` is given and it has no whitespace, because an Ollama tag cannot contain any. A word that is
not an installed tag and has nothing after it is the prompt (`fllama run hello`). An unknown tag
before a prompt is a usage error that names the nearest installed tags; `fllama` never pulls. An
embedding model prints the prompt's vector, and an image path written in the prompt is attached,
both as in `ollama run`. So
`fllama run lfm3:1.7b-q4_K_M "Why?"` pins a model, and `fllama run nimble --ask …` asks `nimble`,
as `ollama run` does. A bare prompt such as `fllama run "Why is the sky blue?"` runs on the tested
chat pick that fits now. With no model, `--ask` runs on the `classify` job's tested pick.

**Thinking.** `--think=false` stops a thinking model from spending its output budget before it
answers. A level (`--think=low`, `--think=xhigh`…) must be one the model lists (`ollama show
MODEL`); a model without levels uses its default.

**stdin.** Input piped (`cmd | fllama run`), redirected (`< file`) or passed by a program is read:
as the `state` for a decision, one item per line for an embedding job, otherwise after the prompt.
A socket on stdin (how Node and many agent harnesses wire a child) is read only when data or its
end arrives within 250 ms, so an agent harness's open socket never hangs the call the way it
hangs `ollama run`.

**Ollama flags.** `--format json|SCHEMA`, `--think[=true|false|high|medium|low]`, `--hidethinking`,
`--keepalive`, `--verbose`, `--ask`/`--choice`/`--level`, `--nowordwrap` and `--insecure` keep their
meaning. `--dimensions`, `--truncate=false` and `--voice` change what a managed task can return, so
they are refused with the command that supports them.

**FreeLlama flags.** `-w/--workflow`, `-j/--job`, `-m/--model`, `-t/--task`, `-f/--file` (paths
or globs, repeatable), `--image`, `-s/--system TEXT|@FILE`, `-o/--option k=v`, `--ctx N`,
`--top N`, `--tools JSON|@FILE`, `--objective`, `--prefer cpu|gpu`, `--min-confidence`, `--session`,
`--priority`, `--timeout`, `--max-wait`, `--defer`, `--preview`, `--save-as NAME`, `--json`,
`-q/--quiet`, `--endpoint URL`. `fllama run --help` lists them.

**Structured output and code.**  `--schema` takes a JSON schema (`{…}` or `@file.json`) or llm's
short form, where every field is required: `--schema 'name, age int, tags list: up to five'`
(types `str`, `int`, `float`, `bool`, `list`). `--each` runs the request once per `-f` file (globs expanded, up to 64) as one batch, one `## path`
section per file: `fllama run -w review-code --each -f 'src/*.ts'`. `-x/--extract` prints only the first fenced code
block, or the whole answer with a note when there is none. It extracts text; it does not make a
patch you can `git apply`. `fix-bug` answers in SEARCH/REPLACE blocks for you to apply.

**Output.** stdout carries only the answer. stderr carries `check:` lines (act on them before
relying on the answer), a `handoff_file:` line for `--save-as`, and refusals. `--verbose` adds the
model, the fields job mode filled in, timings and the cached prompt tokens. `--json` prints the
whole receipt. An answer that stopped at the output limit gets a note on stderr. Exit codes: 0 success, 1 refused, failed or empty (a thinking model that spent its
whole budget; rerun with `--think=false`), 2 usage, 130 interrupted. Ctrl-C cancels the running job.
`--ask` answers are named after the question (`Is a refund requested?` → `refund-requested: true
(p 0.89)`).

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
curl http://127.0.0.1:11437/v1/chat/completions -d '{"model":"freellama/review-code",
  "messages":[{"role":"user","content":"def avg(xs): return sum(xs)/len(xs)"}]}'
curl http://127.0.0.1:11437/v1/messages -H 'anthropic-version: 2023-06-01' -d '{"model":"freellama/explain-code",
  "max_tokens":400,"thinking":{"type":"disabled"},"messages":[{"role":"user","content":"What does git rebase --onto do?"}]}'
```

A thinking model spends `max_tokens` on thinking first; a small budget can come back empty with
`finish_reason: "length"`. Turn thinking off (`"thinking":{"type":"disabled"}`,
`"reasoning_effort":"none"`, `"think":false`) or raise the budget. Responses streamed by Ollama
name the real model; FreeLlama's managed responses name the virtual one.

`summarize` and the other jobs run on their catalog job's picks (`fllama catalog summarize` shows
the coding picks it uses). `fllama chat NAME` does the same on an ephemeral port for the length of one REPL, and adds `check:`
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
