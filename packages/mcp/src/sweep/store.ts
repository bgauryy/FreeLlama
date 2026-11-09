// Durable sweep state: one directory per sweep under the FreeLlama home. The manifest holds the
// configuration and the chunk index; chunk text and per-chunk results are append-only JSONL, so a
// sweep interrupted by a crash or a server restart resumes from the last saved result.
import { appendFileSync, existsSync, mkdirSync, readdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import path from "node:path";
import { createHash } from "node:crypto";
import { FREELLAMA_HOME } from "../config.js";

export const SWEEP_ROOT = process.env.FREELLAMA_SWEEP_DIR || path.join(FREELLAMA_HOME, "sweeps");

export type Question = { type: "noul" | "choice" | "score"; instructions: unknown; criteria?: unknown };

/**
 * When an answer flags a chunk. `when` is a choice option, or "true" for a yes/no (noul) question;
 * `atLeast` is the probability of that answer needed to flag (default 0.5).
 */
export type FlagRule = { question: string; when?: string; atLeast?: number };

export type SweepSource =
  | { kind: "files"; files: string[] }
  | { kind: "urls"; urls: string[]; sameOrigin: boolean; maxPages: number; maxDepth: number; include?: string };

export type SweepConfig = {
  id: string;
  name?: string;
  createdAt: string;
  endpoint?: string;
  model: string;
  questions: Record<string, Question>;
  systemPrompt?: string;
  flags: FlagRule[];
  /** Probabilities inside this band, around a flag rule's threshold, are "unsure", not decided. */
  unsureMargin: number;
  verify?: { model: string };
  explain?: { model: string; prompt: string; maxTokens: number };
  priority: "interactive" | "normal" | "background";
  maxParallelism: number;
  chunkBytes: number;
  baseline?: string;
  source: SweepSource;
};

export type Chunk = { id: string; label: string; source: string; lines?: [number, number]; hash: string; bytes: number };

export type Verdict = "flagged" | "clear" | "unsure";
export type Change = "new" | "changed" | "unchanged";

export type ChunkResult = {
  chunk: string;
  status: "done" | "failed";
  verdict?: Verdict;
  /** Set when a sweep has a baseline: how this chunk compares with it. */
  change?: Change;
  /** The result was copied from the baseline because the chunk text did not change. */
  reused?: boolean;
  answers?: Record<string, unknown>;
  verified?: { model: string; verdict: Verdict; answers: Record<string, unknown> };
  note?: string;
  error?: string;
  attempts: number;
  model: string;
  ms: number;
  at: string;
};

export type Phase = "collecting" | "running" | "completed" | "cancelled" | "failed" | "interrupted";

export type Manifest = {
  config: SweepConfig;
  phase: Phase;
  chunks: Chunk[];
  skipped: { source: string; reason: string }[];
  pages?: number;
  removed?: string[];
  error?: string;
  startedAt: string;
  finishedAt?: string;
  /** The server process running the sweep; absent in records from older versions. */
  owner?: { pid: number };
};

/** True when a process with this pid exists (EPERM still means it exists). */
export function processAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

export const hashText = (text: string) => createHash("sha256").update(text).digest("hex").slice(0, 32);

export class SweepStore {
  constructor(readonly root = SWEEP_ROOT) {}

  dir(id: string) {
    if (!/^[0-9a-f-]{36}$/.test(id)) throw new Error(`invalid sweepId ${id}`);
    return path.join(this.root, id);
  }

  create(manifest: Manifest) {
    const dir = this.dir(manifest.config.id);
    mkdirSync(dir, { recursive: true });
    this.saveManifest(manifest);
    writeFileSync(path.join(dir, "chunks.jsonl"), "");
    writeFileSync(path.join(dir, "results.jsonl"), "");
  }

  exists(id: string) {
    return existsSync(path.join(this.dir(id), "manifest.json"));
  }

  /** Written to a temp file and renamed, so a crash never leaves half a manifest. */
  saveManifest(manifest: Manifest) {
    const file = path.join(this.dir(manifest.config.id), "manifest.json");
    writeFileSync(`${file}.tmp`, JSON.stringify(manifest, null, 1));
    renameSync(`${file}.tmp`, file);
  }

  manifest(id: string): Manifest {
    return JSON.parse(readFileSync(path.join(this.dir(id), "manifest.json"), "utf8")) as Manifest;
  }

  addChunk(id: string, chunk: Chunk, text: string) {
    appendFileSync(path.join(this.dir(id), "chunks.jsonl"), `${JSON.stringify({ id: chunk.id, text })}\n`);
  }

  chunkTexts(id: string): Map<string, string> {
    return new Map(jsonl<{ id: string; text: string }>(path.join(this.dir(id), "chunks.jsonl")).map((row) => [row.id, row.text]));
  }

  addResult(id: string, result: ChunkResult) {
    appendFileSync(path.join(this.dir(id), "results.jsonl"), `${JSON.stringify(result)}\n`);
  }

  /** The latest result per chunk: a resumed or retried chunk appends, and the last line wins. */
  results(id: string): Map<string, ChunkResult> {
    const latest = new Map<string, ChunkResult>();
    for (const row of jsonl<ChunkResult>(path.join(this.dir(id), "results.jsonl"))) latest.set(row.chunk, row);
    return latest;
  }

  files(id: string) {
    const dir = this.dir(id);
    return { manifest: path.join(dir, "manifest.json"), chunks: path.join(dir, "chunks.jsonl"), results: path.join(dir, "results.jsonl") };
  }

  list(): string[] {
    if (!existsSync(this.root)) return [];
    return readdirSync(this.root).filter((name) => /^[0-9a-f-]{36}$/.test(name) && existsSync(path.join(this.root, name, "manifest.json")));
  }
}

/** Rows of a JSONL file; a torn last line from a crash is skipped, not fatal. */
function jsonl<T>(file: string): T[] {
  if (!existsSync(file)) return [];
  return readFileSync(file, "utf8").split("\n").flatMap((line) => {
    if (!line.trim()) return [];
    try {
      return [JSON.parse(line) as T];
    } catch {
      return [];
    }
  });
}
