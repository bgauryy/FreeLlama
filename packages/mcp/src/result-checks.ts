// What to check in one local result, read from the result itself: which decision answers are
// uncertain, whether two code matches are too close to call, which numbers in a summary are not in
// its source, which SEARCH blocks of a coding answer do not match the file. A fixed reminder is the
// fallback only when the result offers no signal.
import { readFileSync } from "node:fs";

type Fields = Record<string, unknown>;
const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? value as Fields : undefined;
const list = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);

/** A decision probability in this band is a coin flip, not an answer. */
const UNSURE = [0.35, 0.65] as const;
/**
 * strands-decider's README: answers at a confidence of 0.9 or more are right about 95% of the time;
 * below that, confirm them. https://raw.githubusercontent.com/strands-labs/strands-decider/main/README.md
 */
const CONFIDENT = 0.9;
/** Two nearest matches closer than this are too close to choose between. */
const CLOSE = 0.02;
/** A best nearest score under this is a weak match. */
const WEAK = 0.45;

/** `tools`: the tool definitions sent with the task, if any. */
export type CheckContext = { job?: string; task?: string; answer: string; structured: Fields; tools?: unknown[] };

function decisionChecks(answers: Fields): string[] {
  const unsure: string[] = [];
  const low: string[] = [];
  for (const [name, raw] of Object.entries(answers)) {
    const answer = object(raw) ?? {};
    const p = answer.type === "noul" && typeof answer.noul === "number" ? answer.noul
      : answer.type === "choice" ? Number(object(answer.probabilities)?.[String(answer.choice)]) : NaN;
    if (Number.isFinite(p) && p >= UNSURE[0] && p <= UNSURE[1]) unsure.push(`${name} (p ${p.toFixed(2)})`);
    else if (typeof answer.confidence === "number" && answer.confidence < CONFIDENT) low.push(`${name} (${answer.confidence.toFixed(2)})`);
  }
  const checks: string[] = [];
  if (unsure.length > 0) checks.push(`uncertain: ${unsure.join(", ")}; decide these yourself or rephrase the question`);
  if (low.length > 0) checks.push(`confidence under ${CONFIDENT}: ${low.join(", ")}; the decider is right about 95% of the time only at ${CONFIDENT} or more, so confirm these`);
  return checks.length > 0 ? checks : ["every answer is outside 0.35-0.65; spot-check a few against the input"];
}

function nearestChecks(nearest: Fields): string[] {
  const rows = nearest.mode === "query" ? list(nearest.matches) : list(nearest.pairs);
  const scores = rows.map((row) => Number(object(row)?.score)).filter(Number.isFinite);
  if (scores.length === 0) return ["nothing reached minScore: rephrase the query or lower minScore"];
  const checks: string[] = [];
  if (scores[0]! < WEAK) checks.push(`best score ${scores[0]!.toFixed(2)} is weak: rephrase with the words the code or text would use`);
  if (scores.length > 1 && scores[0]! - scores[1]! < CLOSE) checks.push(`the top two are within ${CLOSE} (${scores[0]!.toFixed(3)} vs ${scores[1]!.toFixed(3)}): read both`);
  if (nearest.mode !== "query") {
    const likely = scores.filter((score) => score >= 0.9).length;
    if (likely > 0) checks.push(`${likely} pair(s) at 0.90 or more are likely duplicates; read both texts before closing either`);
  }
  return checks.length > 0 ? checks : ["read the top ranges before relying on them"];
}

const readPaths = (structured: Fields): string[] =>
  list(structured.files_read).map((row) => object(row)?.path).filter((value): value is string => typeof value === "string");

function readText(file: string): string | undefined {
  try {
    const bytes = readFileSync(file);
    return bytes.includes(0) ? undefined : bytes.toString("utf8");
  } catch {
    return undefined;
  }
}

