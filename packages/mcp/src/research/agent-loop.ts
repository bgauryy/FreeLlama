// The research adapter chat loop shared by the restricted-shell and octocode adapters. An adapter
// supplies its system prompt, action parser, tool runner and labels; this module owns the
// subprocess contract: environment, context fitting, transport retries, parse repair, paging,
// repeat suppression, forced finalization, and the result.json document.
import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";
import { setTimeout as sleep } from "node:timers/promises";
import { PARSE_REPAIR_NOTICE, REPEAT_NOTICE, callSignature, type Action } from "./actions.js";
import type { AdapterHooks, AdapterSession, AdapterSpec } from "./adapter.js";
import { canonicalJson, isRecord } from "./canonical-json.js";
import { AgentContextManager, writeFailureResult } from "./context-manager.js";
import { AgentRuntimeConfig, type Env, type Message } from "./context-policy.js";
import { ToolExitError, ValidationError, isToolError } from "./errors.js";
import { ObservationStore } from "./observations.js";
import { progressEmitter } from "./progress-events.js";
import { cpSlice, decodeUtf8, normalizeNewlines, trimSpace } from "./text.js";
import {
  PromptCacheUsage, chatErrorDetails, chatRequest, errorDiagnostic, isTransportError, readCatalogJson,
  requestJson, resolveModelIdentity, resolvePlatformTokenRatio, retryableChatError, unwrapChatResponse, type ModelIdentity,
} from "./transport.js";
import { requestJsonAsJob } from "./transport-jobs.js";
import { resolvePath } from "./workspace-path.js";
import { evidencePaths, groundingNote, groundingRepairNotice, missingPaths, pathsIn, sourceRepairNotice, toolErrorHint } from "./grounding.js";

export type { Action, AdapterHooks, AdapterSession, AdapterSpec, ToolCall } from "./adapter.js";

const isValidationError = (error: unknown): error is ValidationError => error instanceof ValidationError;

function requireEnv(env: Env, name: string): string {
  const value = env[name];
  if (value === undefined) throw new Error(`${name} is not set`);
  return value;
}

const readText = (file: string): string => normalizeNewlines(decodeUtf8(readFileSync(file), file));

/** The empty path names the current directory. */
const pathOf = (value: string) => value === "" ? "." : value;

/** A numeric response counter; a missing or non-numeric value counts as `fallback`. */
const numberOr = (value: unknown, fallback: number): number =>
  typeof value === "number" && Number.isFinite(value) ? value : typeof value === "boolean" ? Number(value) : fallback;

/** Model-facing echo of an action, byte-stable across runs. */
const echo = (action: Action) => canonicalJson(action, { sortKeys: false, ensureAscii: false });

/** Run one adapter process to completion and return its exit code. */
export async function runAdapter(spec: AdapterSpec, hooks: AdapterHooks = {}): Promise<number> {
  const env = hooks.env ?? process.env;
  let resultPath: string | null = null;
  try {
    const model = env.FREELLAMA_TARGET_MODEL || requireEnv(env, "FREELLAMA_BENCH_MODEL");
    const workspace = resolvePath(pathOf(requireEnv(env, "FREELLAMA_BENCH_WORKSPACE")));
    const prompt = readText(pathOf(requireEnv(env, "FREELLAMA_BENCH_PROMPT")));
    resultPath = pathOf(requireEnv(env, "FREELLAMA_AGENT_RESULT"));
    return await runLoop(spec, hooks, env, { model, workspace, prompt, resultPath });
  } catch (error) {
    // The MCP contract needs a result document for every non-zero exit, so an unexpected failure
    // still writes one when its path is known.
    const answer = `unexpected adapter error: ${errorDiagnostic(error)}`;
    process.stderr.write(`${error instanceof Error ? error.stack ?? answer : answer}\n`);
    if (resultPath !== null) {
      try {
        writeFailureResult(resultPath, answer);
      } catch {
        // The failure is already reported on stderr and by the exit code.
      }
    }
    return 1;
  }
}

interface RunInputs {
  model: string;
  workspace: string;
  prompt: string;
  resultPath: string;
}

