// Text helpers. Clip and page limits count Unicode code points, and a cut never splits a
// surrogate pair. External bytes decode as strict UTF-8.
import { ValidationError } from "./errors.js";

/** Unicode whitespace plus the ASCII information separators (U+001C to U+001F). */
export const SPACE_CLASS = "\\p{White_Space}\\x1c-\\x1f";
const SPACE = new RegExp(`^[${SPACE_CLASS}]$`, "u");
const EDGE_SPACE = new RegExp(`^[${SPACE_CLASS}]+|[${SPACE_CLASS}]+$`, "gu");

export const isSpace = (char: string | undefined): boolean => char !== undefined && SPACE.test(char);
export const trimSpace = (text: string): string => text.replace(EDGE_SPACE, "");

const ASTRAL = /[\uD800-\uDBFF][\uDC00-\uDFFF]/;

export function cpLength(text: string): number {
  if (!ASTRAL.test(text)) return text.length;
  let count = 0;
  for (const _ of text) count += 1;
  return count;
}

/** UTF-16 offset after advancing `count` code points from the UTF-16 offset `start`. */
export function cpAdvance(text: string, start: number, count: number): number {
  let index = start;
  for (let step = 0; step < count && index < text.length; step += 1) {
    const code = text.charCodeAt(index);
    const pair = code >= 0xd800 && code <= 0xdbff && index + 1 < text.length
      && text.charCodeAt(index + 1) >= 0xdc00 && text.charCodeAt(index + 1) <= 0xdfff;
    index += pair ? 2 : 1;
  }
  return index;
}

/** Code points `[start, end)` of `text`; `end` defaults to the end of the text. */
export function cpSlice(text: string, start: number, end?: number): string {
  if (!ASTRAL.test(text)) return text.slice(start, end);
  const from = cpAdvance(text, 0, start);
  if (end === undefined) return text.slice(from);
  if (end <= start) return "";
  return text.slice(from, cpAdvance(text, from, end - start));
}

/** The last `count` code points of `text` (`count` > 0). */
export function cpTail(text: string, count: number): string {
  const length = cpLength(text);
  return count >= length ? text : cpSlice(text, length - count);
}

const UTF8 = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });

/** Decode strict UTF-8; `source` names the input in the error. */
export function decodeUtf8(bytes: Uint8Array, source: string): string {
  try {
    return UTF8.decode(bytes);
  } catch {
    throw new ValidationError(`${source} is not valid UTF-8`);
  }
}

/** Universal newlines: `\r\n` and `\r` become `\n`. */
export function normalizeNewlines(text: string): string {
  return text.replace(/\r\n?/g, "\n");
}