/** Numbers, versions, paths and code-quoted names in `answer` that its sources never mention. */
function unsupportedFacts(answer: string, sources: string[]): string[] {
  const haystack = sources.join("\n");
  // File paths and FreeLlama's own bracketed notices are not facts from the source.
  answer = answer.replace(/(?:\/|\.{1,2}\/)?[\w.@-]+(?:\/[\w.@-]+)+/g, " ").replace(/\((?:empty answer|empty response)[^)]*\)|\[freellama:[^\]]*\]/g, " ");
  const facts = new Set([
    ...answer.matchAll(/(?<![\w.])\d[\d,._:/-]*\d%?|(?<![\w.])\d%?(?![\w.])/g),
    ...answer.matchAll(/`([^`\n]{2,60})`/g),
  ].map((match) => (match[1] ?? match[0]).trim()).filter((fact) => fact.length > 0));
  return [...facts].filter((fact) => !haystack.includes(fact) && !haystack.includes(fact.replace(/,/g, "")));
}

type EditBlock = { path: string; search: string };
/**
 * The SEARCH/REPLACE blocks of a coding answer. Models trained on Aider-style edits often put the
 * block in a code fence below the path, or decorate the path (`**a.ts**`, `` `a.ts` ``, `### a.ts:`).
 */
export function editBlocks(answer: string): EditBlock[] {
  const blocks: EditBlock[] = [];
  for (const match of answer.matchAll(/^([^\n<>=]+?)[ \t]*\n(?:[ \t]*`{3,}[\w+.-]*[ \t]*\n)?<{7} SEARCH\n([\s\S]*?)\n={7}\n[\s\S]*?\n>{7} REPLACE/gm)) {
    // Models decorate the path line: `--- path`, `+++ path`, `file: path`, headings, bold, backticks.
    const path = match[1]!.trim().replace(/^(?:#+\s*|[*`]+|-{3}\s*|\+{3}\s*|(?:file|path):\s*)+|[*`:]+$/gi, "").trim();
    blocks.push({ path: path.startsWith("```") ? "" : path, search: match[2]! });
  }
  return blocks;
}

function occurrences(text: string, part: string): number {
  if (!part) return 0;
  let count = 0;
  for (let at = text.indexOf(part); at !== -1; at = text.indexOf(part, at + 1)) count += 1;
  return count;
}

function codingChecks(answer: string, files: string[]): string[] {
  const blocks = editBlocks(answer);
  if (blocks.length === 0) return /```/.test(answer) ? ["no SEARCH/REPLACE blocks: the answer is whole code; compare it with the file before replacing anything"] : [];
  if (files.length === 0) return [`${blocks.length} edit block(s), unchecked: no file was sent with the task`];
  const problems: string[] = [];
  blocks.forEach((block, index) => {
    const file = files.find((candidate) => candidate === block.path || candidate.endsWith(`/${block.path}`));
    const text = file ? readText(file) : undefined;
    if (text === undefined) {
      problems.push(`block ${index + 1} names ${block.path}, which was not sent`);
      return;
    }
    const count = occurrences(text, block.search);
    if (count !== 1) problems.push(`block ${index + 1}'s SEARCH matches ${count} times in ${block.path}: ${count === 0 ? "it is not the file's text" : "it is not unique"}`);
  });
  return problems.length > 0 ? problems : [`all ${blocks.length} SEARCH block(s) match exactly once; apply them, then run the tests`];
}

function visionChecks(context: CheckContext): string[] {
  const text = context.answer.trim();
  const checks: string[] = [];
  if (object(context.structured.output_check)?.repetition_loop === true || /repeated characters omitted/.test(text)) {
    checks.push("the model looped: crop to the region you need and retry");
  }
  if (!text || /^\[?non-?text\]?$/i.test(text) || /\bblank\b/i.test(text)) {
    checks.push("nothing read: check the image is not blank or transparent (convert to an opaque JPEG) and crop to the text");
  } else if (/\bnot visible\b/i.test(text)) {
    // answer-image tells the model to say "not visible" rather than guess.
    checks.push("the model says the image does not show it: crop or enlarge the region, or ask about something visible");
  }
  if (context.job === "ocr" && checks.length === 0) checks.push("check every number and name you will use against the image");
  return checks;
}

/** Whether `value` has the JSON Schema `type` (a name or a list of names); an unknown type passes. */
function hasType(value: unknown, type: unknown): boolean {
  if (Array.isArray(type)) return type.some((one) => hasType(value, one));
  switch (type) {
    case "string": return typeof value === "string";
    case "integer": return Number.isInteger(value);
    case "number": return typeof value === "number";
    case "boolean": return typeof value === "boolean";
    case "array": return Array.isArray(value);
    case "object": return object(value) !== undefined;
    case "null": return value === null;
    default: return true;
  }
}

/**
 * Tool calls checked against the tools that were sent: a known name, arguments that are an object
 * with every required parameter, no undeclared one, and declared types and enums. Small models
 * often call a tool that was not offered, drop a required argument or pass a number as a string.
 */
