// The sweep loop: a small local model judges every chunk ("triage"), a second model re-checks what
// it flagged or was unsure of ("verify"), and flagged chunks get a one-paragraph note ("explain").
// Every result is saved before the next chunk starts, so the work survives restarts; unchanged
// chunks reuse a baseline sweep's results, which is how a re-run reports only what happened since.
import { randomUUID } from "node:crypto";
import { collectFiles, crawl, type CollectedChunk } from "./sources.js";
import {
  hashText, SweepStore, type Change, type Chunk, type ChunkResult, type FlagRule, type Manifest, type SweepConfig, type Verdict,
} from "./store.js";

type Fields = Record<string, unknown>;
const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? value as Fields : undefined;

/** Runs one managed task and returns its receipt, or throws an Error whose message is serve's JSON error. */
export type Executor = (request: Fields, signal: AbortSignal) => Promise<Fields>;

/** Waits between attempts: transient failures retry quickly, memory refusals wait for memory. */
const RETRY_MS = [2_000, 8_000];
const MEMORY_RETRY_MS = [30_000, 60_000, 120_000];

/** Default rules: every yes/no question flags a chunk when its answer is probably yes. */
export function defaultFlags(questions: SweepConfig["questions"]): FlagRule[] {
  return Object.entries(questions).filter(([, question]) => question.type === "noul").map(([name]) => ({ question: name, when: "true", atLeast: 0.5 }));
}

/** Probability that a decision answer gives `when` ("true" for yes/no questions). */
function probability(answer: Fields | undefined, when: string): number | undefined {
  if (!answer) return undefined;
  if (answer.type === "noul" && typeof answer.noul === "number") return when === "false" ? 1 - answer.noul : answer.noul;
  const probabilities = object(answer.probabilities);
  const value = probabilities?.[when];
  return typeof value === "number" ? value : undefined;
}

/**
 * One verdict from a chunk's answers: flagged when any rule's probability reaches its threshold,
 * unsure when one falls within `margin` below it (or the answer is missing), clear otherwise.
 */
