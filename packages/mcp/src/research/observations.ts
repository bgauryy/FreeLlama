// Pagination keeps every byte of a tool observation. The full observation is stored on disk, the
// model is shown one page plus an exact instruction for requesting the next, and a page request
// re-serves stored text rather than re-running the command.
import { randomBytes } from "node:crypto";
import { closeSync, existsSync, mkdirSync, mkdtempSync, openSync, readFileSync, readSync, renameSync, rmSync, unlinkSync, writeSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { OBSERVATION_PAGE_CHARS } from "./context-policy.js";
import { ValidationError } from "./errors.js";
import { clip } from "./fit.js";
import { cpAdvance, cpLength, cpSlice } from "./text.js";

/**
 * Return `[chunk, page, totalPages]`, preferring line boundaries within a hard limit. An oversized
 * line is split to keep minified output bounded; joining all chunks reproduces the exact input.
 */
export function paginate(text: string, page = 1, pageSize = OBSERVATION_PAGE_CHARS): [string, number, number] {
  if (pageSize <= 0) throw new ValidationError("observation page size must be > 0");
  if (!text) return ["", 1, 1];
  const pages: string[][] = [[]];
  let size = 0;
  for (const line of text.match(/[^\n]*\n|[^\n]+$/g) ?? []) {
    const lineLength = cpLength(line);
    if (size && size + lineLength > pageSize) {
      pages.push([]);
      size = 0;
    }
    for (let start = 0; start < lineLength; start += pageSize) {
      const part = cpSlice(line, start, start + pageSize);
      const partLength = cpLength(part);
      if (size && size + partLength > pageSize) {
        pages.push([]);
        size = 0;
      }
      pages[pages.length - 1]!.push(part);
      size += partLength;
    }
  }
  const total = Math.max(1, pages.length);
  const chosen = Math.max(1, Math.min(page, total));
  return [pages[chosen - 1]!.join(""), chosen, total];
}

/** Tell the model exactly how to get the rest — or that there is no rest. */
export function pageFooter(step: number, page: number, total: number, totalChars: number): string {
  if (total <= 1) return "";
  const head = `\n\n[page ${page} of ${total} — ${totalChars} characters total, nothing discarded. `;
  if (page >= total) return `${head}End of stored output.]`;
  return `${head}For the next page send exactly: {"action":"page","step":${step},"page":${page + 1}} `
    + "— that re-reads stored output and does NOT re-run the command.]";
}

interface StoredObservation {
  path: string;
  indexPath: string;
  characters: number;
  pages: number;
  metadata: Record<string, string>;
}

/** UTF-8 encode, refusing lone surrogates (they have no UTF-8 encoding). */
function encodeStrict(text: string): Buffer {
  if (/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/.test(text)) {
    throw new ValidationError("observation contains a lone surrogate and cannot be encoded as UTF-8");
  }
  return Buffer.from(text, "utf8");
}

function writeAll(fd: number, data: Buffer): void {
  let offset = 0;
  while (offset < data.length) offset += writeSync(fd, data, offset, data.length - offset);
}

function readAt(file: string, position: number, length: number): Buffer {
  const fd = openSync(file, "r");
  try {
    const buffer = Buffer.alloc(length);
    let read = 0;
    while (read < length) {
      const count = readSync(fd, buffer, read, length - read, position + read);
      if (count === 0) break;
      read += count;
    }
    return buffer.subarray(0, read);
  } finally {
    closeSync(fd);
  }
}

const utf8 = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });

/**
 * Disk-backed exact observations with one small descriptor per step in memory. Page offsets live on
 * disk too: an arbitrary page read loads one fixed-width index record and one page of bytes.
 */
export class ObservationStore {
  readonly directory: string;
  readonly pageSize: number;
  private readonly byStep = new Map<number, StoredObservation>();

  constructor(pageSize = OBSERVATION_PAGE_CHARS) {
    if (pageSize <= 0) throw new ValidationError("observation page size must be > 0");
    this.directory = mkdtempSync(path.join(path.resolve(tmpdir()), "freellama-observations-"));
    this.pageSize = pageSize;
  }

  close(): void {
    this.byStep.clear();
    rmSync(this.directory, { recursive: true, force: true });
  }

  put(step: number, text: string, metadata: Record<string, unknown> = {}): void {
    if (step < 1) throw new ValidationError("observation step must be positive");
    const file = path.join(this.directory, `${step}.utf8`);
    const indexPath = path.join(this.directory, `${step}.index`);
    // Indexes are UTF-16 offsets that always sit on code-point boundaries; page sizes count code
    // points.
    let start = 0;
    let offset = 0;
    let pages = 0;
    let characters = 0;
    const output = openSync(file, "w");
    const index = openSync(indexPath, "w");
    try {
      while (start < text.length) {
        let end = cpAdvance(text, start, this.pageSize);
        // Prefer a complete line; a line longer than a page is split at the hard limit, losslessly.
        if (end < text.length) {
          const newline = text.lastIndexOf("\n", end - 1);
          if (newline >= start) end = newline + 1;
        }
        const slice = text.slice(start, end);
        characters += cpLength(slice);
        const encoded = encodeStrict(slice);
        writeAll(output, encoded);
        const nextOffset = offset + encoded.length;
        const record = Buffer.alloc(16);
        record.writeBigUInt64BE(BigInt(offset), 0);
        record.writeBigUInt64BE(BigInt(nextOffset), 8);
        writeAll(index, record);
        offset = nextOffset;
        pages += 1;
        start = end;
      }
      if (!pages) {
        writeAll(index, Buffer.alloc(16));
        pages = 1;
      }
    } finally {
      closeSync(output);
      closeSync(index);
    }
    const bounded: Record<string, string> = {};
    for (const [key, value] of Object.entries(metadata)) {
      if (key === "action" || key === "status" || key === "source") bounded[key] = clip(String(value), 240);
    }
    this.byStep.set(step, { path: file, indexPath, characters, pages, metadata: bounded });
  }