function toolCallChecks(calls: unknown[], tools: unknown[] | undefined): string[] {
  if (!tools || tools.length === 0) {
    return ["the model answered with a tool call instead of text: rerun with a capable model or a workflow, or send no tools"];
  }
  const schemas = new Map(tools.map((tool) => object(object(tool)?.function)).filter((fn): fn is Fields => fn !== undefined)
    .map((fn) => [String(fn.name), object(fn.parameters) ?? {}]));
  const problems: string[] = [];
  calls.forEach((raw, index) => {
    const fn = object(object(raw)?.function) ?? {};
    const name = String(fn.name ?? "?");
    const at = `call ${index + 1} (${name})`;
    const schema = schemas.get(name);
    if (!schema) {
      problems.push(`${at} is not one of the tools sent (${[...schemas.keys()].join(", ")})`);
      return;
    }
    let args: unknown = fn.arguments ?? {};
    if (typeof args === "string") {
      try { args = JSON.parse(args); } catch { args = undefined; }
    }
    const given = object(args);
    if (!given) {
      problems.push(`${at}: its arguments are not a JSON object`);
      return;
    }
    const properties = object(schema.properties) ?? {};
    const missing = list(schema.required).map(String).filter((key) => !(key in given));
    if (missing.length > 0) problems.push(`${at} lacks required ${missing.join(", ")}`);
    const extra = Object.keys(given).filter((key) => !(key in properties));
    if (extra.length > 0 && Object.keys(properties).length > 0) problems.push(`${at} passes ${extra.join(", ")}, which the tool does not declare`);
    for (const [key, value] of Object.entries(given)) {
      const property = object(properties[key]);
      if (!property) continue;
      if (property.type !== undefined && !hasType(value, property.type)) problems.push(`${at}: ${key} is ${JSON.stringify(value)}, not ${JSON.stringify(property.type)}`);
      else if (Array.isArray(property.enum) && !property.enum.includes(value)) problems.push(`${at}: ${key} is ${JSON.stringify(value)}, not one of ${JSON.stringify(property.enum)}`);
    }
  });
  return problems.length > 0
    ? [...problems, "fix or refuse these calls before running any; a rerun often corrects them"]
    : [`${calls.length} tool call(s) match the tool schemas: run them, then send each result back as {role:"tool", tool_name, content} after the assistant message, with the same tools`];
}

/** The checks for one finished result; empty when there is nothing specific to say. */
export function resultChecks(context: CheckContext): string[] {
  const calls = list(object(object(context.structured.response)?.message)?.tool_calls);
  if (calls.length > 0 || /^\s*tool_calls:/.test(context.answer)) return toolCallChecks(calls, context.tools);
  if (/^\s*\(empty answer/.test(context.answer)) return [];
  const response = object(context.structured.response);
  const answers = object(response?.answers);
  if (answers) return decisionChecks(answers);
  const nearest = object(context.structured.nearest);
  if (nearest) return nearestChecks(nearest);
  const files = readPaths(context.structured);
  if (context.job === "coding" || context.task === "coding" || context.task === "code_repair") {
    const checks = codingChecks(context.answer, files);
    if (checks.length > 0) return checks;
  }
  if (context.task === "vision" || context.job === "ocr" || context.job === "image-recognition") return visionChecks(context);
  const sources = files.map(readText).filter((text): text is string => text !== undefined);
  if (sources.length > 0 && context.answer.trim()) {
    const missing = unsupportedFacts(context.answer, sources);
    return missing.length > 0
      ? [`not in the source: ${missing.slice(0, 8).join(", ")}${missing.length > 8 ? ", …" : ""}; verify these before reusing them`]
      : ["every number and quoted name in the answer appears in the source"];
  }
  return [];
}

/** The result with `checks` in structuredContent and as `check:` lines after the answer. */
export function withChecks<R>(result: R, job: string | undefined, task: string | undefined, tools?: unknown[]): R {
  const shown = result as { content?: Array<{ type: string; text?: string }>; structuredContent?: Fields; isError?: boolean };
  if (shown.isError || !shown.structuredContent) return result;
  const answer = (shown.content ?? []).map((part) => part.text ?? "").join("\n").replace(/^\[[^\]\n]*\] /, "");
  const checks = resultChecks({ job, task, answer, structured: shown.structuredContent, tools });
  if (checks.length === 0) return result;
  const content = (shown.content ?? []).map((part, index) => (index === 0 && part.type === "text"
    ? { ...part, text: `${part.text ?? ""}\n${checks.map((check) => `check: ${check}`).join("\n")}` }
    : part));
  return { ...result, structuredContent: { ...shown.structuredContent, checks }, content } as R;
}
