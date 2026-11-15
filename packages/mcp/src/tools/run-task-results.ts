// Presentation of completed run_task results; execution and admission stay in run-task.ts.
import { errorResult, taskAnswerText, type BatchItem } from "../helpers.js";
import { summaryOf, writeHandoff } from "../handoffs.js";
import type { JobProfile } from "../job-mode.js";
import type { WorkflowName } from "../local-prompts.js";
import { nearestText } from "../nearest.js";
import { fallbackCheck, resultChecks } from "../result-checks.js";
import { tablesToMarkdown } from "../table-markdown.js";

type Fields = Record<string, unknown>;
const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? value as Fields : undefined;

type Shown = { content?: Array<{ type: string; text?: string }>; structuredContent?: Fields; isError?: boolean };

/**
 * A finished result with `job_profile` (what job mode filled in) and, for `saveAs`, the answer text
 * written as a handoff whose path is `handoff_file`. Errors pass through: there is nothing to hand on.
 */
export function withJob<R>(result: R, profile: JobProfile | undefined, saveAs?: string, dir?: string): R {
  const shown = result as Shown;
  if (shown.isError || !shown.structuredContent || (profile === undefined && saveAs === undefined)) return result;
  const structuredContent: Fields = { ...shown.structuredContent, ...(profile ? { job_profile: profile } : {}) };
  let content = shown.content;
  if (saveAs !== undefined) {
    // The answer as the agent reads it, without its `[model · lifecycle]` label.
    const answer = (content ?? []).map((part) => part.text ?? "").join("\n").replace(/^\[[^\]\n]*\] /, "");
    const model = profile?.model ?? object(structuredContent.route)?.selected_model ?? structuredContent.selected_model;
    const file = writeHandoff(saveAs, {
      source: `From run_task${profile ? ` job ${profile.job}` : ""}${typeof model === "string" ? `, model ${model}` : ""}`,
      summary: summaryOf(answer), result: answer,
    }, dir);
    structuredContent.handoff_file = file;
    content = (content ?? []).map((part, index) => (index === 0 && part.type === "text" ? { ...part, text: `${part.text ?? ""}\nhandoff_file: ${file}` } : part));
  }
  return { ...result, structuredContent, content } as R;
}

export function rankedResult(
  finished: { structuredContent: Record<string, unknown>; content?: unknown },
  withQuery: boolean,
  itemCount: number,
) {
  const ranked = finished.structuredContent.nearest as Parameters<typeof nearestText>[0] | undefined;
  if (!ranked) return errorResult(new Error("serve returned no nearest ranking for the embedding task"));
  const header = withQuery ? `closest of ${itemCount} items to the query:` : `most similar pairs among ${itemCount} items:`;
  return { ...finished, content: [{ type: "text" as const, text: `${header}\n${nearestText(ranked)}` }] };
}


/**
 * Per-item checks for a batch: each item's result is read the way a single call's is, and only
 * items with something specific to check are listed, so 50 confident answers stay one line.
 */
export function withBatchChecks<R>(result: R, job: string | undefined, items: BatchItem[]): R {
  const shown = result as { content?: Array<{ type: string; text?: string }>; structuredContent?: Fields; isError?: boolean };
  const rows = Array.isArray(shown.structuredContent?.results) ? shown.structuredContent.results as unknown[] : [];
  if (shown.isError || rows.length === 0) return result;
  const taskOf = new Map(items.map((item) => [item.id, String(item.task.task ?? "completion")]));
  const toolsOf = new Map(items.map((item) => [item.id, item.task.tools as unknown[] | undefined]));
  const flagged: string[] = [];
  const checks: Record<string, string[]> = {};
  for (const raw of rows) {
    const row = object(raw);
    if (!row || row.ok !== true) continue;
    const id = String(row.id);
    const receipt = object(row.response) ?? {};
    const ordinary = resultChecks({ job, task: taskOf.get(id), answer: taskAnswerText(receipt) ?? "", structured: receipt, tools: toolsOf.get(id) });
    const quiet = ordinary.length === 0 || /^every answer is outside|^every number and quoted name|^all \d+ SEARCH|^\d+ tool call\(s\) match/.test(ordinary[0]!);
    const fallback = fallbackCheck(receipt);
    const lines = [...(fallback ? [fallback] : []), ...(quiet ? [] : ordinary)];
    if (lines.length === 0) continue;
    checks[id] = lines;
    flagged.push(`${id}: ${lines.join("; ")}`);
  }
  if (flagged.length === 0) return result;
  const content = (shown.content ?? []).map((part, index) => (index === 0 && part.type === "text"
    ? { ...part, text: `${part.text ?? ""}\n\ncheck these items first:\n${flagged.slice(0, 20).map((line) => `- ${line}`).join("\n")}${flagged.length > 20 ? `\n- … ${flagged.length - 20} more in structuredContent.checks` : ""}` }
    : part));
  return { ...result, structuredContent: { ...shown.structuredContent, checks }, content } as R;
}

/**
 * A workflow's promised output shape, applied to the shown answer only (structuredContent keeps
 * the model's raw text): read-table promises Markdown, and OCR models answer in OTSL or HTML.
 */
export function asWorkflowOutput<R>(result: R, workflow: WorkflowName | undefined): R {
  const shown = result as { content?: Array<{ type: string; text?: string }>; isError?: boolean };
  if (workflow !== "read-table" || shown.isError || !shown.content) return result;
  // A table starts on its own line, after the `[model] ` label.
  const table = (text: string) => tablesToMarkdown(text).replace(/^(\[[^\]\n]*\] )\|/, "$1\n|");
  return { ...result, content: shown.content.map((part) => (part.type === "text" ? { ...part, text: table(part.text ?? "") } : part)) } as R;
}

/** More `check:` lines, after the result's own checks. */
export function withCheck<R>(result: R, ...extra: Array<string | undefined>): R {
  const shown = result as { content?: Array<{ type: string; text?: string }>; structuredContent?: Record<string, unknown>; isError?: boolean };
  const added = extra.filter((check): check is string => check !== undefined);
  if (added.length === 0 || shown.isError || !shown.structuredContent) return result;
  const checks = [...((shown.structuredContent.checks as string[] | undefined) ?? []), ...added];
  const content = (shown.content ?? []).map((part, index) => (index === 0 && part.type === "text" ? { ...part, text: `${part.text ?? ""}${added.map((check) => `\ncheck: ${check}`).join("")}` } : part));
  return { ...result, structuredContent: { ...shown.structuredContent, checks }, content } as R;
}
