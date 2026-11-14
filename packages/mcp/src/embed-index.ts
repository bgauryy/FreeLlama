// A persistent index of embedding vectors, so a repeated `nearest` search over a code tree (the
// find-code job) embeds only the chunks that changed instead of every chunk again. One JSONL
// file per model identity and verified execution scope under FREELLAMA_HOME/embedding-index/, keyed
// by a hash of the exact text embedded (document prefix included); vectors are stored as base64
// float32. A changed file changes its chunks' text and so their keys: stale entries are simply
// never read again. FREELLAMA_EMBED_INDEX=0 turns it off.
import { createHash } from "node:crypto";
import { appendFileSync, existsSync, mkdirSync, readFileSync, statSync, unlinkSync } from "node:fs";
import path from "node:path";
import { FREELLAMA_HOME } from "./config.js";

/** An index file past this size is dropped and rebuilt rather than loaded whole. */
const MAX_INDEX_BYTES = 256 * 1024 * 1024;

const sha = (text: string) => createHash("sha256").update(text).digest("hex");

export function embedIndexEnabled(env: NodeJS.ProcessEnv = process.env): boolean {
  return env.FREELLAMA_EMBED_INDEX !== "0";
}

function encode(vector: number[]): string {
  return Buffer.from(new Float32Array(vector).buffer).toString("base64");
}

function decode(text: string): number[] {
  const bytes = Buffer.from(text, "base64");
  return Array.from(new Float32Array(bytes.buffer, bytes.byteOffset, bytes.byteLength / 4));
}

export class EmbeddingIndex {
  private readonly vectors = new Map<string, number[]>();

  private constructor(readonly file: string, private readonly dimensions: number | undefined) {}

  /** The index of one model and execution identity; different digests, sizes or scopes never mix. */
  static open(model: string, digest: string | undefined, dimensions: number | undefined, root = path.join(FREELLAMA_HOME, "embedding-index"), scope = ""): EmbeddingIndex {
    const identity = sha(`${model}\n${digest ?? ""}\n${dimensions ?? ""}\n${scope}`).slice(0, 24);
    const index = new EmbeddingIndex(path.join(root, `${identity}.jsonl`), dimensions);
    try { index.load(); } catch { /* Optional cache: unreadable or concurrently removed files re-embed. */ }
    return index;
  }

  private load(): void {
    if (!existsSync(this.file)) return;
    if (statSync(this.file).size > MAX_INDEX_BYTES) {
      unlinkSync(this.file);
      return;
    }
    for (const line of readFileSync(this.file, "utf8").split("\n")) {
      if (!line) continue;
      try {
        const { k, v } = JSON.parse(line) as { k: string; v: string };
        if (typeof k !== "string" || !/^[a-f0-9]{64}$/.test(k) || typeof v !== "string") continue;
        const vector = decode(v);
        if (this.valid(vector)) this.vectors.set(k, vector);
      } catch {
        // A torn last line from an interrupted write: skip it.
      }
    }
  }

  static key(text: string): string {
    return sha(text);
  }

  private valid(vector: number[]): boolean {
    return vector.length > 0 && (this.dimensions === undefined || vector.length === this.dimensions)
      && vector.every((value) => Number.isFinite(value) && Number.isFinite(Math.fround(value)));
  }

  get(text: string): number[] | undefined {
    return this.vectors.get(EmbeddingIndex.key(text));
  }

  /** Remember vectors for texts, appending them to the file (a write failure keeps them in memory only). */
  add(entries: Array<{ text: string; vector: number[] }>): void {
    if (entries.some(({ vector }) => !this.valid(vector))) throw new Error("embedding index requires finite vectors of the verified dimensions");
    const lines: string[] = [];
    for (const { text, vector } of entries) {
      const key = EmbeddingIndex.key(text);
      if (this.vectors.has(key)) continue;
      const stored = vector.map(Math.fround);
      this.vectors.set(key, stored);
      lines.push(JSON.stringify({ k: key, v: encode(stored) }));
    }
    if (lines.length === 0) return;
    try {
      mkdirSync(path.dirname(this.file), { recursive: true });
      appendFileSync(this.file, `${lines.join("\n")}\n`);
    } catch {
      // The index is an optimization: a read-only home still answers, it just re-embeds next time.
    }
  }

  get size(): number {
    return this.vectors.size;
  }
}

export function cosine(a: number[], b: number[]): number {
  if (a.length !== b.length || a.some((v) => !Number.isFinite(v)) || b.some((v) => !Number.isFinite(v))) {
    throw new Error("cosine requires finite vectors with matching dimensions");
  }
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (let i = 0; i < a.length; i += 1) {
    dot += a[i]! * b[i]!;
    na += a[i]! * a[i]!;
    nb += b[i]! * b[i]!;
  }
  return na === 0 || nb === 0 ? 0 : dot / Math.sqrt(na * nb);
}

/** The `top` items closest to the query (at least `minScore`), best first: the core's query ranking. */
export function rankByQuery(query: number[], items: Array<{ label: string; vector: number[] }>, top = 5, minScore?: number) {
  return items
    .map((item, index) => ({ index, label: item.label, score: cosine(query, item.vector) }))
    .filter((match) => minScore === undefined || match.score >= minScore)
    .sort((a, b) => b.score - a.score)
    .slice(0, top);
}
