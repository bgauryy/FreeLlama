// read_response: one section or page of a saved tool result (response_file).
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { readFile, realpath, stat } from "node:fs/promises";
import path from "node:path";
import { RESPONSE_ROOT } from "../response-store.js";
import { errorResult, objectResultSchema, structuredResult } from "../helpers.js";

type Fields = Record<string, unknown>;

const STRING_PAGE_CHARS = 20_000;
const ARRAY_PAGE_ITEMS = 20;
/** Objects up to this many JSON characters come back whole; larger ones are paged as JSON text. */
const WHOLE_OBJECT_CHARS = 20_000;

const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : undefined;

/** The saved file, resolved with symlinks followed; refuses anything outside the response root. */
export async function resolveResponseFile(file: string, root = RESPONSE_ROOT): Promise<string> {
  let base: string;
  try {
    base = await realpath(root);
  } catch {
    throw new Error(`No saved responses: ${root} does not exist.`);
  }
  let resolved: string;
  try {
    resolved = await realpath(path.resolve(base, file));
  } catch {
    throw new Error(`No saved response at ${file}.`);
  }
  if (!resolved.startsWith(`${base}${path.sep}`) || !resolved.endsWith(".json") || !(await stat(resolved)).isFile()) {
    throw new Error(`${file} is not a saved response file under ${root}.`);
  }
  return resolved;
}

/** RFC 6901 pointer into the saved record (`/result/structuredContent/response`). */
function pointer(record: unknown, section: string): { found: boolean; value: unknown } {
  let value: unknown = record;
  for (const raw of section.split("/").slice(1)) {
    const key = raw.replace(/~1/g, "/").replace(/~0/g, "~");
    if (Array.isArray(value) && /^\d+$/.test(key) && Number(key) < value.length) value = value[Number(key)];
    else if (object(value) && Object.hasOwn(value as Fields, key)) value = (value as Fields)[key];
    else return { found: false, value: undefined };
  }
  return { found: true, value };
}

/** Named views of the saved record; any other name is a top-level field of the structured result. */
function sectionValue(record: Fields, section: string): { found: boolean; value: unknown } {
  if (section.startsWith("/")) return pointer(record, section);
  const result = object(record.result);
  const content = object(result?.structuredContent);
  const response = object(content?.response);
  const named: Record<string, unknown> = {
    receipt: content,
    arguments: record.arguments,
    content: result?.content,
    thinking: object(response?.message)?.thinking ?? response?.thinking,
    embeddings: response?.embeddings,
  };
  if (Object.hasOwn(named, section)) return { found: named[section] !== undefined, value: named[section] };
  if (content && Object.hasOwn(content, section)) return { found: true, value: content[section] };
  if (Object.hasOwn(record, section)) return { found: true, value: record[section] };
  return { found: false, value: undefined };
}

function size(value: unknown): Fields {
  if (typeof value === "string") return { type: "string", chars: value.length };
  if (Array.isArray(value)) return { type: "array", items: value.length };
  return { type: value === null ? "null" : typeof value, chars: JSON.stringify(value)?.length ?? 0 };
}

/** What a saved record holds: its named sections and the structured result's top-level fields. */
function outline(file: string, record: Fields): Fields {
  const content = object(object(record.result)?.structuredContent) ?? {};
  const sections: Fields[] = [];
  for (const name of ["arguments", "content", "receipt", "thinking", "embeddings"]) {
    const { found, value } = sectionValue(record, name);
    if (found) sections.push({ name, ...size(value) });
  }
  for (const [name, value] of Object.entries(content)) sections.push({ name, ...size(value) });
  return { file, tool: record.tool, sections };
}

/** One page of `value`: characters of a string, items of an array, or the JSON text of a large object. */
export function page(value: unknown, offset = 0, limit?: number): Fields {
  if (typeof value === "string" || Array.isArray(value)) {
    const chosen = limit ?? (typeof value === "string" ? STRING_PAGE_CHARS : ARRAY_PAGE_ITEMS);
    const total = value.length;
    const end = Math.min(total, offset + chosen);
    return {
      type: typeof value === "string" ? "string" : "array",
      total, offset, limit: chosen,
      next_offset: end < total ? end : null,
      value: value.slice(offset, end),
    };
  }
  const json = JSON.stringify(value, null, 2) ?? "null";
  if (json.length <= WHOLE_OBJECT_CHARS && offset === 0 && limit === undefined) return { type: "json", value };
  return { ...page(json, offset, limit), type: "json_text" };
}

/** The read_response tool body; `root` is the response store's directory. */
export async function readResponse(
  { file, section, offset, limit }: { file: string; section?: string; offset?: number; limit?: number },
  root = RESPONSE_ROOT,
) {
  try {
    const resolved = await resolveResponseFile(file, root);
    const record = object(JSON.parse(await readFile(resolved, "utf8")));
    if (!record) return errorResult(new Error(`${file} does not hold a saved tool result.`));
    if (section === undefined) {
      if (offset !== undefined || limit !== undefined) return errorResult(new Error("offset and limit need a section."));
      const view = outline(resolved, record);
      return structuredResult(view, { text: JSON.stringify(view) });
    }
    const { found, value } = sectionValue(record, section);
    if (!found) {
      const names = (outline(resolved, record).sections as Fields[]).map((entry) => entry.name).join(", ");
      return errorResult(new Error(`No section ${section} in ${file}. Sections: ${names}.`));
    }
    const view: Fields = { file: resolved, section, ...page(value, offset, limit) };
    const text = typeof view.value === "string" ? view.value : JSON.stringify(view.value);
    const more = typeof view.next_offset === "number" ? `\n[more: offset ${view.next_offset} of ${String(view.total)}]` : "";
    return structuredResult(view, { text: text + more });
  } catch (error) {
    return errorResult(error);
  }
}

export function registerReadResponseTool(server: McpServer): void {
  server.registerTool(
    "read_response",
    {
      description:
        "Use when: a result's response_file holds what you need beyond the inline view (its `sections`). " +
        "Do not use when: the inline result already answers. " +
        "Returns: one section — a sections name, a top-level structuredContent field, or a JSON pointer such as /result/structuredContent/route — " +
        "paged by offset/limit (characters of a string, items of an array, JSON text of a large object; next_offset continues). " +
        "Omit section for an outline.",
      inputSchema: z.object({
        file: z.string().min(1),
        section: z.string().min(1).optional(),
        offset: z.number().int().nonnegative().optional(),
        limit: z.number().int().positive().optional(),
      }).strict(),
      outputSchema: objectResultSchema,
      annotations: { readOnlyHint: true },
    },
    async (args) => readResponse(args),
  );
}
