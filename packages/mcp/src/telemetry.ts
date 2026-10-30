// Cost telemetry for managed task and batch results.
import {
  batchAnswerText,
  configuredExternalCost,
  costTelemetry,
  isStructuredSuccess,
  type parsedResult,
  structuredResult,
  taskAnswerText,
} from "./helpers.js";

export const EXTERNAL_COST = configuredExternalCost();

/** Attach accounting after a successful managed response; the Rust layer remains provider-neutral. */
export function withTaskTelemetry(result: ReturnType<typeof parsedResult>) {
  if (!isStructuredSuccess(result)) return result;
  const payload = result.structuredContent as Record<string, unknown>;
  const metrics = payload.metrics as Record<string, unknown> | undefined;
  return structuredResult({
    ...payload,
    telemetry: costTelemetry({
      inputTokens: typeof metrics?.prompt_tokens === "number" ? metrics.prompt_tokens : null,
      outputTokens: typeof metrics?.output_tokens === "number" ? metrics.output_tokens : null,
      totalDurationNs: typeof metrics?.total_duration_ns === "number" ? metrics.total_duration_ns : null,
    }, EXTERNAL_COST),
  }, { text: taskAnswerText(payload) });
}

export function withBatchTelemetry(result: ReturnType<typeof parsedResult>) {
  if (!isStructuredSuccess(result)) return result;
  const payload = result.structuredContent as Record<string, unknown>;
  const rows = Array.isArray(payload.results) ? payload.results : [];
  let inputTokens = 0;
  let outputTokens = 0;
  let complete = 0;
  const results = rows.map((row) => {
    if (!row || typeof row !== "object") return row;
    const item = row as Record<string, unknown>;
    if (item.ok !== true || !item.response || typeof item.response !== "object") return item;
    const response = item.response as Record<string, unknown>;
    const metrics = response.metrics as Record<string, unknown> | undefined;
    const input = typeof metrics?.prompt_tokens === "number" ? metrics.prompt_tokens : null;
    const output = typeof metrics?.output_tokens === "number" ? metrics.output_tokens : null;
    if (input !== null && output !== null) {
      inputTokens += input;
      outputTokens += output;
      complete += 1;
    }
    return { ...item, response: { ...response, telemetry: costTelemetry({ inputTokens: input, outputTokens: output }, EXTERNAL_COST) } };
  });
  return structuredResult({
    ...payload,
    results,
    telemetry: complete === rows.filter((row) => (row as Record<string, unknown>)?.ok === true).length
      ? costTelemetry({ inputTokens, outputTokens }, EXTERNAL_COST)
      : { local: null, externalEquivalent: null, note: "Batch aggregate unavailable because one or more successful items omitted token counts." },
  }, { text: batchAnswerText(payload) });
}
