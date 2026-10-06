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
import { withoutSchemaDialect } from "./schema-dialect.js";
import { registerDocsResources } from "./docs-resources.js";
import { registerDoctorTool } from "./tools/doctor.js";
import { registerSessionTool } from "./tools/session.js";
import { registerModelsTool } from "./tools/models.js";
import { registerScopeTool } from "./tools/scope.js";
import { registerRunTaskTool } from "./tools/run-task.js";
import { registerTaskJobsTool } from "./tools/task-jobs.js";
import { registerOllamaLifecycleTools } from "./tools/ollama-lifecycle.js";
import { registerDelegateResearchTool } from "./tools/delegate-research.js";

installProcessLifecycle();

const INSTRUCTIONS = `models{view:"installed"}; models{view:"resident"}.
run_task preview never executes; code_review aliases coding. requiredCapabilities:["tools"] previews a tool route; omit preview and supply the payload. Caller runs tool calls.
quality objective needs a task policy or explicit model; medium confidence needs policy+benchmark; observed placement needs /api/ps proof.
Scoped run_task needs scopeId+scopeRevision. defer:true: job.id→task_jobs.jobId unchanged. Restart loses scopes/sessions/jobs.
Check isError first; prefer structuredContent, else content[].text. page.next_cursor→cursor unchanged.
caller owns task decomposition/prompts/format/verification; findings are candidates, not accepted defects. operator owns endpoints, exact --cpu-model assignments/lifecycle. Ollama plus the OS/driver run physical CPU/GPU.
ask approval for one exact tag and reported size before ollama_manage pull; search or recommendation is never download permission. Stop/delete need exact-tag approval.
serve :11435; default-endpoint autostart optional. Docs: freellama://docs/index.`;

const server = new McpServer(
  { name: "freellama", version: SERVER_VERSION },
  { instructions: INSTRUCTIONS },
);

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

const transport = withoutSchemaDialect(new StdioServerTransport());
await server.connect(transport);
