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

const INSTRUCTIONS = `FreeLlama runs bounded work on local Ollama models so you spend fewer of your own tokens. Offload when what you send plus what you read back costs less than doing the work yourself, and the answer is cheap to check. Rule of thumb: offload 10 or more similar items, or an input over about 2,000 tokens that you would otherwise read whole; keep a single short item.

Offload:
- Label, triage, route or yes/no-check many items: run_task {task:"decision", state, questions}, or tasks:[...] for up to 64 items. One short line per answer.
- Find the files or issues that relate to a text, or duplicates: run_task {task:"embedding", files or input, nearest:{query}} (omit query for duplicate pairs). Returns ranked labels, never vectors.
- Read a screenshot, scan or diagram: run_task {task:"vision", files:[path], prompt}.
- Rewrite, summarize or extract across many short texts: one run_task with tasks:[...].
- Where is X / what calls Y in a repository: delegate_research, then verify its citations.
- Turn a recording into text: audio {action:"transcribe", file}; you cannot hear audio yourself.
Do it yourself: judgment you must stand behind (security review, design, merge decisions), edits across files, a task shorter than its instructions, or one item you already understand.

Spend fewer tokens:
- Name inputs with files (paths or globs such as src/**/*.rs, under the server's working directory or the temp directory); pasted text and base64 images are your own output tokens.
- One call with tasks:[...] instead of many calls, with what the items share (task, model, systemPrompt, questions) set once at the top level; each item carries only its own prompt, state or files.
- ollama_manage {action:"warm", model, systemPrompt} before a burst loads the model and evaluates the shared prompt once, so later calls start faster; it saves local time, not your tokens.
- Answers come back compact; large receipts stay in response_file, so read_response only the section you need.
- A multi-turn local chat: scope keeps the history server-side, so each turn sends only the new message.
- You own the prompts, output format and verification: local answers are candidates, so check a few (3 to 5, more where a mistake costs more) before you rely on the rest. Decision answers carry probabilities: treat low ones as unknown.

Not sure which model? models{view:"catalog"} gives tested picks per job, what is installed and fits now, and a cookbook for each at freellama://docs/cookbooks/<job>. Otherwise start with models{view:"installed"} (what can run, by capability: decision, embedding, vision, completion, tools) and doctor{view:"status"} (free memory, queues). Choose by capability, not by name. With tools, the local model returns tool_calls and you run them.
Memory: a refusal names the programs holding memory; tell the user and never stop their processes, then retry once they free memory, or use a smaller installed model with the same capability. When a burst ends, ollama_manage unload the models you warmed.
Approval: before ollama_manage pull, ask the user for one exact tag and its reported size; search results are not permission. Unload only a model you warmed or one the user named; delete only a tag the user names.
Results: check isError first; prefer structuredContent. Pass page.next_cursor back unchanged. defer:true returns job.id for task_jobs. A server restart loses scopes, sessions and jobs. CPU/GPU placement is operator-configured; executionPreference only chooses among eligible backends. Docs: freellama://docs/index.`;

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
