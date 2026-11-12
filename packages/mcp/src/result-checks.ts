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
 * That figure is the publisher's, for that model only: another decider's confidence is uncalibrated
 * here, so its low answers are flagged without the accuracy claim.
 */
const CONFIDENT = 0.9;
const CALIBRATED_DECIDER = /(?:^|\/)strands-decider(?::|$)/i;
/** Two nearest matches closer than this are too close to choose between. */
const CLOSE = 0.02;
/** A best nearest score under this is a weak match. */
const WEAK = 0.45;

/** `tools`: the tool definitions sent with the task, if any. */
export type CheckContext = { job?: string; task?: string; answer: string; structured: Fields; tools?: unknown[]; workflow?: string };

/** The model that produced the result, from the receipt's route (absent when the receipt lacks one). */
function answeringModel(structured: Fields): string | undefined {
  const selected = object(structured.route)?.selected_model ?? object(structured.response)?.model ?? structured.model;
  return typeof selected === "string" ? selected : undefined;
}

/** Coding workflows whose answer is edits; review-code and explain-code answer in prose. */
const PROSE_CODING_WORKFLOWS = new Set(["review-code", "explain-code"]);

function decisionChecks(answers: Fields, model: string | undefined): string[] {
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
  if (low.length > 0) {
    checks.push(model !== undefined && CALIBRATED_DECIDER.test(model)
      ? `confidence under ${CONFIDENT}: ${low.join(", ")}; the decider is right about 95% of the time only at ${CONFIDENT} or more, so confirm these`
      : `confidence under ${CONFIDENT}: ${low.join(", ")}; ${model ?? "this decider"} has no published calibration, so treat its confidence as a ranking, not an accuracy, and confirm these`);
  }
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
  // FreeLlama's own bracketed notices are not facts from the source.
  answer = answer.replace(/\((?:empty answer|empty response)[^)]*\)|\[freellama:[^\]]*\]/g, " ");
  // Code quotes are read whole, paths included (`POST /api/orders 500` is one quoted log line);
  // numbers are read outside them, with file paths blanked (a path's digits are not figures).
  const quoted = [...answer.matchAll(/`([^`\n]{2,60})`/g)].map((match) => match[1]!.trim());
  const prose = answer.replace(/`[^`\n]{2,60}`/g, " ").replace(/(?:\/|\.{1,2}\/)?[\w.@-]+(?:\/[\w.@-]+)+/g, " ");
  const numbers = [...prose.matchAll(/(?<![\w.])\d[\d,._:/-]*\d%?|(?<![\w.])\d%?(?![\w.])/g)].map((match) => match[0]);
  const found = (fact: string) => haystack.includes(fact) || haystack.includes(fact.replace(/,/g, ""));
  // A range the model joined with a hyphen ("10:01:10-10:01:40") holds when both ends do.
  const range = (fact: string) => /^(\d[\d:.]*:\d[\d:.]*)-(\d[\d:.]*:\d[\d:.]*)$/.exec(fact);
  return [...new Set([...quoted, ...numbers].filter((fact) => fact.length > 0))]
    .filter((fact) => !found(fact) && !(range(fact)?.slice(1).every(found) ?? false));
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
    // `--- file: src/a.ts ---` copied whole from the input framing leaves a trailing `---`.
    const path = match[1]!.trim().replace(/^(?:#+\s*|[*`]+|-{3}\s*|\+{3}\s*|(?:file|path):\s*)+|(?:\s*-{3}|[*`:]+)+$/gi, "").trim();
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
 * A reply that is a tool call although no tools were sent is not an answer. The CLI exits 1 on it,
 * so keep this text stable.
 */
export const TOOL_CALL_WITHOUT_TOOLS = "the model answered with a tool call although no tools were sent: discard it and rerun on a stronger pick that fits (models {view:\"catalog\", job}) or pass `model`";

/**
 * Tool calls checked against the tools that were sent: a known name, arguments that are an object
 * with every required parameter, no undeclared one, and declared types and enums. Small models
 * often call a tool that was not offered, drop a required argument or pass a number as a string.
 */
function toolCallChecks(calls: unknown[], tools: unknown[] | undefined): string[] {
  if (!tools || tools.length === 0) return [TOOL_CALL_WITHOUT_TOOLS];
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

/**
 * Text in the input that addresses the model rather than describing anything: a summary, review or
 * extraction over it may have followed it instead of the caller. Prompt-side defenses only halve
 * such injections (arXiv 2403.14720), so this check runs outside the model, on the input itself.
 */
const ADDRESSED_TO_MODEL = [
  /\b(?:ignore|disregard|forget)\s+(?:all\s+|any\s+|the\s+|your\s+)*(?:previous\s+|prior\s+|above\s+|earlier\s+|preceding\s+)?(?:instructions?|prompts?|rules|task|directions)\b/i,
  /\byou are now\b/i,
  /^\s*(?:system|assistant)\s*:/im,
  /\bnew instructions?\s*:/i,
];
/** A caller may ask this of the model in their own request, so only clear data is checked for it. */
const ASKS_FOR_ONLY = /\b(?:reply|respond|answer|output)\s+(?:only\s+with|with\s+only)\b/i;

/**
 * The data part of a request, where an instruction is out of place: inlined files (after the first
 * `--- file:` line), else what follows the request's first paragraph (the CLI appends piped input
 * after a blank line). The caller's own request is never scanned.
 */
export function inputData(prompt: string | undefined, state?: string): string {
  const text = prompt ?? "";
  const file = text.indexOf("--- file: ");
  const data = file >= 0 ? text.slice(file) : text.includes("\n\n") ? text.slice(text.indexOf("\n\n") + 2) : "";
  return [data, state ?? ""].filter(Boolean).join("\n");
}

/**
 * A check when the input carries instructions addressed to a model; undefined otherwise. `unsplit`
 * is a prompt with no data part to separate (piped input alone, a gateway chat turn): it may be all
 * data or all request, so it is scanned only for what no caller asks of their own model.
 */
export function injectionCheck(input: string, unsplit = ""): string | undefined {
  const match = [...ADDRESSED_TO_MODEL, ASKS_FOR_ONLY].map((pattern) => pattern.exec(input)).find(Boolean)
    ?? ADDRESSED_TO_MODEL.map((pattern) => pattern.exec(unsplit)).find(Boolean);
  if (!match) return undefined;
  const quoted = match[0].trim().replace(/\s+/g, " ").slice(0, 60);
  return `the input contains text addressed to a model ("${quoted}"): check the answer followed your request, not that text`;
}

/**
 * extract-json fields left null although the input names them with a value ("Receipt R-19" for
 * receipt_id): small models drop a value whose label differs from the field name (eval 2026-11-12,
 * bonsai-2:9b 3 of 12). Checked here, outside the model.
 */
export function nullFieldCheck(answer: string, input: string): string | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(answer.replace(/^\[[^\]\n]*\]\s*/, "").replace(/^```(?:json)?\s*|\s*```\s*$/g, "").trim());
  } catch {
    return undefined;
  }
  const fields = object(parsed);
  if (!fields) return undefined;
  const found: string[] = [];
  for (const [key, value] of Object.entries(fields)) {
    if (value !== null) continue;
    const label = key.toLowerCase().replace(/[_-]?(?:id|number|no|num|code|ref)$/, "").replace(/[_-]+/g, " ").trim();
    if (label.length < 3) continue;
    const match = new RegExp(`\\b${label.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*(?:#|no\\.?|number|id|ref(?:erence)?\\.?)?\\s*:?\\s*([A-Za-z]*-?\\d[\\w-]*)`, "i").exec(input);
    if (match) found.push(`${key} (the input has "${match[0].trim()}")`);
  }
  return found.length > 0 ? `null although the input names a value: ${found.join(", ")}; check the extraction` : undefined;
}