async function runLoop(spec: AdapterSpec, hooks: AdapterHooks, env: Env, inputs: RunInputs): Promise<number> {
  const { model, workspace, prompt, resultPath } = inputs;
  const print = hooks.print ?? ((line: string) => process.stdout.write(`${line}\n`));
  const progress = hooks.progress ?? progressEmitter(env);
  const endpoint = (env.FREELLAMA_OLLAMA_ENDPOINT ?? "http://127.0.0.1:11434").replace(/\/+$/, "");
  const managedEndpoint = (env.FREELLAMA_AGENT_MANAGED_ENDPOINT ?? "").replace(/\/+$/, "");
  // The turn (or the final answer, without `turn`) a managed job's phases are reported under.
  let phaseAt: { turn?: number; maxTurns: number } = { maxTurns: 0 };
  // Every managed turn has a job identity for timeout and outer-process cleanup. Progress is
  // only a view of that lifecycle; omitting it must not change cancellation behavior.
  const sendJson = hooks.requestJson ?? (managedEndpoint
    ? (url: string, payload: Record<string, unknown>, timeoutSeconds: number) => requestJsonAsJob(url, payload, timeoutSeconds, {
      report: (message) => progress?.({ event: "job_phase", ...phaseAt, message }),
      onJob: (id, open) => progress?.({ event: open ? "job_open" : "job_closed", id }),
      request: hooks.httpRequest,
    })
    : requestJson);
  const executionPreference = env.FREELLAMA_AGENT_EXECUTION_PREFERENCE ?? "auto";
  const minPlacementEvidence = env.FREELLAMA_AGENT_MIN_PLACEMENT_EVIDENCE ?? "configured";
  let runtime: AgentRuntimeConfig;
  try {
    runtime = AgentRuntimeConfig.fromEnv({ defaultToolTimeoutSeconds: spec.defaultToolTimeoutSeconds, env });
  } catch (error) {
    if (!isValidationError(error)) throw error;
    const answer = `invalid research adapter configuration: ${error.message}`;
    writeFailureResult(resultPath, answer);
    print(answer);
    return 1;
  }
  let messages: Message[] = [
    { role: "system", content: spec.systemPrompt(workspace) },
    { role: "user", content: withBriefing(prompt, spec, workspace) },
  ];
  const calls: Record<string, unknown>[] = [];
  const usage: Record<string, unknown> = { input_tokens: 0, output_tokens: 0, cache_read_tokens: null, cache_write_tokens: null };
  const cacheUsage = new PromptCacheUsage();
  const metrics = { load_ms: 0, prompt_eval_ms: 0, eval_ms: 0 };
  const executionReceipts: Record<string, unknown>[] = [];
  let answer = "";
  let failure: string | null = null;
  let transportError: Record<string, unknown> | null = null;
  const readCatalog = (url: string) => readCatalogJson(url, hooks.httpRequest);
  const platformRatio = managedEndpoint
    ? await (hooks.resolvePlatformTokenRatio ?? resolvePlatformTokenRatio)(managedEndpoint, model, { readJson: readCatalog })
    : null;
  const calibrationDir = trimSpace(env.FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR ?? "");
  const calibrationModelIdentity: ModelIdentity = calibrationDir
    ? await (hooks.resolveModelIdentity ?? resolveModelIdentity)(
      managedEndpoint || endpoint, model, messages[0]!.content as string, { readJson: readCatalog },
    )
    : { verified: false, scope: "persistence_disabled", identity: "" };
  const contextManager = new AgentContextManager(runtime, {
    model,
    calibrationIdentity: calibrationModelIdentity.identity,
    calibrationDir: calibrationDir && calibrationModelIdentity.verified ? calibrationDir : null,
    platformRatio,
  });
  try {
    messages = contextManager.fit(messages);
  } catch (error) {
    if (!isValidationError(error)) throw error;
    answer = `research task does not fit the configured context safely: ${error.message}`;
    writeFailureResult(resultPath, answer);
    print(answer);
    return 1;
  }
  const chatOptions = {
    temperature: runtime.temperature,
    seed: runtime.seed,
    num_ctx: runtime.num_ctx,
    num_predict: runtime.num_predict,
  };

  // Retry explicit overload/connection refusals only. Replaying a timeout or an uncertain transport
  // failure can overlap a generation that is still consuming the runner.
  const callModel = async (): Promise<Record<string, unknown>> => {
    let lastError: unknown = null;
    let response: Record<string, unknown> | null = null;
    for (let attempt = 0; attempt < runtime.retry_attempts; attempt += 1) {
      try {
        const transportEndpoint = managedEndpoint ? `${managedEndpoint}/_freellama/v1/tasks` : endpoint;
        const [url, payload] = chatRequest(
          transportEndpoint, model, messages, chatOptions, runtime.think, runtime.keep_alive,
          executionPreference, minPlacementEvidence,
        );
        const recorded = executionReceipts.length;
        response = unwrapChatResponse(await sendJson(url, payload, runtime.request_timeout_seconds), executionReceipts);
        if (managedEndpoint && executionReceipts.length === recorded) {
          executionReceipts.push({ model: null, execution: null, reason: "managed_execution_receipt_missing" });
        }
        break;
      } catch (error) {
        if (!isTransportError(error) || !retryableChatError(error)) throw error;
        lastError = error;
        if (attempt + 1 < runtime.retry_attempts) {
          progress?.({ event: "retry", attempt: attempt + 2, attempts: runtime.retry_attempts });
          await sleep(runtime.retry_backoff_seconds * 1000);
        }
      }
    }
    if (response === null) throw lastError;
    usage.input_tokens = (usage.input_tokens as number) + Math.trunc(numberOr(response.prompt_eval_count, 0));
    cacheUsage.observe(response);
    usage.cache_read_tokens = cacheUsage.tokens;
    usage.output_tokens = (usage.output_tokens as number) + Math.trunc(numberOr(response.eval_count, 0));
    metrics.load_ms += numberOr(response.load_duration, 0) / 1_000_000;
    metrics.prompt_eval_ms += numberOr(response.prompt_eval_duration, 0) / 1_000_000;
    metrics.eval_ms += numberOr(response.eval_duration, 0) / 1_000_000;
    contextManager.observe(messages, response.prompt_eval_count);
    return response;
  };

  const refit = (): boolean => {
    try {
      messages = contextManager.fit(messages);
      return true;
    } catch (error) {
      if (!isValidationError(error)) throw error;
      failure = `research context cannot be fitted safely: ${error.message}`;
      return false;
    }
  };

  const content = (response: Record<string, unknown>): string => {
    const message = response.message ?? {};
    if (!isRecord(message)) throw new TypeError("chat response message must be a JSON object");
    const text = message.content ?? "";
    return typeof text === "string" ? text : JSON.stringify(text);
  };

  const seenCalls = new Map<string, number>();
  const pageSize = runtime.context.observation_page_chars;
  const observations = hooks.observationStore ? hooks.observationStore(pageSize) : new ObservationStore(pageSize);
  let session: AdapterSession | undefined;
  try {
    session = spec.open ? await spec.open(workspace) : undefined;
    let parseFailures = 0;
    let exhausted = true;
    // Repeats answered from earlier steps; a second one means the model is circling, so finalize.
    let repeats = 0;
    let groundingRepaired = false;
    let sourceRepaired = false;
    const seenOutputs: string[] = [];
    for (let turn = 0; turn < runtime.max_turns; turn += 1) {
      const at = { turn: turn + 1, maxTurns: runtime.max_turns };
      phaseAt = at;
      progress?.({ event: "model", ...at });
      // Transport failures are terminal (callModel already retried them). A parse failure is not:
      // tell the model what was wrong and let it correct itself, keeping every tool result so far.
      let response: Record<string, unknown>;
      try {
        response = await callModel();
      } catch (error) {
        if (!isTransportError(error)) throw error;
        let diagnostic: string;
        [diagnostic, transportError] = chatErrorDetails(error);
        failure = `agent response failed: ${diagnostic}`;
        exhausted = false;
        break;
      }
      const raw = content(response);
      let action: Action;
      try {
        action = spec.parseAction(raw);
      } catch (error) {
        if (!isValidationError(error)) throw error;
        parseFailures += 1;
        if (parseFailures > runtime.max_parse_repairs) {
          failure = `agent gave ${parseFailures} unparseable replies: ${errorDiagnostic(error)}`;
          exhausted = false;
          break;
        }
        progress?.({ event: "repair", ...at });
        messages.push({ role: "assistant", content: cpSlice(raw, 0, runtime.parse_repair_echo_chars) });
        messages.push({ role: "user", content: PARSE_REPAIR_NOTICE });
        if (!refit()) {
          exhausted = false;
          break;
        }
        continue;
      }
      parseFailures = 0;
      // Paging re-serves stored output: it costs a turn but never a subprocess.
      if (action.action === "page") {
        const wantStep = Math.trunc(numberOr(action.step, 0));
        const wantPage = Math.trunc(numberOr(action.page, 1));
        progress?.({ event: "action", ...at, action });
        messages.push({ role: "assistant", content: echo(action) });
        messages.push({ role: "user", content: observations.message(wantStep, wantPage) });
        if (!refit()) {
          exhausted = false;
          break;
        }
        continue;
      }
      if (action.action === "finish") {
        const proposed = trimSpace(String(action.answer ?? ""));
        const missing = missingPaths(proposed, workspace);
        // One repair turn when the answer cites a file that is not there; it costs no tool call.
        if (missing.length > 0 && !groundingRepaired) {
          groundingRepaired = true;
          progress?.({ event: "repair", ...at });
          messages.push({ role: "assistant", content: echo(action) });
          messages.push({ role: "user", content: groundingRepairNotice(missing, evidencePaths(seenOutputs, workspace)) });
          if (!refit()) {
            answer = proposed + groundingNote(missing);
            exhausted = false;
            break;
          }
          continue;
        }
        // An answer must say where it comes from: one repair turn when it names no file although
        // the run's own results named some (a "no, not found" answer has no such results).
        const evidence = missing.length === 0 && !sourceRepaired && pathsIn(proposed).length === 0
          ? evidencePaths(seenOutputs, workspace) : [];
        if (evidence.length > 0) {
          sourceRepaired = true;
          progress?.({ event: "repair", ...at });
          messages.push({ role: "assistant", content: echo(action) });
          messages.push({ role: "user", content: sourceRepairNotice(evidence) });
          if (!refit()) {
            answer = proposed;
            exhausted = false;
            break;
          }
          continue;
        }
        progress?.({ event: "finish", ...at });
        answer = missing.length > 0 ? proposed + groundingNote(missing) : proposed;
        exhausted = false;
        break;
      }
      const started = performance.now();
      const call = session!.toolCall(action, workspace, runtime);
      // An exact repeat costs a turn and a second copy of an observation the model has already
      // seen. Answer it from the prior step instead of re-running the tool.
      const signature = callSignature(call.signature);
      let observation: string;
      let status: string;
      let exitCode: number | undefined;
      if (seenCalls.has(signature)) {
        progress?.({ event: "action", ...at, action, repeat: true });
        observation = REPEAT_NOTICE;
        status = "repeat";
        repeats += 1;
      } else {
        progress?.({ event: "action", ...at, action });
        try {
          observation = hooks.runTool ? await hooks.runTool(call.record.arguments as Record<string, unknown>) : await call.run();
          status = "ok";
        } catch (error) {
          if (!isToolError(error)) throw error;
          status = error instanceof ToolExitError ? error.status : "error";
          exitCode = error instanceof ToolExitError ? error.exitCode : undefined;
          observation = `${status === "empty" ? "tool search" : "tool error"}: ${error.message}${toolErrorHint(error.message)}`;
        }
        seenCalls.set(signature, calls.length + 1);
      }
      calls.push({
        ...call.record,
        status,
        ...(exitCode === undefined ? {} : { exit_code: exitCode }),
        duration_ms: Math.round((performance.now() - started) * 1000) / 1000,
      });
      messages.push({ role: "assistant", content: echo(action) });
      const remaining = runtime.max_turns - calls.length;
      const step = calls.length;
      observations.put(step, observation, call.ledger(status));
      seenOutputs.push(observation);
      messages.push({ role: "user", content: observations.message(step) + spec.remaining(remaining) });
      if (!refit()) {
        exhausted = false;
        break;
      }
      if (repeats >= 2) break;
    }
    if (exhausted) {
      progress?.({ event: "finalize", maxTurns: runtime.max_turns });
      phaseAt = { maxTurns: runtime.max_turns };
      messages.push({ role: "user", content: spec.exhausted });
      if (refit()) {
        try {
          const response = await callModel();
          const finalAction = spec.parseAction(content(response));
          if (finalAction.action !== "finish") throw new ValidationError("forced final response was not finish");
          answer = trimSpace(String(finalAction.answer ?? ""));
          const missing = missingPaths(answer, workspace);
          if (missing.length > 0) answer += groundingNote(missing);
        } catch (error) {
          if (!isTransportError(error) && !isValidationError(error)) throw error;
          let diagnostic: string;
          [diagnostic, transportError] = chatErrorDetails(error);
          failure = `agent exceeded ${runtime.max_turns} turns and finalization failed: ${diagnostic}`;
        }
      }
    }

    if (!answer) answer = failure ?? "agent stopped without a final answer";
    const result = {
      final_answer: answer,
      tool_calls: calls,
      usage,
      provider_metrics: {
        load_ms: metrics.load_ms,
        prompt_eval_ms: metrics.prompt_eval_ms,
        eval_ms: metrics.eval_ms,
      },
      model_metadata: {
        ...spec.adapterMetadata(session),
        temperature: chatOptions.temperature,
        seed: chatOptions.seed,
        num_ctx: chatOptions.num_ctx,
        num_predict: chatOptions.num_predict,
        max_turns: runtime.max_turns,
        runtime_config: runtime.metadata(),
        context_management: contextManager.metadata(),
        context_compactions: contextManager.compactions,
        transport: managedEndpoint ? "managed" : "direct_ollama",
        execution_preference: executionPreference,
        min_placement_evidence: minPlacementEvidence,
        execution_receipts: executionReceipts,
        ...(transportError ? { transport_error: transportError } : {}),
        cache_token_metrics: cacheUsage.metadata(),
        calibration_model_identity: calibrationModelIdentity,
      },
    };
    observations.writeResult(resultPath, result);
    print(answer);
    return failure ? 1 : 0;
  } finally {
    session?.close?.();
    observations.close();
  }
}

/** The question plus the adapter's workspace facts; a failed briefing never fails the run. */
function withBriefing(prompt: string, spec: AdapterSpec, workspace: string): string {
  try {
    const briefing = spec.briefing?.(workspace);
    return briefing ? `${prompt}\n\n${briefing}` : prompt;
  } catch {
    return prompt;
  }
}
