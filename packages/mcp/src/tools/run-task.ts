// run_task: preview, warm, single, and batch execution through managed serve.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { route, runTaskBatchRequest, runTaskRequest, warmModelRequest } from "../serve-client.js";
import { withBatchTelemetry, withTaskTelemetry } from "../telemetry.js";
import {
  BATCH_TASK_FIELDS, batchItemParam, canonicalTaskKind, endpointParam, errorResult, executionPreferenceParam,
  isStructuredSuccess, localToolsParam, messagesParam, minConfidenceParam, minPlacementEvidenceParam, objectiveParam,
  parsedResult, requiredCapabilitiesParam, structuredResult, summarizeEmbeddings, systemPromptParam, taskMessages,
  taskParam, withRequiredCapability,
} from "../helpers.js";

const taskResultSchema = z.object({
  selected_model: z.string().optional(), context_window_fit: z.string().optional(), execution: z.unknown().optional(), response: z.unknown().optional(), telemetry: z.unknown().optional(),
}).loose();
const batchResultSchema = z.object({
  results: z.unknown().optional(), telemetry: z.unknown().optional(),
}).loose();

type BatchItem = z.infer<typeof batchItemParam>;
type TaskPayload = Pick<BatchItem["task"],
  "task" | "scopeId" | "scopeRevision" | "prompt" | "systemPrompt" | "messages" | "images" | "input" | "tools" |
  "format" | "think" | "logprobs" | "topLogprobs"> & { returnEmbeddings?: boolean };

/**
 * Payload rules shared by single and batch execution. Returns the first problem, prefixed with
 * `label` (the batch item) when given, or null when the payload is executable.
 */
function validateTaskPayload(task: TaskPayload, label?: string): string | null {
  const fail = (message: string) => (label ? `${label}: ${message}` : message);
  if ((task.scopeId === undefined) !== (task.scopeRevision === undefined)) {
    return fail("scopeId and scopeRevision must be supplied together.");
  }
  if (task.topLogprobs !== undefined && task.logprobs !== true) return fail("`topLogprobs` requires `logprobs:true`.");
  if (task.task === "embedding") {
    if (task.input === undefined) return fail('task "embedding" requires `input`.');
    const chatControls = [task.prompt, task.systemPrompt, task.images, task.messages, task.tools, task.format, task.think,
      task.logprobs, task.topLogprobs];
    if (chatControls.some((value) => value !== undefined)) {
      return fail('task "embedding" accepts `input`, `options`, and routing fields; chat controls are not valid.');
    }
    return null;
  }
  if (task.input !== undefined || task.returnEmbeddings !== undefined) {
    return fail('`input` and `returnEmbeddings` are valid only for task "embedding".');
  }
  if (task.prompt === undefined && task.systemPrompt === undefined && (task.messages === undefined || task.messages.length === 0)) {
    return fail(`task "${task.task ?? "completion"}" requires \`prompt\`, \`systemPrompt\`, or \`messages\`.`);
  }
  if (task.images !== undefined && (task.messages !== undefined || task.prompt === undefined)) {
    return fail("Top-level `images` is valid only with `prompt`; put images inside messages instead.");
  }
  return null;
}

