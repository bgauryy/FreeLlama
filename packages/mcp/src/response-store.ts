import { appendFile, mkdir, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { FREELLAMA_HOME } from "./config.js";

const INDEX = "index.jsonl";
const DAY_MS = 86_400_000;

type Entry = { file: string; directory: string; bytes: number; created: number };
type IndexLine = { seq?: unknown; tool?: unknown; bytes?: unknown; created?: unknown; removed?: unknown };

const fileName = (seq: number, tool: string) => `${String(seq).padStart(6, "0")}-${tool}.json`;
const ordered = (left: Entry, right: Entry) => left.created - right.created || left.file.localeCompare(right.file);

/**
 * Every tool result, whole, as one file per call under `<FreeLlama home>/responses/<session>/`, so
 * the agent's context can carry the answer and the rest stays one read away. Files hold the call's
 * arguments (prompts included) and are owner-only. Each session keeps an append-only `index.jsonl`
 * (`{seq, tool, model, bytes, created}`, plus `{seq, removed:true}` when a file is evicted), so
 * cleanup reads one index per session instead of every file. The store holds at most
 * `FREELLAMA_RESPONSE_STORE_BYTES` (default 1 GiB; `0` turns it off) across every session, drops
 * the oldest files first, and drops files older than `FREELLAMA_RESPONSE_TTL_DAYS` (default 7;
 * `0` turns the age limit off). They are copies: no answer is ever cut to make room.
 */
export class ResponseStore {
  private readonly session: string;
  private sequence = 0;
  private entries: Entry[] = [];
  private total = 0;
  private loaded: Promise<void> | undefined;
  private cleanup: Promise<void> = Promise.resolve();

  constructor(
    readonly root: string,
    private readonly maxBytes: number,
    started = new Date(),
    private readonly ttlDays = 7,
    private readonly now: () => number = Date.now,
  ) {
    this.session = `${started.toISOString().replace(/[:.]/g, "-")}-${process.pid}`;
  }

  get enabled(): boolean {
    return this.maxBytes > 0;
  }

  /** Save one result; returns the file, or `undefined` when the store is off or unwritable. */
  async save(tool: string, record: unknown, meta: { model?: string } = {}): Promise<string | undefined> {
    if (!this.enabled) return undefined;
    try {
      // Concurrent first saves share one load.
      await (this.loaded ??= this.load());
      const directory = path.join(this.root, this.session);
      await mkdir(directory, { recursive: true, mode: 0o700 });
      this.sequence += 1;
      const seq = this.sequence;
      const file = path.join(directory, fileName(seq, tool));
      const body = JSON.stringify(record, null, 2);
      await writeFile(file, body, { mode: 0o600 });
      const bytes = Buffer.byteLength(body);
      const created = this.now();
      const line = { seq, tool, ...(meta.model ? { model: meta.model } : {}), bytes, created: new Date(created).toISOString() };
      await appendFile(path.join(directory, INDEX), `${JSON.stringify(line)}\n`, { mode: 0o600 });
      this.entries.push({ file, directory, bytes, created });
      this.total += bytes;
      if (this.total > this.maxBytes || this.expired(this.entries[0])) {
        // One cleanup at a time; each re-reads the indexes so other sessions' files count.
        this.cleanup = this.cleanup.then(() => this.evict(file)).catch(() => undefined);
        await this.cleanup;
      }
      return file;
    } catch {
      return undefined;
    }
  }

  private expired(entry: Entry | undefined): boolean {
    return entry !== undefined && this.ttlDays > 0 && entry.created < this.now() - this.ttlDays * DAY_MS;
  }

  /** Index every saved file, oldest first: this process's and every other session's. */
  private async load(): Promise<void> {
    const found: Entry[] = [];
    for (const session of await readdir(this.root).catch(() => [] as string[])) {
      const directory = path.join(this.root, session);
      found.push(...(await this.readIndex(directory) ?? await this.scanDirectory(directory)));
    }
    found.sort(ordered);
    this.entries = found;
    this.total = found.reduce((sum, entry) => sum + entry.bytes, 0);
  }

  /** A session's live files from its index, or `undefined` when it has none (older sessions). */
  private async readIndex(directory: string): Promise<Entry[] | undefined> {
    let text: string;
    try {
      text = await readFile(path.join(directory, INDEX), "utf8");
    } catch {
      return undefined;
    }
    const live = new Map<number, Entry>();
    for (const raw of text.split("\n")) {
      if (!raw.trim()) continue;
      let line: IndexLine;
      try {
        line = JSON.parse(raw) as IndexLine;
      } catch {
        continue; // A torn line from a writer that crashed mid-append.
      }
      if (typeof line.seq !== "number") continue;
      if (line.removed === true) {
        live.delete(line.seq);
        continue;
      }
      if (typeof line.tool !== "string" || typeof line.bytes !== "number") continue;
      const created = typeof line.created === "string" ? Date.parse(line.created) : Number.NaN;
      live.set(line.seq, {
        file: path.join(directory, fileName(line.seq, line.tool)),
        directory,
        bytes: line.bytes,
        created: Number.isFinite(created) ? created : 0,
      });
    }
    return [...live.values()];
  }

  /** Sessions written before the index existed: one stat per file. */
  private async scanDirectory(directory: string): Promise<Entry[]> {
    const found: Entry[] = [];
    for (const name of await readdir(directory).catch(() => [] as string[])) {
      const file = path.join(directory, name);
      const info = await stat(file).catch(() => undefined);
      if (info?.isFile()) found.push({ file, directory, bytes: info.size, created: info.mtimeMs });
    }
    return found;
  }

  /**
   * Drop expired files, then the oldest until the store fits. The file just saved is always
   * kept, and each session's newest file is kept while the store stays under twice its size, so
   * a concurrent agent does not lose the result it was handed a moment ago. A file that cannot be
   * removed is skipped; cleanup never fails the save that triggered it.
   */
  private async evict(keep: string): Promise<void> {
    await this.load();
    // Names carry the session's sequence, so the greatest name is its newest file even when
    // several were written within one clock tick.
    const newest = new Map<string, string>();
    const remaining = new Map<string, number>();
    for (const { file, directory } of this.entries) {
      if ((newest.get(directory) ?? "") < file) newest.set(directory, file);
      remaining.set(directory, (remaining.get(directory) ?? 0) + 1);
    }
    const own = path.join(this.root, this.session);
    const kept: Entry[] = [];
    while (this.entries.length > 0) {
      const oldest = this.entries[0]!;
      const expired = this.expired(oldest);
      if (!expired && this.total <= this.maxBytes) break;
      this.entries.shift();
      const newestKept = newest.get(oldest.directory) === oldest.file && !expired && this.total <= 2 * this.maxBytes;
      if (oldest.file === keep || newestKept) {
        kept.push(oldest);
        continue;
      }
      this.total -= oldest.bytes;
      if (!(await this.remove(oldest))) continue;
      const left = (remaining.get(oldest.directory) ?? 1) - 1;
      remaining.set(oldest.directory, left);
      if (left === 0 && oldest.directory !== own) {
        await rm(oldest.directory, { recursive: true, force: true }).catch(() => undefined);
      }
    }
    this.entries = [...kept, ...this.entries].sort(ordered);
  }

  /** Delete one file and record the removal in its session's index. */
  private async remove(entry: Entry): Promise<boolean> {
    try {
      await rm(entry.file, { force: true });
    } catch {
      return false;
    }
    const seq = Number.parseInt(path.basename(entry.file), 10);
    const index = path.join(entry.directory, INDEX);
    // Sessions without an index (written before it existed) are rescanned instead.
    if (Number.isFinite(seq) && (await stat(index).then(() => true, () => false))) {
      await appendFile(index, `${JSON.stringify({ seq, removed: true })}\n`, { mode: 0o600 }).catch(() => undefined);
    }
    return true;
  }
}

function envNumber(name: string, fallback: number): number {
  const configured = Number(process.env[name] ?? fallback);
  return Number.isFinite(configured) && configured >= 0 ? configured : fallback;
}

export const RESPONSE_ROOT = path.join(FREELLAMA_HOME, "responses");

export const responseStore = new ResponseStore(
  RESPONSE_ROOT,
  envNumber("FREELLAMA_RESPONSE_STORE_BYTES", 1024 ** 3),
  new Date(),
  envNumber("FREELLAMA_RESPONSE_TTL_DAYS", 7),
);