export function verdictOf(answers: Fields | undefined, flags: FlagRule[], margin: number): Verdict {
  let unsure = false;
  for (const rule of flags) {
    const p = probability(object(answers?.[rule.question]), rule.when ?? "true");
    const threshold = rule.atLeast ?? 0.5;
    if (p === undefined) {
      unsure = true;
      continue;
    }
    if (p >= threshold) return "flagged";
    if (p >= threshold - margin) unsure = true;
  }
  return unsure ? "unsure" : "clear";
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function errorBody(error: unknown): Fields | undefined {
  try {
    return object(JSON.parse(errorText(error)));
  } catch {
    return undefined;
  }
}

/** A refusal that waiting can fix: memory, queue or capacity, or a lost connection to serve. */
function retryable(error: unknown): "memory" | "transient" | undefined {
  const body = errorBody(error);
  const text = errorText(error);
  if (body?.code === "task_cancelled") return undefined;
  if (/memory|resource_admission|resources/i.test(String(body?.code ?? "")) || /needs [\d.]+ GB/.test(text)) return "memory";
  if (body?.retry_safe === true || /queue|capacity|timeout|ECONNREFUSED|error sending request|503|429/i.test(text)) return "transient";
  return undefined;
}

type Counters = { total: number; done: number; failed: number; flagged: number; unsure: number; clear: number; reused: number; changed: number; added: number };

export type SweepView = {
  id: string;
  name?: string;
  phase: string;
  model: string;
  counts: Counters & { pending: number; removed: number };
  pages?: number;
  skipped: number;
  error?: string;
  startedAt: string;
  finishedAt?: string;
  elapsedSeconds: number;
  etaSeconds?: number;
  files: ReturnType<SweepStore["files"]>;
};

type Active = { abort: AbortController; done: Promise<void>; ms: number[] };

export class SweepRunner {
  private readonly active = new Map<string, Active>();
  private readonly listeners = new Map<string, Set<(message: string) => void>>();

  constructor(private readonly store: SweepStore, private readonly execute: Executor, private readonly wait = (ms: number, signal: AbortSignal) => sleep(ms, signal)) {}

  /** Collect the chunks of `config.source`, save the sweep, and start judging in the background. */
  async start(input: Omit<SweepConfig, "id" | "createdAt">): Promise<SweepView> {
    const config: SweepConfig = { ...input, id: randomUUID(), createdAt: new Date().toISOString() };
    if (config.baseline && !this.store.exists(config.baseline)) throw new Error(`baseline sweep ${config.baseline} not found`);
    const manifest: Manifest = { config, phase: "collecting", chunks: [], skipped: [], startedAt: config.createdAt };
    const add = (collected: CollectedChunk[]) => {
      for (const item of collected) {
        const chunk: Chunk = { id: `c${String(manifest.chunks.length + 1).padStart(5, "0")}`, label: item.label, source: item.source,
          lines: item.lines, hash: hashText(item.text), bytes: Buffer.byteLength(item.text) };
        manifest.chunks.push(chunk);
        this.store.addChunk(config.id, chunk, item.text);
      }
    };
    if (config.source.kind === "files") {
      // Files are collected before returning, so a bad path or an empty glob fails the call itself.
      const { chunks, skipped } = collectFiles(config.source.files, config.chunkBytes);
      if (chunks.length === 0) throw new Error(`no text to sweep${skipped.length ? `: ${skipped.map((item) => `${item.source} (${item.reason})`).slice(0, 5).join("; ")}` : ""}`);
      this.store.create(manifest);
      add(chunks);
      manifest.skipped = skipped;
      manifest.phase = "running";
      this.store.saveManifest(manifest);
    } else {
      this.store.create(manifest);
    }
    this.launch(manifest);
    return this.view(config.id);
  }

  /** Continue a sweep that was cancelled, interrupted by a restart, or left failed chunks. */
  resume(id: string): SweepView {
    if (this.active.has(id)) return this.view(id);
    const manifest = this.store.manifest(id);
    if (manifest.phase === "collecting") throw new Error("this sweep was interrupted while collecting pages; start a new sweep");
    manifest.phase = "running";
    delete manifest.finishedAt;
    delete manifest.error;
    this.store.saveManifest(manifest);
    this.launch(manifest, true);
    return this.view(id);
  }

  async cancel(id: string): Promise<SweepView> {
    const active = this.active.get(id);
    if (active) {
      active.abort.abort();
      await active.done.catch(() => undefined);
    }
    return this.view(id);
  }

  isActive(id: string) {
    return this.active.has(id);
  }

  /** Progress messages for one sweep while `listener` is subscribed. */
  subscribe(id: string, listener: (message: string) => void): () => void {
    const set = this.listeners.get(id) ?? new Set();
    set.add(listener);
    this.listeners.set(id, set);
    return () => set.delete(listener);
  }

  /** Resolves when the sweep stops, or after `ms`. */
  async waitFor(id: string, ms: number, signal?: AbortSignal): Promise<void> {
    const active = this.active.get(id);
    if (!active) return;
    let timer: NodeJS.Timeout | undefined;
    await Promise.race([
      active.done.catch(() => undefined),
      new Promise<void>((resolve) => {
        timer = setTimeout(resolve, ms);
        signal?.addEventListener("abort", () => resolve(), { once: true });
      }),
    ]);
    clearTimeout(timer);
  }

  view(id: string): SweepView {
    const manifest = this.store.manifest(id);
    const results = this.store.results(id);
    const counts: Counters = { total: manifest.chunks.length, done: 0, failed: 0, flagged: 0, unsure: 0, clear: 0, reused: 0, changed: 0, added: 0 };
    for (const result of results.values()) {
      if (result.status === "failed") {
        counts.failed += 1;
        continue;
      }
      counts.done += 1;
      const verdict = result.verified?.verdict ?? result.verdict;
      if (verdict) counts[verdict] += 1;
      if (result.reused) counts.reused += 1;
      if (result.change === "changed") counts.changed += 1;
      if (result.change === "new") counts.added += 1;
    }
    // A sweep this process is not running cannot still be running: the server restarted mid-sweep.
    const phase = (manifest.phase === "running" || manifest.phase === "collecting") && !this.active.has(id) ? "interrupted" : manifest.phase;
    const elapsedSeconds = Math.round(((manifest.finishedAt ? Date.parse(manifest.finishedAt) : Date.now()) - Date.parse(manifest.startedAt)) / 1000);
    const pending = Math.max(0, counts.total - counts.done - counts.failed);
    const ms = this.active.get(id)?.ms ?? [];
    const average = ms.length > 0 ? ms.reduce((sum, value) => sum + value, 0) / ms.length : undefined;
    return {
      id, name: manifest.config.name, phase, model: manifest.config.model,
      counts: { ...counts, pending, removed: manifest.removed?.length ?? 0 },
      pages: manifest.pages, skipped: manifest.skipped.length, error: manifest.error,
      startedAt: manifest.startedAt, finishedAt: manifest.finishedAt, elapsedSeconds,
      etaSeconds: phase === "running" && average !== undefined ? Math.round((pending * average) / 1000 / Math.max(1, manifest.config.maxParallelism)) : undefined,
      files: this.store.files(id),
    };
  }

  private emit(id: string, message: string) {
    for (const listener of this.listeners.get(id) ?? []) listener(message);
  }

  private launch(manifest: Manifest, resuming = false) {
    const abort = new AbortController();
    const active: Active = { abort, done: Promise.resolve(), ms: [] };
    this.active.set(manifest.config.id, active);
    active.done = this.run(manifest, abort.signal, active, resuming)
      .catch((error: unknown) => {
        manifest.phase = abort.signal.aborted ? "cancelled" : "failed";
        if (!abort.signal.aborted) manifest.error = errorText(error);
      })
      .finally(() => {
        if (manifest.phase === "running" || manifest.phase === "collecting") manifest.phase = abort.signal.aborted ? "cancelled" : "completed";
        manifest.finishedAt = new Date().toISOString();
        this.store.saveManifest(manifest);
        this.active.delete(manifest.config.id);
        const view = this.view(manifest.config.id);
        this.emit(manifest.config.id, `sweep ${view.phase}: ${view.counts.flagged} flagged, ${view.counts.unsure} unsure, ${view.counts.failed} failed of ${view.counts.total}`);
      });
  }

  private async run(manifest: Manifest, signal: AbortSignal, active: Active, resuming: boolean) {
    const { config } = manifest;
    const id = config.id;
    if (config.source.kind === "urls") {
      const source = config.source;
      const { pages, skipped } = await crawl(source.urls, {
        sameOrigin: source.sameOrigin, maxPages: source.maxPages, maxDepth: source.maxDepth, include: source.include,
        chunkBytes: config.chunkBytes, signal,
        onPage: (url, chunks) => {
          for (const item of chunks) {
            const chunk: Chunk = { id: `c${String(manifest.chunks.length + 1).padStart(5, "0")}`, label: item.label, source: item.source,
              lines: item.lines, hash: hashText(item.text), bytes: Buffer.byteLength(item.text) };
            manifest.chunks.push(chunk);
            this.store.addChunk(id, chunk, item.text);
          }
          manifest.pages = (manifest.pages ?? 0) + 1;
          this.emit(id, `fetched ${url} (${chunks.length} chunks)`);
        },
      });
      manifest.pages = pages;
      manifest.skipped = skipped;
      if (signal.aborted) return;
      if (manifest.chunks.length === 0) throw new Error(`no pages fetched${skipped.length ? `: ${skipped.slice(0, 5).map((item) => `${item.source} (${item.reason})`).join("; ")}` : ""}`);
      manifest.phase = "running";
      this.store.saveManifest(manifest);
    }

    const texts = this.store.chunkTexts(id);
    const done = this.store.results(id);
    const baseline = config.baseline ? this.baselineIndex(config.baseline) : undefined;
    if (baseline && !resuming) {
      // Files or pages the baseline read that are gone now.
      const current = new Set(manifest.chunks.map((chunk) => chunk.source));
      manifest.removed = [...baseline.sources].filter((source) => !current.has(source));
      this.store.saveManifest(manifest);
    }
    const pending = manifest.chunks.filter((chunk) => done.get(chunk.id)?.status !== "done");
    let finished = manifest.chunks.length - pending.length;
    const total = manifest.chunks.length;
    const step = async (chunk: Chunk) => {
      const text = texts.get(chunk.id) ?? "";
      const change: Change | undefined = baseline
        ? (baseline.byHash.has(`${chunk.source}\n${chunk.hash}`) ? "unchanged" : baseline.sources.has(chunk.source) ? "changed" : "new")
        : undefined;
      const prior = baseline?.byHash.get(`${chunk.source}\n${chunk.hash}`);
      const result = prior && sameJudges(prior.config, config)
        ? { ...prior.result, chunk: chunk.id, change, reused: true, at: new Date().toISOString() }
        : { ...await this.judge(config, chunk, text, signal, active), ...(change ? { change } : {}) };
      if (signal.aborted && result.status === "failed") return;
      this.store.addResult(id, result);
      finished += 1;
      const verdict = result.verified?.verdict ?? result.verdict;
      this.emit(id, `${finished}/${total} ${chunk.label}: ${result.status === "failed" ? `failed (${(result.error ?? "").slice(0, 120)})` : `${verdict}${result.reused ? " (unchanged, reused)" : ""}`}`);
    };
    const queue = [...pending];
    await Promise.all(Array.from({ length: Math.max(1, Math.min(config.maxParallelism, queue.length)) }, async () => {
      while (queue.length > 0 && !signal.aborted) await step(queue.shift()!);
    }));
    if (signal.aborted) throw new Error("cancelled");
  }

  private baselineIndex(id: string) {
    const manifest = this.store.manifest(id);
    const results = this.store.results(id);
    const byHash = new Map<string, { result: ChunkResult; config: SweepConfig }>();
    const sources = new Set<string>();
    for (const chunk of manifest.chunks) {
      sources.add(chunk.source);
      const result = results.get(chunk.id);
      if (result?.status === "done") byHash.set(`${chunk.source}\n${chunk.hash}`, { result, config: manifest.config });
    }
    return { byHash, sources };
  }

  /** Triage, then verify what was flagged or unsure, then explain what stays flagged. */
  private async judge(config: SweepConfig, chunk: Chunk, text: string, signal: AbortSignal, active: Active): Promise<ChunkResult> {
    const started = Date.now();
    let attempts = 0;
    const attempt = async <T>(work: () => Promise<T>): Promise<T> => {
      for (let memory = 0, transient = 0; ; ) {
        attempts += 1;
        try {
          return await work();
        } catch (error) {
          const kind = signal.aborted ? undefined : retryable(error);
          const delays = kind === "memory" ? MEMORY_RETRY_MS : RETRY_MS;
          const index = kind === "memory" ? memory++ : transient++;
          if (!kind || index >= delays.length) throw error;
          this.emit(config.id, `${chunk.label}: ${kind === "memory" ? "waiting for memory" : "retrying"} in ${delays[index]! / 1000} s`);
          await this.wait(delays[index]!, signal);
        }
      }
    };
    const decide = async (model: string) => {
      const receipt = await attempt(() => this.execute({
        task: "decision", objective: "balanced", model, state: `Source: ${chunk.label}\n\n${text}`, questions: config.questions,
        priority: config.priority, keep_alive: "5m",
      }, signal));
      const answers = object(object(receipt.response)?.answers);
      if (!answers) throw new Error(`${model} returned no decision answers`);
      return answers;
    };
    try {
      const answers = await decide(config.model);
      const verdict = verdictOf(answers, config.flags, config.unsureMargin);
      const result: ChunkResult = { chunk: chunk.id, status: "done", verdict, answers, attempts, model: config.model, ms: 0, at: "" };
      if (config.verify && verdict !== "clear") {
        const verified = await decide(config.verify.model);
        result.verified = { model: config.verify.model, verdict: verdictOf(verified, config.flags, config.unsureMargin), answers: verified };
      }
      if (config.explain && (result.verified?.verdict ?? verdict) === "flagged") {
        const receipt = await attempt(() => this.execute({
          task: "completion", objective: "balanced", model: config.explain!.model, priority: config.priority, keep_alive: "5m",
          messages: [
            ...(config.systemPrompt ? [{ role: "system", content: config.systemPrompt }] : []),
            { role: "user", content: `${config.explain!.prompt}\n\nSource: ${chunk.label}\n\n${text}` },
          ],
          request_options: { think: false, options: { temperature: 0, num_predict: config.explain!.maxTokens } },
        }, signal));
        const content = object(object(receipt.response)?.message)?.content;
        if (typeof content === "string") result.note = content.trim();
      }
      result.ms = Date.now() - started;
      result.at = new Date().toISOString();
      active.ms.push(result.ms);
      if (active.ms.length > 50) active.ms.shift();
      return result;
    } catch (error) {
      return { chunk: chunk.id, status: "failed", error: errorText(error).slice(0, 2_000), attempts, model: config.model, ms: Date.now() - started, at: new Date().toISOString() };
    }
  }
}

/** A baseline result can be reused only when the same models answered the same questions. */
function sameJudges(left: SweepConfig, right: SweepConfig) {
  return left.model === right.model && JSON.stringify(left.questions) === JSON.stringify(right.questions)
    && JSON.stringify(left.flags) === JSON.stringify(right.flags) && left.verify?.model === right.verify?.model
    && left.explain?.model === right.explain?.model && left.explain?.prompt === right.explain?.prompt;
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve();
    const timer = setTimeout(resolve, ms);
    signal.addEventListener("abort", () => {
      clearTimeout(timer);
      resolve();
    }, { once: true });
  });
}
