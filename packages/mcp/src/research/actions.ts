// Model action parsing shared by the research adapters: recover one JSON object from a reply,
// detect exact repeat calls, and the notices sent when a reply is unusable or repeated.
import { canonicalJson, isRecord } from "./canonical-json.js";
import { ValidationError } from "./errors.js";

export type Action = Record<string, unknown>;


/** The JSON object that starts at `start`: its text ends at the brace that balances the first. */
function objectAt(text: string, start: number): unknown {
  let depth = 0;
  let inString = false;
  for (let index = start; index < text.length; index += 1) {
    const char = text[index];
    if (inString) {
      if (char === "\\") index += 1;
      else if (char === '"') inString = false;
    } else if (char === '"') {
      inString = true;
    } else if (char === "{" || char === "[") {
      depth += 1;
    } else if (char === "}" || char === "]") {
      depth -= 1;
      if (depth === 0) return JSON.parse(text.slice(start, index + 1));
    }
  }
  throw new SyntaxError("unterminated JSON object");
}

/** Recover one JSON object while leaving adapter-specific action validation to the caller. */
export function parseJsonAction(content: string): Action {
  let text = content.trim();
  if (text.startsWith("```")) text = text.split(/\r\n|\r|\n/).slice(1, -1).join("\n").trim();
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (error) {
    if (!(error instanceof SyntaxError)) throw error;
    const start = text.indexOf("{");
    if (start < 0) throw new ValidationError("model did not return a JSON action");
    try {
      value = objectAt(text, start);
    } catch (inner) {
      if (!(inner instanceof SyntaxError)) throw inner;
      throw new ValidationError("model did not return a valid JSON action");
    }
  }
  if (!isRecord(value) || typeof value.action !== "string") throw new ValidationError("model action must be a JSON object with an action");
  return value;
}

/**
 * Canonical form of a tool call, used to detect an exact repeat within one run. A repeat costs a
 * turn and re-appends an observation the model has already seen.
 */
export function callSignature(payload: unknown): string {
  return canonicalJson(payload, { ensureAscii: false });
}

export const REPEAT_NOTICE = "(identical call already made this run — its result is above and was not re-executed. "
  + "Use a different call, or finish now with the evidence already collected.)";

export const PARSE_REPAIR_NOTICE = "Your last reply was not a valid JSON action, so it was discarded. Reply with EXACTLY one JSON "
  + "object and nothing else — no prose before or after, no markdown fence. Use the action shapes "
  + "given in your instructions. If you already have enough evidence, send the finish action now.";