/** The checks for one finished result; empty when there is nothing specific to say. */
export function resultChecks(context: CheckContext): string[] {
  const calls = list(object(object(context.structured.response)?.message)?.tool_calls);
  if (calls.length > 0 || /^\s*tool_calls:/.test(context.answer)) return toolCallChecks(calls, context.tools);
  if (/^\s*\(empty answer/.test(context.answer)) return [];
  const response = object(context.structured.response);
  const answers = object(response?.answers);
  if (answers) return decisionChecks(answers, answeringModel(context.structured));
  const nearest = object(context.structured.nearest);
  if (nearest) return nearestChecks(nearest);
  const files = readPaths(context.structured);
  const edits = !PROSE_CODING_WORKFLOWS.has(context.workflow ?? "");
  if (edits && (context.job === "coding" || context.task === "coding" || context.task === "code_repair")) {
    const checks = codingChecks(context.answer, files);
    if (checks.length > 0) return checks;
  }
  const padding = paddingChecks(context.answer);
  if (context.task === "vision" || context.job === "ocr" || context.job === "image-recognition") return [...padding, ...visionChecks(context)];
  const sources = files.map(readText).filter((text): text is string => text !== undefined);
  if (sources.length > 0 && context.answer.trim()) {
    const missing = unsupportedFacts(context.answer, sources);
    return [...padding, ...(missing.length > 0
      ? [`not in the source: ${missing.slice(0, 8).join(", ")}${missing.length > 8 ? ", …" : ""}; verify these before reusing them`]
      : ["every number and quoted name in the answer appears in the source"])];
  }
  return padding;
}

/** Phrases that add nothing to an answer: small models append them unasked. */
const FILLER = /\b(let me know if|i hope this helps|feel free to|as an ai\b|here(?:'s| is) (?:a|the|your) (?:summary|breakdown|answer))/i;
/** A bullet about what the input lacks, which a summary should leave out. */
const ABSENT = /^\s*(?:[-*•]|\d+[.)])\s.*\b(?:not (?:provided|specified|mentioned|stated|given|visible)|no (?:further |other |additional )?(?:details|information|data) (?:provided|given|available))\b/i;

/**
 * Padding and leaked reasoning in a text answer: what to strip before reusing it. Thinking that
 * reached the content (`<think>`) is the model's scratch work, not its answer.
 */
export function paddingChecks(answer: string): string[] {
  const checks: string[] = [];
  if (/<\/?think>/i.test(answer)) checks.push("the answer contains <think> reasoning; use only the text after it");
  const filler = answer.match(FILLER)?.[0];
  if (filler) checks.push(`filler ("${filler}"); drop that sentence`);
  const absent = answer.split("\n").filter((line) => ABSENT.test(line)).length;
  if (absent > 0) checks.push(`${absent} line(s) only say what the input lacks; drop them`);
  return checks;
}

/**
 * When an explicit fallback chain moved past the first model: which model answered and why the
 * ones before it were refused, so the caller checks the answer against the model it really got.
 */
export function fallbackCheck(structured: Fields): string | undefined {
  const fallback = object(structured.fallback);
  if (fallback?.used !== true) return undefined;
  const refused = list(fallback.attempts).map(object).filter((row): row is Fields => row !== undefined)
    .map((row) => `${typeof row.model === "string" ? row.model : "the routed model"} (${typeof row.code === "string" ? row.code : "refused"})`);
  const selected = typeof fallback.selected === "string" ? fallback.selected : "a fallback model";
  return `answered by fallback ${selected} after ${refused.join(", ")} was refused before running; judge the answer by that model`;
}

/** The result with `checks` in structuredContent and as `check:` lines after the answer. */
export function withChecks<R>(result: R, job: string | undefined, task: string | undefined, tools?: unknown[], workflow?: string): R {
  const shown = result as { content?: Array<{ type: string; text?: string }>; structuredContent?: Fields; isError?: boolean };
  if (shown.isError || !shown.structuredContent) return result;
  const answer = (shown.content ?? []).map((part) => part.text ?? "").join("\n").replace(/^\[[^\]\n]*\] /, "");
  const fallback = fallbackCheck(shown.structuredContent);
  const checks = [...(fallback ? [fallback] : []), ...resultChecks({ job, task, answer, structured: shown.structuredContent, tools, workflow })];
  if (checks.length === 0) return result;
  const content = (shown.content ?? []).map((part, index) => (index === 0 && part.type === "text"
    ? { ...part, text: `${part.text ?? ""}\n${checks.map((check) => `check: ${check}`).join("\n")}` }
    : part));
  return { ...result, structuredContent: { ...shown.structuredContent, checks }, content } as R;
}
