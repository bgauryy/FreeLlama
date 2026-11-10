// Every MCP tool's registration, in tools/list order. The MCP server (index.ts) and the fllama CLI
// (agent-cli/registry.ts) both register from this list, so the two surfaces cannot drift.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { registerAudioTool } from "./audio.js";
import { registerDelegateResearchTool } from "./delegate-research.js";
import { registerDoctorTool } from "./doctor.js";
import { registerModelsTool } from "./models.js";
import { registerOllamaLifecycleTools } from "./ollama-lifecycle.js";
import { registerReadResponseTool } from "./read-response.js";
import { registerRunTaskTool } from "./run-task.js";
import { registerScopeTool } from "./scope.js";
import { registerSessionTool } from "./session.js";
import { registerSweepTool } from "./sweep.js";
import { registerTaskJobsTool } from "./task-jobs.js";

export const TOOL_REGISTRATIONS: ReadonlyArray<(server: McpServer) => void> = [
  registerDoctorTool,
  registerSessionTool,
  registerModelsTool,
  registerScopeTool,
  registerRunTaskTool,
  registerTaskJobsTool,
  registerOllamaLifecycleTools,
  registerDelegateResearchTool,
  registerAudioTool,
  registerSweepTool,
  registerReadResponseTool,
];
