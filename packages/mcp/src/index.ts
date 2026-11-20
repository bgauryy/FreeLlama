#!/usr/bin/env node
/**
 * MCP server entry: server instructions, tool registration, and the stdio transport. Each tool
 * lives in src/tools/; tool schemas are re-sent every request — keep them short. Measured model
 * guidance lives in docs/MODEL_SELECTION.md, not in tool descriptions.
 *
 * doctor: Ollama directly. models library: ollama.com. run_task / installed and resident lists:
 * need serve (:11435). delegate_research: adapter subprocess + Ollama (or the serve proxy).
 */
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { SERVER_VERSION } from "./native.js";
import { installProcessLifecycle } from "./lifecycle.js";
import { startServeInBackground } from "./serve.js";
import { withNormalizedToolSchemas } from "./schema-dialect.js";
import { withToolResults } from "./tool-results.js";
import { installLogLevelFilter } from "./progress.js";
import { registerDocsResources } from "./docs-resources.js";
import { registerBuiltinPrompts } from "./builtin-prompts.js";
import { TOOL_REGISTRATIONS } from "./tools/registrations.js";

installProcessLifecycle();

const INSTRUCTIONS = `FreeLlama runs bounded work on local Ollama models so you spend fewer of your own tokens. Offload when what you send plus read back costs less than doing it yourself and the answer is cheap to check: 10 or more similar items, or an input over about 2,000 tokens you would otherwise read whole. Do it yourself: judgment you must stand behind (security review, design, merge decisions), edits across files, one short item.

Start with models{view:"catalog", job} (cookbooks: freellama://docs/cookbooks/<job>) and pass its pick as model: without model, routing picks by capability only and may choose one that does not fit now. Or run_task{workflow} (fix-bug, write-tests, review-code, summarize-log, extract-json, read-fields, describe-screen...) or run_task{job}: FreeLlama picks the model and gives it step-by-step instructions (your fields win). Send one of workflow, job or task: a workflow implies its job and task, a conflicting job or task is refused. With no tested pick installed, a workflow or job runs on an installed model with the job's capability, marked untested by a check: line: verify more. Call doctor{view:"status"} when a call fails or before a large batch. Choose by the capabilities models reports, not a tag's name. Keep one model per batch when you can: each extra model is a load and may evict another.

Name inputs with files (paths or globs under the server's working directory, the temp directory or FREELLAMA_FILE_ROOTS); pasted text and base64 images cost your own output tokens. One task reads all its files together, so a glob gives one answer; for one answer per file send tasks:[{id, independent:true, task:{files:[path]}}] (up to 64). Large divisible check (code tree, logs, docs site): sweep. Files related to a text, or duplicate pairs: run_task{task:"embedding", files, nearest:{query}}, not a model call per chunk. Long calls or a large batch: defer:true, then task_jobs{action:"get", jobId} with the job_id (accepted_job_ids for tasks). Warming a model saves local time, not your tokens.

You own prompts, output format and verification: local answers are candidates, so check 3 to 5 (more where a mistake costs more) before relying on the rest, and treat low decision probabilities as unknown (a check: line marks p 0.35 to 0.65). check: lines also flag an answer cut at its output limit (raise options.num_predict) and invalid JSON.
Memory: a refusal names the programs holding memory; tell the user, never stop their processes, retry once they free memory, or use a smaller installed model with the same capability.
Approval: before ollama_pull, ask the user for one exact tag (name:tag) and its reported size, and pass that size as approvedSizeBytes; search results are not permission, and a user naming a tag has not yet approved its size. Unload only a model you warmed (when its burst ends) or one the user named; delete only a tag the user names.
Results: check isError first, then the text: run_task and task_jobs answer with the text, check: lines and a closing response_file: line; read_response{file, section} opens a receipt field you need (receipt:"full" returns it inline). A server restart loses scopes, sessions and jobs; with FREELLAMA_DURABLE_JOBS=1 finished job receipts survive and an unfinished job reports an unknown outcome, never replayed. Docs: freellama://docs/index.`;

const server = withToolResults(new McpServer(
  { name: "freellama", version: SERVER_VERSION },
  { instructions: INSTRUCTIONS, capabilities: { logging: {} } },
));

// Log notices honor the client's logging/setLevel, which the SDK applies only to its own sender.
installLogLevelFilter(server);

registerDocsResources(server);
registerBuiltinPrompts(server);
// Registration order (tools/registrations.ts) is the tools/list order clients display.
for (const register of TOOL_REGISTRATIONS) register(server);

const transport = withNormalizedToolSchemas(new StdioServerTransport());
await server.connect(transport);
startServeInBackground();
