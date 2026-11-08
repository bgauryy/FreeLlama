// sweep: hand a local model a large, divisible job (a code tree, a docs site) as many small
// chunks, judged slowly and durably, so the calling agent reads only what was flagged.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { DEFAULT_SERVE_ENDPOINT } from "../config.js";
import { endpointParam, errorResult, objectResultSchema, structuredResult } from "../helpers.js";
import { runWithJobProgress } from "../job-progress.js";
import { progressSink, type ToolExtra } from "../progress.js";
import { defaultFlags, SweepRunner, type Executor, type SweepView } from "../sweep/runner.js";
import { SweepStore, type ChunkResult, type SweepConfig } from "../sweep/store.js";

type Fields = Record<string, unknown>;

/** Chunk sizes a decision model judges well: about 150 to 1,000 tokens of code or prose. */
const DEFAULT_CHUNK_BYTES = 3_000;
const MAX_WAIT_SECONDS = 50;

function executorFor(endpoint: string | undefined): Executor {
  const target = endpoint ?? DEFAULT_SERVE_ENDPOINT;
  return async (request, signal) => {
    const { runTaskRequest, getTaskJob, cancelTaskJob } = await import("../serve-client.js");
    const raw = await runWithJobProgress({
      submit: () => runTaskRequest(target, { ...request, defer: true }),
      get: (id) => getTaskJob(target, id),
      cancel: (id) => cancelTaskJob(target, id),
      sync: (refusal) => {
        throw new Error(JSON.stringify({ ...refusal, code: typeof refusal?.code === "string" ? refusal.code : "task_jobs_full", retry_safe: true }));
      },
    }, () => undefined, { signal });
    const receipt = JSON.parse(raw) as Fields;
    if (typeof receipt.error === "string") throw new Error(raw);
    return receipt;
  };
}

const store = new SweepStore();
const runners = new Map<string, SweepRunner>();
/** One runner per serve endpoint; it owns the sweeps started against that endpoint in this process. */
function runnerFor(endpoint: string | undefined): SweepRunner {
  const key = endpoint ?? "";
  let runner = runners.get(key);
  if (!runner) {
    runner = new SweepRunner(store, executorFor(endpoint));
    runners.set(key, runner);
  }
  return runner;
}

function runnerOf(id: string): SweepRunner {
  for (const runner of runners.values()) if (runner.isActive(id)) return runner;
  return runnerFor(store.manifest(id).config.endpoint);
}

function viewText(view: SweepView): string {
  const c = view.counts;
  const changes = c.reused + c.changed + c.added > 0 || c.removed > 0
    ? `; since baseline: ${c.added} new, ${c.changed} changed, ${c.reused} unchanged (reused), ${c.removed} removed`
    : "";
  const eta = view.etaSeconds !== undefined ? `, about ${Math.max(1, Math.round(view.etaSeconds / 60))} min left` : "";
  const next = view.phase === "running" || view.phase === "collecting"
    ? `sweep{action:"wait", sweepId:"${view.id}"} to follow it`
    : view.phase === "interrupted" || view.phase === "cancelled" || c.failed > 0
      ? `sweep{action:"resume", sweepId:"${view.id}"} continues it`
      : `sweep{action:"results", sweepId:"${view.id}"} lists flagged chunks`;
  return `sweep ${view.id} ${view.phase}${view.error ? ` (${view.error})` : ""}: ${c.done + c.failed}/${c.total} chunks${view.pages !== undefined ? ` from ${view.pages} pages` : ""}, ` +
    `${c.flagged} flagged, ${c.unsure} unsure, ${c.clear} clear, ${c.failed} failed${changes}${eta}. Next: ${next}.`;
}

function answerLine(answers: Fields | undefined): string {
  return Object.entries(answers ?? {}).map(([name, raw]) => {
    const answer = (raw ?? {}) as Fields;
    if (answer.type === "noul" && typeof answer.noul === "number") return `${name} ${answer.noul.toFixed(2)}`;
    if (answer.type === "choice") {
      const probabilities = (answer.probabilities ?? {}) as Fields;
      const p = probabilities[String(answer.choice)];
      return `${name} ${String(answer.choice)}${typeof p === "number" ? ` ${p.toFixed(2)}` : ""}`;
    }
    if (typeof answer.score === "number") return `${name} ${answer.score.toFixed(2)}`;
    return name;
  }).join(", ");
}

type Filter = "flagged" | "unsure" | "failed" | "changed" | "all";