  /** Explicit full materialization for consumers that need it; adapters use view/writeResult. */
  get(step: number): string | null {
    const record = this.byStep.get(step);
    return record ? utf8.decode(readFileSync(record.path)) : null;
  }

  pageCount(step: number): number {
    const record = this.byStep.get(step);
    if (!record) throw new Error(`no stored observation for step ${step}`);
    return record.pages;
  }

  /** Return `[body, footer]` for one page of a stored observation. */
  view(step: number, page = 1): [string, string] {
    const record = this.byStep.get(step);
    if (!record) {
      const known = [...this.byStep.keys()].sort((a, b) => a - b).join(", ") || "none yet";
      return [`no stored output for step ${step} (known steps: ${known})`, ""];
    }
    const chosen = Math.max(1, Math.min(page, record.pages));
    const entry = readAt(record.indexPath, (chosen - 1) * 16, 16);
    const start = Number(entry.readBigUInt64BE(0));
    const end = Number(entry.readBigUInt64BE(8));
    const chunk = utf8.decode(readAt(record.path, start, end - start));
    return [chunk, pageFooter(step, chosen, record.pages, record.characters)];
  }

  message(step: number, page = 1): string {
    const [body, footer] = this.view(step, page);
    const record = this.byStep.get(step);
    if (!record) return `Observation (step ${step}):\n${body}`;
    const chosen = Math.max(1, Math.min(page, record.pages));
    const ledger = { ...record.metadata, step, page: chosen, total_pages: record.pages, trust: "untrusted_tool_output" };
    return `Observation (step ${step}):\n[evidence ledger: ${JSON.stringify(ledger)}]\n${body}${footer}`;
  }

  /** Decode a stored UTF-8 file in 64 KiB reads, never splitting a code point across chunks. */
  private streamStored(file: string, consume: (chunk: string) => void): void {
    const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
    const fd = openSync(file, "r");
    try {
      const buffer = Buffer.alloc(65_536);
      for (;;) {
        const count = readSync(fd, buffer, 0, buffer.length, null);
        if (count === 0) break;
        const chunk = decoder.decode(buffer.subarray(0, count), { stream: true });
        if (chunk) consume(chunk);
      }
      const rest = decoder.decode();
      if (rest) consume(rest);
    } finally {
      closeSync(fd);
    }
  }

  /** Atomically write the public JSON result, restoring full call results from disk. */
  writeResult(resultPath: string, result: Record<string, unknown>): void {
    const directory = path.dirname(resultPath);
    mkdirSync(directory, { recursive: true });
    const temporary = path.join(directory, `.${path.basename(resultPath)}.${randomBytes(6).toString("hex")}.tmp`);
    let fd: number | null = null;
    try {
      // The temporary file is created 0600; the rename keeps that mode on the result.
      fd = openSync(temporary, "wx", 0o600);
      const out = fd;
      const emit = (text: string) => writeAll(out, Buffer.from(text, "utf8"));
      emit("{");
      Object.entries(result).forEach(([key, value], fieldIndex) => {
        if (fieldIndex) emit(",");
        emit(`${JSON.stringify(key)}:`);
        if (key !== "tool_calls") {
          emit(JSON.stringify(value) ?? "null");
          return;
        }
        emit("[");
        (value as Array<Record<string, unknown>>).forEach((call, callIndex) => {
          const step = callIndex + 1;
          if (step > 1) emit(",");
          const metadata: Record<string, unknown> = {};
          for (const [name, item] of Object.entries(call)) if (name !== "result") metadata[name] = item;
          const encoded = JSON.stringify(metadata);
          emit(encoded.slice(0, -1) + (Object.keys(metadata).length ? "," : "") + '"result":"');
          const record = this.byStep.get(step);
          if (!record) throw new Error(`no stored observation for step ${step}`);
          // Stream the stored text in bounded chunks without newline translation.
          this.streamStored(record.path, (chunk) => emit(JSON.stringify(chunk).slice(1, -1)));
          emit('"}');
        });
        emit("]");
      });
      emit("}\n");
      closeSync(fd);
      fd = null;
      renameSync(temporary, resultPath);
    } finally {
      if (fd !== null) closeSync(fd);
      if (existsSync(temporary)) unlinkSync(temporary);
    }
  }
}
