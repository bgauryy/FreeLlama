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
import { useLogLevelFilter } from "./progress.js";
import { registerDocsResources } from "./docs-resources.js";
import { registerBuiltinPrompts } from "./builtin-prompts.js";
import { TOOL_REGISTRATIONS } from "./tools/registrations.js";

installProcessLifecycle();

const INSTRUCTIONS = `FreeLlama runs bounded work on local Ollama models so you spend fewer of your own tokens. Offload when what you send plus what you read back costs less than doing the work yourself and the answer is cheap to check: 10 or more similar items, or an input over about 2,000 tokens that you would otherwise read whole. Do it yourself: judgment you must stand behind (security review, design, merge decisions), edits across files, or one short item.

Start with models{view:"catalog", job} (tested picks per job, whether each is installed and fits now; cookbooks at freellama://docs/cookbooks/<job>), and pass that pick as model: routing without model picks by capability only and may choose one that does not fit now. Or pass run_task{workflow} (fix-bug, write-tests, review-code, summarize-log, extract-json, read-fields, describe-screen...) or run_task{job}: FreeLlama picks the model and gives it step-by-step instructions for the task (your fields win); MCP prompts show each workflow's call. Call doctor{view:"status"} when a call fails or before a large batch. Choose by capability, not by name. Keep one model per batch when you can; each extra model is a load and may evict another.

Name inputs with files (paths or globs under the server's working directory, the temp directory or FREELLAMA_FILE_ROOTS); pasted text and base64 images are your own output tokens. One task reads all its files together, so a glob gives one answer; for one answer per file send tasks:[{id, independent:true, task:{files:[path]}}] (up to 64), with what the items share (task, model, systemPrompt, questions, tools, format, options) once at the top level. For a large divisible check (code tree, logs, docs site) use sweep: it judges small chunks in the background, saves each result and reports only flagged or changed chunks. Hosts cut calls near 60 s: for a cold load, a long input or a large batch pass defer:true and poll task_jobs{action:"get", jobId}. Warming a model saves local time, not your tokens.

You own prompts, output format and verification: local answers are candidates, so check 3 to 5 (more where a mistake costs more) before relying on the rest, and treat low decision probabilities as unknown.
Memory: a refusal names the programs holding memory; tell the user and never stop their processes, then retry once they free memory, or use a smaller installed model with the same capability.
Approval: before ollama_pull, ask the user for one exact tag (name:tag) and its reported size, and pass that size as approvedSizeBytes; search results are not permission. Unload only a model you warmed (when its burst ends) or one the user named; delete only a tag the user names.
Results: check isError first, then read the text; open structuredContent or read_response{file:response_file, section} only for a receipt field your next step needs. A long answer (run_task, delegate_research, task_jobs get) comes back as a summary plus the output_file holding it; output:"inline" returns it whole, outputPath picks the file. Pass page.next_cursor (models) and next_offset (read_response) back unchanged. A server restart loses scopes, sessions and jobs. CPU/GPU placement is operator-configured; executionPreference only chooses among eligible backends. Docs: freellama://docs/index.`;

const server = withToolResults(new McpServer(
  { name: "freellama", version: SERVER_VERSION },
  { instructions: INSTRUCTIONS, capabilities: { logging: {} } },
));

// Log notices honor the client's logging/setLevel, which the SDK applies only to its own sender.
{
  const core = server.server as unknown as { isMessageIgnored?: (level: string, sessionId?: string) => boolean };
  if (typeof core.isMessageIgnored === "function") {
    const ignored = core.isMessageIgnored.bind(core);
    useLogLevelFilter((level, sessionId) => ignored(level, sessionId));
  }
}

registerDocsResources(server);
registerBuiltinPrompts(server);
// Registration order (tools/registrations.ts) is the tools/list order clients display.
for (const register of TOOL_REGISTRATIONS) register(server);

const transport = withNormalizedToolSchemas(new StdioServerTransport());
await server.connect(transport);
startServeInBackground();
