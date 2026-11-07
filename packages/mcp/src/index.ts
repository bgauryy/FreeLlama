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
import { registerDocsResources } from "./docs-resources.js";
import { registerDoctorTool } from "./tools/doctor.js";
import { registerSessionTool } from "./tools/session.js";
import { registerModelsTool } from "./tools/models.js";
import { registerScopeTool } from "./tools/scope.js";
import { registerRunTaskTool } from "./tools/run-task.js";
import { registerTaskJobsTool } from "./tools/task-jobs.js";
import { registerOllamaLifecycleTools } from "./tools/ollama-lifecycle.js";
import { registerDelegateResearchTool } from "./tools/delegate-research.js";
import { registerReadResponseTool } from "./tools/read-response.js";
import { registerAudioTool } from "./tools/audio.js";

installProcessLifecycle();

const INSTRUCTIONS = `FreeLlama runs bounded work on local Ollama models so you spend fewer of your own tokens. Offload when what you send plus what you read back costs less than doing the work yourself and the answer is cheap to check: 10 or more similar items, or an input over about 2,000 tokens that you would otherwise read whole. Do it yourself: judgment you must stand behind (security review, design, merge decisions), edits across files, or one short item.

Start with models{view:"catalog"} (tested picks per job; cookbooks at freellama://docs/cookbooks/<job>), then models{view:"installed"} and doctor{view:"status"}. Choose by capability, not by name. Pass model from models{view:"catalog", job} for the job: routing without model picks by capability only and may choose one that does not fit now. Keep one model per batch when you can; each extra model is a load and may evict another.

Name inputs with files (paths or globs such as src/**/*.rs, under the server's working directory, the temp directory or FREELLAMA_FILE_ROOTS); pasted text and base64 images are your own output tokens. Put what a batch's items share (task, model, systemPrompt, questions, tools) once at the top level. Warming a model saves local time, not your tokens.

You own prompts, output format and verification: local answers are candidates, so check 3 to 5 (more where a mistake costs more) before relying on the rest, and treat low decision probabilities as unknown.
Memory: a refusal names the programs holding memory; tell the user and never stop their processes, then retry once they free memory, or use a smaller installed model with the same capability.
Approval: before ollama_manage pull, ask the user for one exact tag and its reported size; search results are not permission. Unload only a model you warmed (when its burst ends) or one the user named; delete only a tag the user names.
Results: check isError first; prefer structuredContent. Pass page.next_cursor back unchanged. A server restart loses scopes, sessions and jobs. CPU/GPU placement is operator-configured; executionPreference only chooses among eligible backends. Docs: freellama://docs/index.`;

const server = withToolResults(new McpServer(
  { name: "freellama", version: SERVER_VERSION },
  { instructions: INSTRUCTIONS, capabilities: { logging: {} } },
));

registerDocsResources(server);
// Registration order is the tools/list order clients display.
registerDoctorTool(server);
registerSessionTool(server);
registerModelsTool(server);
registerScopeTool(server);
registerRunTaskTool(server);
registerTaskJobsTool(server);
registerOllamaLifecycleTools(server);
registerDelegateResearchTool(server);
registerAudioTool(server);
registerReadResponseTool(server);

const transport = withNormalizedToolSchemas(new StdioServerTransport());
await server.connect(transport);
startServeInBackground();
