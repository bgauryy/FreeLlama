import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import * as native from "./native.js";
import { withServe } from "./serve.js";
import {
  endpointParam, taskParam, objectiveParam, messagesParam, executionPreferenceParam,
  minPlacementEvidenceParam, minConfidenceParam, requiredCapabilitiesParam,
  canonicalTaskKind, objectResultSchema, parsedResult, structuredResult, errorResult,
} from "./helpers.js";

const routeDefaults = z.object({
  task: taskParam.removeDefault().optional(), objective: objectiveParam,
  model: z.string().min(1).optional(), contextTokens: z.number().int().positive().optional(),
  executionPreference: executionPreferenceParam, minPlacementEvidence: minPlacementEvidenceParam,
  minConfidence: minConfidenceParam, requiredCapabilities: requiredCapabilitiesParam,
}).strict();
const limits = z.object({
  maxMessages: z.number().int().positive().optional(), maxBytes: z.number().int().positive().optional(),
  maxEstimatedTokens: z.number().int().positive().optional(), ttlSeconds: z.number().int().positive().optional(),
}).strict();

function scopeBody(messages: unknown, defaults?: z.infer<typeof routeDefaults>, bounds?: z.infer<typeof limits>) {
  return {
    messages,
    route_defaults: defaults === undefined ? undefined : {
      task: defaults.task === undefined ? undefined : canonicalTaskKind(defaults.task),
      objective: defaults.objective, model: defaults.model, context_tokens: defaults.contextTokens,
      execution_preference: defaults.executionPreference, min_placement_evidence: defaults.minPlacementEvidence,
      min_confidence: defaults.minConfidence, required_capabilities: defaults.requiredCapabilities,
    },
    limits: bounds === undefined ? undefined : {
      max_messages: bounds.maxMessages, max_bytes: bounds.maxBytes,
      max_estimated_tokens: bounds.maxEstimatedTokens, ttl_seconds: bounds.ttlSeconds,
    },
  };
}

/** History and residency are separate controls over the same managed task path. */
export function registerContextTools(server: McpServer) {
  const createScope = withServe(native.createScope);
  const getScope = withServe(native.getScope);
  const forkScope = withServe(native.forkScope);
  const deleteScope = withServe(native.deleteScope);
  const warmModel = withServe(native.warmModelRequest);
  server.registerTool("scope", {
    description: "Use when: saved history. Do not use when: KV control. Returns: process-local scope metadata; history is opt-in.",
    inputSchema: z.object({
      action: z.enum(["create", "get", "fork", "delete"]), endpoint: endpointParam,
      scopeId: z.string().uuid().optional(), revision: z.number().int().nonnegative().optional(),
      messages: messagesParam.unwrap().min(0).optional(), routeDefaults: routeDefaults.optional(), limits: limits.optional(),
      includeMessages: z.boolean().optional(),
    }).strict(),
    outputSchema: objectResultSchema, annotations: { destructiveHint: true },
  }, async ({ action, endpoint, scopeId, revision, messages, routeDefaults: defaults, limits: bounds, includeMessages }) => {
    try {
      if ((action === "create") !== (scopeId === undefined)) throw new Error("scopeId is required for get/fork/delete, forbidden for create.");
      if ((action === "fork") !== (revision !== undefined)) throw new Error("revision is required only for fork.");
      if (includeMessages !== undefined && action !== "get") throw new Error("includeMessages is valid only for get.");
      if (messages !== undefined && action !== "create") throw new Error("messages is valid only for create.");
      if ((defaults !== undefined || bounds !== undefined) && action !== "create" && action !== "fork") throw new Error("routeDefaults and limits are valid only for create/fork.");
      if (action === "delete") {
        await deleteScope(endpoint, scopeId!);
        return structuredResult({ deleted: true, scope_id: scopeId });
      }
      return parsedResult(action === "create"
        ? await createScope(endpoint, scopeBody(messages, defaults, bounds))
        : action === "fork"
          ? await forkScope(endpoint, scopeId!, { ...scopeBody(undefined, defaults, bounds), revision })
          : await getScope(endpoint, scopeId!, includeMessages));
    } catch (error) { return errorResult(error); }
  });
  server.registerTool("warm_model", {
    description: "Use when: preload an installed model. Do not use when: generation or pull. Returns: managed residency receipt or job ID.",
    inputSchema: z.object({
      endpoint: endpointParam, model: z.string().min(1), task: taskParam.removeDefault().optional(),
      contextTokens: z.number().int().positive().optional(), executionPreference: executionPreferenceParam,
      minPlacementEvidence: minPlacementEvidenceParam,
      priority: z.enum(["interactive", "normal", "background"]).optional(),
      keepAlive: z.string().min(1).optional().describe("Omitted = adaptive finite TTL."),
      maxWaitSeconds: z.number().int().positive().optional(), timeoutSeconds: z.number().int().positive().optional(),
      defer: z.boolean().optional(),
    }).strict(), outputSchema: objectResultSchema, annotations: { destructiveHint: false },
  }, async ({ endpoint, model, task, contextTokens, executionPreference, minPlacementEvidence, priority, keepAlive, maxWaitSeconds, timeoutSeconds, defer }) => {
    try {
      if (task === "embedding") throw new Error("warm_model supports chat models; embedding warming is unsupported.");
      return parsedResult(await warmModel(endpoint, {
        model, task: task === undefined ? undefined : canonicalTaskKind(task), context_tokens: contextTokens,
        execution_preference: executionPreference, min_placement_evidence: minPlacementEvidence,
        priority, keep_alive: keepAlive, max_wait_seconds: maxWaitSeconds, timeout_seconds: timeoutSeconds, defer,
      }));
    } catch (error) { return errorResult(error); }
  });
}
