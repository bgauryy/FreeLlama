# MCP and CLI: same operations, two doors

Load when connecting FreeLlama, choosing between the MCP server and the shell, or translating a call between them.

## Install and serve

| Door | Install | Notes |
|---|---|---|
| MCP (preferred in an MCP host) | `claude mcp add freellama -- npx -y @octocodeai/freellama-mcp-server`; other hosts: `{"mcpServers":{"freellama":{"command":"npx","args":["-y","@octocodeai/freellama-mcp-server"]}}}` | Starts `freellama serve` on `127.0.0.1:11435` at launch. `delegate_research` needs `-e FREELLAMA_MCP_ALLOWED_ROOTS=/abs/project`; `run_task` `files` uses `FREELLAMA_FILE_ROOTS` (plus server cwd and temp dir). |
| CLI | `npm install -g @octocodeai/freellama` (or prefix `npx -y @octocodeai/freellama`) | One `freellama` command. Agent commands (`run`, `chat`, `gateway`, `workflows`, `job`, `tool`, `tools`) need Node 22+ and run the MCP handlers in-process. |

- Operator commands run in the Rust binary: `doctor`, `init`, `catalog`, `auth-token`, `serve`, `proxy`, `bench-all`, `policy` need no running serve; `models`, `route`, `task`, `jobs`, `warm`, `unload`, `status`, `usage`, `session`, `scope`, `ready`, `config`, `machine` need one. `--endpoint` or `FREELLAMA_SERVE_ENDPOINT` selects it.
- `freellama run` starts serve in the background if nothing answers (log `~/.octocode/freellama/serve.log`); `FREELLAMA_MCP_AUTOSTART_SERVE=0` turns that off.
- `freellama task` has no decoding options; use `freellama run -o k=v`, `--think`, `--format`/`--schema`, or MCP.
- For authenticated serve, set `FREELLAMA_AUTH_TOKEN_FILE` to the same mode-0600 file used at startup.
- If the CLI and server disagree on a flag, align versions or restart serve before trusting either.

## `freellama run` essentials

```bash
freellama run -w summarize-log -f app.log                         # workflow: FreeLlama picks model + prompt
freellama run -w fix-bug -f src/parser.ts "drops the last token"
freellama run -j find-code -f 'src/**/*.ts' --top 3 "where are flags parsed"
freellama run --ask "Is a refund requested?" < ticket.txt          # decision model, probabilities
echo "Invoice 2291, Acme, 1,240.50 EUR" | freellama run -w extract-json --schema "invoice_id, vendor, total float, currency"
freellama run -w review-code --each -f 'src/*.ts'                  # one answer per file
freellama run -m TAG "prompt" --think=false                        # an exact installed tag
```

- stdout is the answer; stderr carries `check:` lines and notes. `--verbose` names the model that actually ran; `--json` prints the full receipt. `--prefer cpu|gpu` chooses among eligible backends only.
- Exit codes: `0` answered (still read `check:`), `75` busy, short of memory or Ollama down (retry), `1` refused, failed, empty, or a tool call nobody asked for, `2` usage error, `130` interrupted.
- A terminal streams tokens; a pipe gets one managed answer with its receipt. `--stream`/`--no-stream` force either.
- Attach images with `--image PATH` or `-f PATH`, or name the path anywhere in the prompt (`"What is the total in ./receipt.png?"`), as with `ollama run`.
- `--defer` returns a job id; `freellama job ID --wait` prints the answer, `--cancel` stops it. `--preview` routes without running.
- Usage errors name the fix (unknown tag suggests installed ones; FreeLlama never pulls).

## Tool ↔ command map

| MCP call | CLI |
|---|---|
| `run_task {workflow\|job, files, prompt}` | `freellama run -w NAME\|-j NAME -f FILE "PROMPT"` |
| `run_task {model, prompt, think, format, options}` | `freellama run -m TAG "PROMPT" --think=false --schema … -o k=v` |
| `run_task {task:"decision", state, questions}` | `freellama run --ask Q [--choice k=desc …\|--level L …] < STATE` |
| `run_task {task:"embedding", files, nearest}` | `freellama run -j find-code\|duplicates\|image-search -f GLOB --top N "QUERY"` |
| `run_task {tasks:[{id, independent:true, task:{files:[p]}}]}` | `freellama run … --each -f GLOB` |
| `run_task {preview:true, …routing only}` | `freellama run … --preview` or `freellama route --task T` |
| `run_task {defer:true}`, `task_jobs {action:"get"\|"cancel", jobId}` | `--defer`, `freellama job ID --wait\|--cancel`, `freellama jobs` |
| `models {view:"catalog", job}` | `freellama catalog` |
| `models {view:"installed"\|"resident"}` | `freellama models` (each entry has `resident`) |
| `doctor`, `doctor {view:"status"\|"usage"}` | `freellama doctor`, `freellama status` (JSON when piped or with `--json`), `freellama usage --days N` |
| `doctor` readiness | `freellama ready` (exit 0 only when serve can run work) |
| `ollama_manage {action:"warm"\|"unload", model, contextTokens}` | `freellama warm --model TAG --context-tokens N`, `freellama unload --model TAG` |
| `session {action:"create"}`, `scope {…}` | `freellama session`, `freellama scope create\|get\|fork\|delete` |
| `read_response {file, section}` | read the `response_file` JSON directly |
| `audio`, `sweep`, `delegate_research`, `ollama_pull`, `ollama_delete`, `models {view:"library"\|"detail"}` | `freellama tool NAME 'JSON'` (or `@file.json`, or `-` for stdin); `freellama tool NAME --help` shows its schema |

`freellama tool` adds a door, not authority: pull and delete approvals and allowed roots still apply.

## Gateway: workflows as models for other clients

`freellama gateway` (loopback `127.0.0.1:11437`) serves `freellama/<workflow>`, `freellama/<job>` (`coding`, `summarize`, `ocr`, `image-recognition`) and `freellama/auto` over Ollama (`/api/chat`, `/api/generate`), OpenAI (`/v1/chat/completions`, `/v1/responses`) and Anthropic (`/v1/messages`). Other tags pass through to serve. `freellama chat NAME` runs the real `ollama` chat through a gateway that lives as long as the chat.

- Non-loopback Origin or Host gets 403; no files are read and no URLs fetched over HTTP (images as base64).
- Turn thinking off per protocol: `think:false` (Ollama, Responses), `reasoning_effort:"none"` (OpenAI), `thinking:{type:"disabled"}` (Anthropic). A small `max_tokens` with thinking on can return empty content.
- Every virtual-model response reports the virtual name in `model` and carries a `freellama` receipt (real `model`, `checks`, `job`) on its final event: the NDJSON done line, the last OpenAI chunk, Responses `response.completed`, Anthropic `message_stop`. Read the checks there.
- Local models are 2–9B: good for bounded requests, poor for long agent loops.

Full reference: [agent CLI and gateway](https://github.com/bgauryy/FreeLlama/blob/main/docs/AGENT_CLI.md), [every API](https://github.com/bgauryy/FreeLlama/blob/main/docs/API.md).
