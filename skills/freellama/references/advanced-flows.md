# Advanced flows

Load the section the task needs: grounded research, preview and vision controls, batches and long input, history and warming, sweeps, and keeping local work light.

## Grounded research

`delegate_research {question, workspacePath, model?, adapter?, agent?}` runs one narrow read-only lookup over about 1–5 source files. It needs serve; every model turn is a managed `coding` task.

- Keep `workspacePath` inside `FREELLAMA_MCP_ALLOWED_ROOTS`; never widen it to `$HOME` or `/`.
- Bash is the measured default adapter; Octocode is opt-in for structural or LSP search. An empty LSP result is ambiguous: retry or search, never treat it as "unsupported". Structural search needs `pattern` or `rule`, not `keywords` alone.
- Read `verification`, successful call counts and `citations[]` inline. Context management, execution receipts and every observation the model saw are in `response_file` (`read_response {file, section}`, or `receipt:"full"`). Failed calls do not ground a claim. Retry `isError` once, then do the work yourself; discard an `escalate` result.
- Context budget is `contextTokens - outputTokens - safetyMarginTokens`; older observations compact first and full tool output stays retrievable by page. Token counts are estimates learned from Ollama's real counts (`platform_learned_ratio`), falling back to `charsPerToken`. Keep `FREELLAMA_AGENT_PINNED_OVERFLOW` unset; raising `contextTokens` or narrowing the question is safer than `clip`. Tune per call with `agent`, deployment defaults with `FREELLAMA_AGENT_*`; confinement and read-only actions are not knobs.
- The one writing adapter, `coder`, is operator opt-in (`FREELLAMA_MCP_ALLOW_CODER=1`): it edits and runs allowlisted commands in an isolated worktree copy and returns a patch. You review and apply it; it never writes your tree.

## Preview, vision and runtime controls

- `run_task {preview:true, task?, model?, executionPreference?, minPlacementEvidence?, minConfidence?}` takes routing fields only; it rejects prompts, messages, embedding input, tools, images and runtime controls, and reserves nothing. Review, then send a separate execution call.
- `minPlacementEvidence:"observed"` refuses cold, mixed or mismatched placement. Warm once with `"configured"`, inspect the receipt, then require `"observed"`.
- Images: name them in `files`, or send base64 without a data-URI prefix; they need a trialled vision `model` or an operator `[policies.vision]` model. A capability tag alone is refused.
- `output_check.repetition_loop` means the model looped (one copy shown): low confidence. `stopOnRepetition:true` stops a known looper early and keeps what it generated.
- Choose output limits and stop sequences from the output you need. "Stopped at the output limit" means the answer is cut: raise `num_predict`.

## Batches, long input and deferral

- One task reads all its `files` together: a glob gives one answer. For one answer per file send `tasks:[{id, independent:true, task:{files:[path]}}]` (up to 64), with what the items share (`task`, `model`, `systemPrompt`, `questions`, `tools`, `format`, `options`) once at the top level.
- FreeLlama never splits input; where to cut changes the answer. Split a long document to fit the model's window (`execution.context_sizing` shows the estimate). A prompt past the 32k automatic cap, or over 8 bytes per token of a given `contextTokens`, is refused fast (`context_window_exceeded`). An embedding item over 8 bytes per token of the window is refused with its indices (`embedding_item_exceeds_context`): chunk and embed the chunks. Merge the answers yourself.
- Hosts cut calls near 60 s. For a cold load, long input or a large batch pass `defer:true` (with `timeoutSeconds`, `priority`) and poll `task_jobs {action:"get", jobId}`. `timeoutSeconds` covers discovery, queueing, loading and inference. `cancel` releases the local permit; it does not prove Ollama stopped decoding. Jobs are process-local and lost on restart unless serve runs with `FREELLAMA_DURABLE_JOBS=1`: then finished receipts survive, an unfinished job reads back as an unknown outcome (never replayed), and `idempotencyKey` (with `defer:true`) returns the original job on a resubmission.
- With a `progressToken`, a task reports `queued`, `waiting for memory`, `loading MODEL (~N s)`, `running` every 10 s, and batches report `k/N items done`. A long wait is the machine protecting itself.
- Embeddings: batch `input:[…]`; leave `returnEmbeddings:false` unless code needs vectors.

## History and warming

- `scope {action:"create", messages}` keeps bounded process-local history; `session` stores model affinity only. Pass `scopeId` and `scopeRevision` to `run_task` and use the returned revision. Fork at the expected revision before parallel branches. Read history only with `includeMessages:true`; failures do not append.
- `ollama_manage {action:"warm", model, contextTokens, keepAlive?}` loads an exact installed runner through the same admission and fit checks. Warm at the context the tasks will send, or the first task reloads. Explicit `keepAlive` wins over adaptive retention. [Scopes and warming](https://github.com/bgauryy/FreeLlama/blob/main/docs/SCOPES_AND_WARMING.md).

## Sweep

For a check over a code tree, log or docs site that splits into independent chunks. You choose the questions and verify; the local model reads.

1. `sweep {action:"start", files|urls, model:<classify pick>, questions}`; add `verifyModel` for a stronger second look at flagged or unsure chunks.
2. `sweep {action:"wait", sweepId}` until complete (each call up to 50 s); do other work meanwhile.
3. `sweep {action:"results", sweepId}` lists flagged chunks as `path:Lstart-Lend` or `url#n`. Read 3–5 yourself before relying on the rest.
4. Later, start with `baseline:<sweepId>`: unchanged chunks reuse saved results; `filter:"changed"` shows what moved.

Recipe: `freellama://docs/cookbooks/sweep`.

## Keep local work fast and light

- One model, one context per batch: a different `num_ctx` or a model switch reloads the runner, drops its prompt cache and may evict another model.
- Stable prefix first (system prompt, tools, documents), variable part last; append rather than edit. Warm a long prefix once (`ollama_manage {action:"warm", model, systemPrompt}`) and confirm reuse in `metrics.cached_prompt_tokens`.
- Keep your own context small: use the inline answer and verdict; fetch one `response_file` section with `read_response`. An answer over 8,000 characters returns a summary, its `check:` lines and an `output_file`; `output:"inline"` returns it whole, `outputPath` picks the file. Pass `page.next_cursor` and `next_offset` back unchanged.
- Name inputs by path or glob; a pasted file or base64 screenshot is your own output tokens.