async function runBatch(tasks: BatchItem[], maxParallelism: number | undefined, endpoint: string | undefined) {
  for (const item of tasks) {
    const problem = validateTaskPayload(item.task, `batch item ${item.id}`);
    if (problem) return errorResult(new Error(problem));
  }
  return withBatchTelemetry(parsedResult(await runTaskBatchRequest(endpoint ?? DEFAULT_SERVE_ENDPOINT, {
    tasks: tasks.map(({ id, independent, task }) => ({
      id,
      independent,
      task: {
        task: task.task === undefined
          ? (task.scopeId === undefined ? "completion" : undefined)
          : canonicalTaskKind(task.task),
        objective: task.objective ?? (task.scopeId === undefined ? "balanced" : undefined),
        model: task.model,
        session_id: task.sessionId,
        scope_id: task.scopeId,
        scope_revision: task.scopeRevision,
        context_tokens: task.contextTokens,
        execution_preference: task.executionPreference,
        min_placement_evidence: task.minPlacementEvidence,
        required_capabilities: task.requiredCapabilities,
        priority: task.priority ?? "normal",
        max_wait_seconds: task.maxWaitSeconds,
        timeout_seconds: task.timeoutSeconds,
        stop_on_repetition: task.stopOnRepetition,
        prompt: task.prompt,
        images: task.images,
        messages: taskMessages(task),
        input: task.input,
        tools: task.tools,
        keep_alive: task.keepAlive,
        min_confidence: task.minConfidence,
        request_options: {
          format: task.format,
          think: task.think,
          options: task.options,
          logprobs: task.logprobs,
          top_logprobs: task.topLogprobs,
        },
      },
    })),
    max_parallelism: maxParallelism,
  })));
}