export function registerSweepTool(server: McpServer): void {
  server.registerTool(
    "sweep",
    {
      description:
        "Use when: a large divisible check (code tree, logs, docs site) should run chunk by chunk on a local model in the background; " +
        "each result is saved and you read only flagged chunks. start returns at once; wait (<=50 s) follows it; results pages " +
        "flagged|unsure|failed|changed|all; resume continues after a restart; `baseline` reuses an earlier sweep for unchanged chunks. " +
        "Do not use when: a few items (run_task tasks). Returns: status; results cite path:Lstart-Lend or url#n.",
      inputSchema: z.object({
        action: z.enum(["start", "status", "wait", "results", "cancel", "resume", "list"]),
        sweepId: z.string().length(36).optional(),
        name: z.string().min(1).max(120).optional(),
        files: z.array(z.string().min(1)).min(1).max(2_000).optional(),
        urls: z.array(z.url({ protocol: /^https?$/ })).min(1).max(20).optional().describe("public pages; robots.txt honored"),
        crawl: z.object({ sameOrigin: z.boolean(), maxPages: z.number().int().max(200), maxDepth: z.number().int().max(5), include: z.string() })
          .partial().optional().describe("defaults: true, 20 pages, depth 1; include is a link regex"),
        model: z.string().min(1).optional().describe("the catalog's classify pick"),
        // Compact: the core validates each question the way run_task's decision does.
        questions: z.record(z.string(), z.object({ type: z.enum(["noul", "choice", "score"]) }).loose())
          .optional().describe("run_task decision questions, asked per chunk; noul flags at p>=0.5"),
        flags: z.array(z.object({ question: z.string(), when: z.string().optional(), atLeast: z.number().optional() }))
          .optional().describe('when: a choice option or "true"; atLeast: default 0.5'),
        verifyModel: z.string().min(1).optional().describe("re-asks flagged and unsure chunks"),
        explainModel: z.string().min(1).optional(),
        explainPrompt: z.string().min(1).optional().describe("with explainModel: a note per confirmed flag"),
        chunkBytes: z.number().int().min(500).max(16_000).optional().describe("default 3000"),
        maxParallelism: z.number().int().positive().max(4).optional().describe("default 1"),
        baseline: z.string().length(36).optional().describe("an earlier sweepId"),
        filter: z.enum(["flagged", "unsure", "failed", "changed", "all"]).optional(),
        cursor: z.string().min(1).optional(),
        endpoint: endpointParam,
      }).strict(),
      outputSchema: objectResultSchema,
      // Reads and writes only its own sweep records; crawls reach the internet (openWorldHint default).
      annotations: { destructiveHint: false },
    },
    async (args, extra: ToolExtra) => {
      try {
        const { action, sweepId } = args;
        const startOnly = ["name", "files", "urls", "crawl", "model", "questions", "flags", "verifyModel", "explainModel", "explainPrompt", "chunkBytes", "maxParallelism", "baseline"] as const;
        if (action !== "start") {
          const stray = startOnly.filter((name) => args[name] !== undefined);
          if (stray.length > 0) return errorResult(new Error(`${stray.map((name) => `\`${name}\``).join(", ")} apply to action "start" only.`));
        }
        if (action !== "results" && (args.filter !== undefined || args.cursor !== undefined)) {
          return errorResult(new Error("`filter` and `cursor` apply to action \"results\" only."));
        }
        if (action === "list") {
          const sweeps = store.list().map((id) => runnerOf(id).view(id))
            .sort((left, right) => right.startedAt.localeCompare(left.startedAt)).slice(0, 50);
          return structuredResult({ sweeps }, {
            text: sweeps.length === 0 ? "no sweeps yet" : sweeps.map((view) => `- ${view.name ?? "(unnamed)"}: ${viewText(view)}`).join("\n"),
          });
        }
        if (action === "start") return await start(args);
        if (!sweepId) return errorResult(new Error(`action "${action}" needs \`sweepId\` (sweep{action:"list"} shows them).`));
        if (!store.exists(sweepId)) return errorResult(new Error(`sweep ${sweepId} not found`));
        const runner = runnerOf(sweepId);
        if (action === "cancel") {
          const view = await runner.cancel(sweepId);
          return structuredResult(view as unknown as Fields, { text: viewText(view) });
        }
        if (action === "resume") {
          const view = runner.resume(sweepId);
          return structuredResult(view as unknown as Fields, { text: viewText(view) });
        }
        if (action === "wait") {
          const report = progressSink(extra);
          const unsubscribe = report ? runner.subscribe(sweepId, report) : () => undefined;
          try {
            await runner.waitFor(sweepId, MAX_WAIT_SECONDS * 1000, extra.signal);
          } finally {
            unsubscribe();
          }
        }
        if (action === "results") return results(sweepId, args.filter ?? "flagged", 50, args.cursor);
        const view = runner.view(sweepId);
        return structuredResult(view as unknown as Fields, { text: viewText(view) });
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}

type StartArgs = {
  name?: string; files?: string[]; urls?: string[];
  crawl?: { sameOrigin?: boolean; maxPages?: number; maxDepth?: number; include?: string };
  model?: string; questions?: Record<string, { type: "noul" | "choice" | "score"; instructions?: unknown; criteria?: unknown }>; flags?: SweepConfig["flags"];
  verifyModel?: string; explainModel?: string; explainPrompt?: string;
  chunkBytes?: number; maxParallelism?: number; baseline?: string; endpoint?: string;
};

async function start(args: StartArgs) {
  if ((args.files === undefined) === (args.urls === undefined)) return errorResult(new Error("start needs exactly one of `files` or `urls`."));
  if (args.crawl !== undefined && args.urls === undefined) return errorResult(new Error("`crawl` applies to `urls` only."));
  if (!args.model) return errorResult(new Error('start needs `model`: a decision model such as models{view:"catalog", job:"classify"} lists.'));
  if (!args.questions || Object.keys(args.questions).length === 0) {
    return errorResult(new Error('start needs `questions`, e.g. {"issue":{"type":"noul","instructions":"Does this chunk contain a bug?"}}.'));
  }
  if ((args.explainModel === undefined) !== (args.explainPrompt === undefined)) return errorResult(new Error("`explainModel` and `explainPrompt` go together."));
  const flags = args.flags ?? defaultFlags(args.questions as SweepConfig["questions"]);
  if (flags.length === 0) return errorResult(new Error("no yes/no (noul) question to flag on: add `flags` naming a choice question and the option that flags, e.g. [{question:\"kind\", when:\"bug\"}]."));
  if (flags.some((rule) => rule.atLeast !== undefined && (rule.atLeast < 0 || rule.atLeast > 1))) return errorResult(new Error("flags[].atLeast is a probability from 0 to 1."));
  const crawlLimits = args.crawl ?? {};
  if ((crawlLimits.maxPages !== undefined && crawlLimits.maxPages < 1) || (crawlLimits.maxDepth !== undefined && crawlLimits.maxDepth < 0)) {
    return errorResult(new Error("crawl.maxPages must be 1 to 200 and crawl.maxDepth 0 to 5."));
  }
  if (crawlLimits.include !== undefined) {
    try { new RegExp(crawlLimits.include); } catch { return errorResult(new Error(`crawl.include is not a valid regex: ${crawlLimits.include}`)); }
  }
  const unknown = flags.filter((rule) => !(rule.question in args.questions!)).map((rule) => rule.question);
  if (unknown.length > 0) return errorResult(new Error(`flags name unknown questions: ${unknown.join(", ")}`));
  const config: Omit<SweepConfig, "id" | "createdAt"> = {
    name: args.name, endpoint: args.endpoint, model: args.model, questions: args.questions as SweepConfig["questions"], flags,
    unsureMargin: 0.15, verify: args.verifyModel ? { model: args.verifyModel } : undefined,
    explain: args.explainModel ? { model: args.explainModel, prompt: args.explainPrompt!, maxTokens: 160 } : undefined,
    priority: "background", maxParallelism: args.maxParallelism ?? 1, chunkBytes: args.chunkBytes ?? DEFAULT_CHUNK_BYTES,
    baseline: args.baseline,
    source: args.files
      ? { kind: "files", files: args.files }
      : { kind: "urls", urls: args.urls!, sameOrigin: args.crawl?.sameOrigin ?? true, maxPages: args.crawl?.maxPages ?? 20,
        maxDepth: args.crawl?.maxDepth ?? 1, include: args.crawl?.include },
  };
  const view = await runnerFor(args.endpoint).start(config);
  return structuredResult(view as unknown as Fields, { text: viewText(view) });
}

function results(id: string, filter: Filter, limit: number, cursor: string | undefined) {
  const manifest = store.manifest(id);
  const latest = store.results(id);
  const offset = cursor ? Number.parseInt(cursor, 10) : 0;
  if (!Number.isInteger(offset) || offset < 0) return errorResult(new Error("cursor must be the next_cursor of an earlier page"));
  const keep = (result: ChunkResult | undefined) => {
    if (!result) return false;
    const verdict = result.verified?.verdict ?? result.verdict;
    if (filter === "all") return true;
    if (filter === "failed") return result.status === "failed";
    if (filter === "changed") return result.change === "new" || result.change === "changed";
    return result.status === "done" && verdict === filter;
  };
  const rows = manifest.chunks.filter((chunk) => keep(latest.get(chunk.id))).map((chunk) => {
    const result = latest.get(chunk.id)!;
    return {
      label: chunk.label, source: chunk.source, lines: chunk.lines, status: result.status,
      verdict: result.verified?.verdict ?? result.verdict, triage: result.verdict, change: result.change, reused: result.reused,
      answers: result.answers, verified: result.verified, note: result.note, error: result.error,
    };
  });
  const page = rows.slice(offset, offset + limit);
  const next = offset + limit < rows.length ? String(offset + limit) : undefined;
  const text = page.length === 0
    ? `no ${filter} chunks in sweep ${id}`
    : page.map((row) => {
      const verified = row.verified ? ` → ${row.verified.model}: ${row.verified.verdict} (${answerLine(row.verified.answers)})` : "";
      const change = row.change ? ` [${row.change}]` : "";
      return `- ${row.label}${change}: ${row.status === "failed" ? `failed: ${(row.error ?? "").slice(0, 200)}` : `${row.triage} (${answerLine(row.answers)})${verified}`}${row.note ? `\n  note: ${row.note.replace(/\s+/g, " ").slice(0, 400)}` : ""}`;
    }).join("\n") + (next ? `\n(${rows.length - offset - page.length} more: cursor "${next}")` : "") +
      "\nThese are a small model's candidates: read 3 to 5 of the cited ranges yourself before relying on the rest.";
  return structuredResult({ sweep_id: id, filter, total: rows.length, chunks: page, page: { next_cursor: next } }, { text });
}