export function registerRunTaskTool(server: McpServer): void {
  server.registerTool(
    "run_task",
    {
      description:
        "Use when: offloading caller-supplied work (chat, small tasks, code, tool calls, OCR/vision, embeddings) to a local model. " +
        "Modes: single (`prompt`/`messages`, or `input` for embedding); `tasks` = up to 64 independent items, ordered results, `maxParallelism` caps dispatch; " +
        "`preview:true` decides only; `warm:true` preloads `model`. " +
        "Images: base64 `images` + the model's own prompt; `stopOnRepetition` ends a looping answer early, keeping its text. " +
        "Do not use when: it needs file access (pass content, or delegate_research) or high-stakes judgment. " +
        "Returns: answer text; receipts in structuredContent.",
      inputSchema: z.object({
        endpoint: endpointParam,
        task: taskParam.removeDefault().optional().describe("caller owns prompts and output format."),
        objective: objectiveParam,
        model: z.string().min(1).optional(),
        sessionId: z.string().uuid().optional(),
        scopeId: z.string().uuid().optional(),
        scopeRevision: z.number().int().nonnegative().optional(),
        contextTokens: z.number().int().positive().optional().describe("Total input + output window (num_ctx)."),
        executionPreference: executionPreferenceParam,
        minPlacementEvidence: minPlacementEvidenceParam,
        requiredCapabilities: requiredCapabilitiesParam,
        prompt: z.string().min(1).optional(),
        systemPrompt: systemPromptParam,
        images: z
          .array(z.string().min(1))
          .min(1)
          .optional()
          .describe("base64, no data-URI prefix; prompt mode only; needs `model` or [policies.vision]"),
        messages: messagesParam.describe(
            "Caller system prompts; no injected task instructions. Overrides prompt; extra fields preserved.",
          ),
        input: z
          .union([z.string().min(1), z.array(z.string().min(1)).min(1)])
          .optional(),
        tools: localToolsParam,
        keepAlive: z.string().min(1).optional().describe('"0" unloads now, "-1" pins; omitted = adaptive finite TTL'),
        format: z
          .union([z.literal("json"), z.record(z.string(), z.unknown())])
          .optional(),
        think: z
          .union([z.boolean(), z.enum(["low", "medium", "high"])])
          .optional(),
        options: z
          .record(z.string(), z.unknown())
          .optional()
          .describe("num_ctx/contextTokens and num_gpu/placement are routing-owned; num_predict caps output."),
        logprobs: z.boolean().optional(),
        topLogprobs: z.number().int().nonnegative().optional(),
        minConfidence: minConfidenceParam,
        priority: z.enum(["interactive", "normal", "background"]).optional(),
        maxWaitSeconds: z.number().int().positive().optional(),
        timeoutSeconds: z.number().int().positive().optional(),
        stopOnRepetition: z
          .boolean()
          .optional()
          .describe("Stream and stop once output repeats itself (a loop to num_predict); keeps all generated text"),
        defer: z.boolean().optional(),
        returnEmbeddings: z.boolean().optional(),
        preview: z
          .boolean()
          .optional()
          .describe(
            "true = routing fields only; rejects payloads and runtime controls; never executes",
          ),
        warm: z
          .boolean()
          .optional()
          .describe("Load exact `model`; no prompt"),
        tasks: z
          .array(batchItemParam)
          .min(1)
          .max(64)
          .optional()
          .describe("Batch mode: independent items; per-item fields go inside each item's `task`"),
        maxParallelism: z.number().int().positive().max(64).optional().describe("Batch mode only"),
      }).strict(),
      outputSchema: taskResultSchema.extend(batchResultSchema.shape),
      annotations: { destructiveHint: false },
    },
    async ({
      endpoint,
      task,
      objective,
      model,
      sessionId,
      scopeId,
      scopeRevision,
      contextTokens,
      executionPreference,
      minPlacementEvidence,
      requiredCapabilities,
      prompt,
      systemPrompt,
      images,
      messages,
      input,
      tools,
      keepAlive,
      format,
      think,
      options,
      logprobs,
      topLogprobs,
      minConfidence,
      priority,
      maxWaitSeconds,
      timeoutSeconds,
      stopOnRepetition,
      defer,
      returnEmbeddings,
      preview,
      warm,
      tasks,
      maxParallelism,
    }) => {
      try {
        const singleFields = {
          task, objective, model, sessionId, scopeId, scopeRevision, contextTokens, executionPreference,
          minPlacementEvidence, requiredCapabilities, prompt, systemPrompt, images, messages, input, tools,
          keepAlive, format, think, options, logprobs, topLogprobs, minConfidence, priority, maxWaitSeconds,
          timeoutSeconds, stopOnRepetition, defer, returnEmbeddings, preview, warm,
        };
        if (tasks !== undefined) {
          const stray = Object.entries(singleFields).filter(([, value]) => value !== undefined).map(([name]) => name);
          const unsupported = stray.filter((name) => !BATCH_TASK_FIELDS.has(name)).map((name) => `\`${name}\``);
          if (unsupported.length > 0) {
            return errorResult(new Error(`${unsupported.join(", ")} ${unsupported.length === 1 ? "is" : "are"} not supported with \`tasks\`.`));
          }
          const movable = stray.map((name) => `\`${name}\``);
          if (movable.length > 0) {
            return errorResult(new Error(`With \`tasks\`, put ${movable.join(", ")} inside each item's \`task\`.`));
          }
          return await runBatch(tasks, maxParallelism, endpoint);
        }
        if (maxParallelism !== undefined) return errorResult(new Error("`maxParallelism` is valid only with `tasks`."));
        if (warm) {
          const allowed = new Set(["warm", "model", "task", "contextTokens", "executionPreference", "minPlacementEvidence",
            "priority", "keepAlive", "maxWaitSeconds", "timeoutSeconds", "defer"]);
          const stray = Object.entries(singleFields)
            .filter(([name, value]) => value !== undefined && !allowed.has(name))
            .map(([name]) => `\`${name}\``);
          if (stray.length > 0) return errorResult(new Error(`\`warm:true\` loads a model without generation; remove ${stray.join(", ")}.`));
          if (model === undefined) return errorResult(new Error("`warm:true` requires an exact installed `model`."));
          if (task === "embedding") return errorResult(new Error("`warm:true` supports chat models; embedding warming is unsupported."));
          return parsedResult(await warmModelRequest(endpoint, {
            model, task: task === undefined ? undefined : canonicalTaskKind(task), context_tokens: contextTokens,
            execution_preference: executionPreference, min_placement_evidence: minPlacementEvidence,
            priority, keep_alive: keepAlive, max_wait_seconds: maxWaitSeconds, timeout_seconds: timeoutSeconds, defer,
          }));
        }
        const canonicalTask = canonicalTaskKind(task ?? "completion");
        if (preview) {
          const executionOnlyFields = ([
            ["scopeId", scopeId],
            ["scopeRevision", scopeRevision],
            ["prompt", prompt],
            ["systemPrompt", systemPrompt],
            ["images", images],
            ["messages", messages],
            ["input", input],
            ["tools", tools],
            ["keepAlive", keepAlive],
            ["format", format],
            ["think", think],
            ["options", options],
            ["logprobs", logprobs],
            ["topLogprobs", topLogprobs],
            ["returnEmbeddings", returnEmbeddings],
            ["maxWaitSeconds", maxWaitSeconds],
            ["priority", priority],
            ["timeoutSeconds", timeoutSeconds],
            ["stopOnRepetition", stopOnRepetition],
            ["defer", defer],
          ] satisfies Array<[string, unknown]>)
            .filter(([, value]) => value !== undefined)
            .map(([name]) => `\`${name}\``);
          if (executionOnlyFields.length > 0) {
            return errorResult(
              new Error(
                "`preview:true` accepts routing fields only; remove execution-only fields " +
                  `${executionOnlyFields.join(", ")}. Use \`requiredCapabilities:[\"tools\"]\` ` +
                  "to preview tool capability, then make a separate execution call with the payload.",
              ),
            );
          }
        }
        const effectiveRequiredCapabilities =
          tools === undefined
            ? requiredCapabilities
            : withRequiredCapability(requiredCapabilities, "tools");
        // The core enforces minConfidence inside route(), so preview and synchronous execution
        // refuse there, with the core's actionable message, before any generation.
        if (preview) {
          return parsedResult(
            await route(endpoint, canonicalTask, objective, model, sessionId, contextTokens, effectiveRequiredCapabilities, minConfidence, executionPreference, minPlacementEvidence),
          );
        }
        const problem = validateTaskPayload({
          task, scopeId, scopeRevision, prompt, systemPrompt, messages, images, input, tools, format, think, logprobs,
          topLogprobs, returnEmbeddings,
        });
        if (problem) return errorResult(new Error(problem));
        // A deferred job is accepted before it routes, so a floor would otherwise surface only as a
        // failed job. Route first (no generation) so the refusal returns synchronously.
        if (minConfidence && defer && scopeId === undefined) {
          const decision = parsedResult(
            await route(endpoint, canonicalTask, objective, model, sessionId, contextTokens, effectiveRequiredCapabilities, minConfidence, executionPreference, minPlacementEvidence),
          );
          if (!isStructuredSuccess(decision)) return decision;
        }
        const result = parsedResult(
          await runTaskRequest(endpoint ?? DEFAULT_SERVE_ENDPOINT, {
            task: scopeId !== undefined && task === undefined ? undefined : canonicalTask,
            objective: objective ?? (scopeId === undefined ? "balanced" : undefined),
            model,
            session_id: sessionId,
            scope_id: scopeId,
            scope_revision: scopeRevision,
            context_tokens: contextTokens,
            required_capabilities: effectiveRequiredCapabilities,
            prompt,
            images,
            messages: taskMessages({ systemPrompt, messages, prompt, images }),
            input,
            tools,
            keep_alive: keepAlive,
            min_confidence: minConfidence,
            priority: priority ?? "normal",
            max_wait_seconds: maxWaitSeconds,
            timeout_seconds: timeoutSeconds,
            stop_on_repetition: stopOnRepetition,
            defer: defer ?? false,
            execution_preference: executionPreference,
            min_placement_evidence: minPlacementEvidence,
            request_options: {
              format,
              think,
              options,
              logprobs,
              top_logprobs: topLogprobs,
            },
          }),
        );
        if (isStructuredSuccess(result) && result.structuredContent.deferred === true) return result;
        if (!returnEmbeddings && isStructuredSuccess(result)) {
          const trimmed = summarizeEmbeddings(result.structuredContent);
          if (trimmed) return withTaskTelemetry(structuredResult(trimmed));
        }
        return withTaskTelemetry(result);
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
