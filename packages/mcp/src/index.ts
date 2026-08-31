#!/usr/bin/env node
/**
 * MCP server exposing FreeLlama's local-LLM control plane as tools.
 *
 * PURPOSE: lets a large orchestrating model offload HEAVY, well-scoped, NON-reasoning work
 * (bulk file reads, structural search, embeddings, plain completions) to a cheap local Ollama
 * model. Not a general-purpose standalone MCP, not a substitute for the orchestrator's judgment —
 * give the local model a narrow, guided instruction, never an open-ended one (see accuracy split
 * in the decision guide below).
 *
 * Every tool below is a thin wrapper around the native NAPI binding compiled from
 * `../../packages/rust-core/src/napi.rs` (loaded via `../native/index.js`, which itself just re-exports the compiled
 * `../native/freellama.darwin-arm64.node`). Build output lands inside this package (not the repo
 * root) so `packages/mcp/` is self-contained and publishable on its own. The Rust side does the real
 * work — this file only defines the MCP tool schema/description and forwards arguments straight
 * through.
 *
 * `doctor` works standalone against Ollama. `machine`/`listModels`/`route`/`runTask` require a
 * running `freellama serve` instance (default
 * http://127.0.0.1:11435) — they will return a clear connection error otherwise, they won't hang.
 *
 * DECISION GUIDE (measured, not estimated):
 *   - Fact from this codebase (where/what/find) -> `delegate_research`. 1-file lookup: 4,584 in /
 *     296 out local tokens, ~220 returned (95% offloaded). 3-file cross-ref: 26,298 in / 849 out
 *     local, ~480 returned (98%). Accuracy: 98.9% grounded lookups, only ~67% judgment calls —
 *     never delegate judgment unsupervised.
 *   - Ollama needs to actually run something (prompt/chat/embedding/vision) -> `run_task`. Output
 *     tokens land on the local model; orchestrator pays only the JSON wrapper.
 *   - Just need to know which model WOULD be picked -> `route`. Free, no local generation.
 *   - Already-known one-liner -> don't delegate; the ~10-100s round trip costs more than it saves.
 */
import { type ChildProcess, execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import {
  REPO_ROOT,
  RESEARCH_ADAPTERS,
  type ResearchAdapter,
  DEFAULT_RESEARCH_ADAPTER,
  DEFAULT_SERVE_ENDPOINT,
  DEFAULT_DELEGATE_MODEL,
  DEFAULT_DELEGATE_MAX_TURNS,
  DEFAULT_DELEGATE_TIMEOUT_SECONDS,
  DEFAULT_PULL_TIMEOUT_SECONDS,
  DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS,
  assertAllowedWorkspace,
} from "./config.js";
import { doctor, machine, listModels, route, runTask, SERVER_VERSION } from "./native.js";
import {
  ollamaFetch,
  endpointParam,
  ollamaEndpointParam,
  taskParam,
  objectiveParam,
  minConfidenceParam,
  belowConfidence,
  requiredCapabilitiesParam,
  clipText,
  structuredResult,
  parsedResult,
  errorResult,
  summarizeEmbeddings,
} from "./helpers.js";
import { parseModelSearch, parseModelTags } from "./model-search.js";
import { MODEL_EVIDENCE, assessDelegatedAnswer } from "./delegate.js";

const execFileAsync = promisify(execFile);

// `delegate_research` spawns a python subprocess that can run for minutes. If this server goes
// away first — client disconnect, Ctrl-C, a supervisor restart — an untracked child keeps a local
// model pinned in VRAM with nothing left to return its answer to. Track every live child and take
// it down with the server.
const liveDelegates = new Set<ChildProcess>();

function killLiveDelegates(): void {
  for (const child of liveDelegates) {
    try {
      child.kill("SIGKILL");
    } catch {
      // Already gone; nothing to clean up.
    }
  }
  liveDelegates.clear();
}

// A client that disappears mid-response leaves the stdio transport writing to a closed pipe.
// Node surfaces that as an unhandled 'error' event on the socket, which killed the whole process
// with a stack trace on stderr (observed live). There is nothing to recover — the client is gone —
// but it should exit quietly through the normal path so the cleanup below still runs and a real
// error isn't buried in an EPIPE trace.
for (const stream of [process.stdout, process.stderr] as const) {
  stream.on("error", (error: NodeJS.ErrnoException) => {
    if (error.code === "EPIPE") process.exit(0);
    throw error;
  });
}

process.on("exit", killLiveDelegates);
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
  process.on(signal, () => {
    killLiveDelegates();
    process.exit(0);
  });
}


// Guidance that applies ACROSS tools lives here, not repeated in each description. Measured
// motivation: the tool list is re-sent on every request — it was 7,431 tokens across 13 tools,
// dwarfing anything a single delegated call saves. Shared caveats stated once here, tool-specific
// facts in the descriptions.
const INSTRUCTIONS = `Offload token-heavy, non-reasoning work to local Ollama models.
Optimise for quality and token reduction; latency is the tiebreak.
For the full orchestration playbook — the five flows, the tiering across you / a cheap cloud model /
local Ollama, and what each tier must never be given — load the \`freellama\` skill (in this repo:
skills/freellama/SKILL.md).

DELEGATE WHEN
A delegated answer costs a roughly fixed ~150 tokens whatever the input size, so past ~1k tokens
of source it already wins on tokens.
Hard rules:
- Use a model measured strong for research; accuracy collapses on small ones. Never trade model
  size for speed — a cheap wrong answer costs more than the tokens it saved. Installed models
  differ per machine, so call models{view:"installed"} rather than assuming a name.
- Judgment work (review, "is this good", design): do it yourself. Local models are markedly worse
  at it than at grounded lookups, and the tone is identical either way — the text gives no signal.
- Source under ~1k tokens and you are waiting on it: just read it.
- Privacy-bound or rate-limited: delegate regardless.

CHEAPEST LOCAL WORK, best first
1. Embeddings via run_task — fast, zero tokens back to you, and no sampling
   so nothing to hallucinate. Index a corpus, then search it. By far the strongest use.
2. Image work — accurate but not fast; see IMAGES for which model to name.
3. Bulk transforms you do not block on.
4. Single questions — see the rules above.
Never use a local model to pick which files are relevant: it is slower and less accurate than grep.

TRUST
Pass minConfidence:"medium" to route/run_task to refuse a weakly-justified model instead of acting
on it. Without a configured policy most routes grade "low" — which is how a far-too-small model
once got selected for a demanding task. Read the verification verdict on every delegate_research answer — it is
computed from what the run actually did and which model ran, never from the model's own claim.
"escalate" means it answered without reading anything.

IMAGES
qwen3.8:27b-mlx and muse-glimmer:30b-mlx both do real vision — UI and chart description, and
accurate transcription. ~17-45s per image. qwen is the faster and more accurate of the two.
The OCR-only and broken vision models were removed, so routing can no longer land on a bad one,
but naming the model explicitly is still the safe habit.`;

const server = new McpServer(
  { name: "freellama", version: SERVER_VERSION },
  { instructions: INSTRUCTIONS },
);



server.registerTool(
  "doctor",
  {
    description:
      "Ollama health + the 9 OLLAMA_* settings that govern memory, each with its EFFECTIVE default " +
      "(unset means Ollama picks, not off). Warns if MAX_LOADED_MODELS is unset — the cap is then " +
      "3 x GPU count, and 3 large models don't fit in 48GB. Adds chip/RAM/disk when serve is up. " +
      "Run FIRST on any error. Free",
    inputSchema: { endpoint: ollamaEndpointParam, serveEndpoint: endpointParam },
    // Reads Ollama's version/ps/env endpoints; changes nothing. `openWorldHint` because the
    // answer depends on a separate process this server does not own.
    // Only deviations from the spec defaults are declared; restating a default says nothing.
    // rest = spec defaults
    annotations: { readOnlyHint: true },
  },
  async ({ endpoint, serveEndpoint }) => {
    try {
      const report = parsedResult(await doctor(endpoint));
      if (!("structuredContent" in report)) return report;
      // Absorbed the former `machine` tool. Attempted, not required: `doctor` must keep working
      // with no `freellama serve` running, because the Ollama half of the diagnostic is exactly
      // the half you need when things are broken. A failure degrades to a stated reason.
      try {
        report.structuredContent.machine = JSON.parse(await machine(serveEndpoint));
      } catch (error) {
        report.structuredContent.machine = null;
        report.structuredContent.machine_unavailable =
          `freellama serve unreachable, so no machine profile: ${error instanceof Error ? error.message : String(error)}`;
      }
      return structuredResult(report.structuredContent);
    } catch (error) {
      return errorResult(error);
    }
  },
);

server.registerTool(
  "models",
  {
    description:
      "Local model estate. `installed` (default, needs serve): capabilities, VRAM, context, " +
      "policy_rank — what route's requiredCapabilities filters against. `resident`: loaded now + derived " +
      "GPU/CPU split; check before any large call, two big models crashed a 48GB box here, and a " +
      "placement.warning means it spilled to CPU (many times slower, no error). `detail` (needs " +
      "`model`): true max context, quantization. `raw`: GET /api/tags. Views differ by design",
    inputSchema: {
      view: z
        .enum(["installed", "resident", "detail", "raw"])
        .optional()
.describe('"installed" (default, needs serve) | "resident" | "detail" | "raw"'),
      model: z.string().optional().describe('required for view "detail"'),
      includeVerbose: z
        .boolean()
        .optional()
.describe('"detail" only. Adds license/modelfile — the bulk of that payload, never routing-relevant'),
      endpoint: endpointParam,
      ollamaEndpoint: ollamaEndpointParam,
    },
    // Permissive by necessity: the list views return {models:[...]}, `detail` returns a flat
    // model record. One schema covers both rather than splitting a read-only tool back apart.
    // Only deviations from the spec defaults are declared; restating a default says nothing.
    // rest = spec defaults
    annotations: { readOnlyHint: true },
  },
  async ({ view, model, includeVerbose, endpoint, ollamaEndpoint }) => {
    try {
      switch (view ?? "installed") {
        case "raw":
          return structuredResult((await ollamaFetch(ollamaEndpoint, "/api/tags")) as Record<string, unknown>);

        case "resident": {
          const data = (await ollamaFetch(ollamaEndpoint, "/api/ps")) as {
            models?: Array<Record<string, unknown>>;
          };
          // Ollama's own docs say to check the GPU/CPU split, but /api/ps exposes only the raw
          // `size`/`size_vram` bytes it is derived from. The CLI computes it; the API doesn't.
          const models = (data.models ?? []).map((entry) => {
            const size = typeof entry.size === "number" ? entry.size : null;
            const vram = typeof entry.size_vram === "number" ? entry.size_vram : null;
            if (size === null || vram === null || size === 0) return entry;
            const gpuPercent = Math.round((vram / size) * 100);
            return {
              ...entry,
              placement: {
                gpu_percent: gpuPercent,
                processor:
                  gpuPercent >= 100
                    ? "100% GPU"
                    : gpuPercent <= 0
                      ? "100% CPU"
                      : `${gpuPercent}% GPU / ${100 - gpuPercent}% CPU`,
                ...(gpuPercent < 100
                  ? {
                      warning:
                        "Partially offloaded to CPU — expect a large slowdown. Free VRAM (`ollama_stop`) or lower the context length.",
                    }
                  : {}),
              },
            };
          });
          return structuredResult({ ...data, models });
        }

        case "detail": {
          if (!model) {
            return errorResult(new Error('view "detail" requires a `model` argument, e.g. {view:"detail", model:"qwen3.8:27b-mlx"}.'));
          }
          const data = (await ollamaFetch(ollamaEndpoint, "/api/show", {
            method: "POST",
            body: { model },
          })) as Record<string, unknown>;
          const { license, modelfile, ...rest } = data;
          // The real ceiling hides under a per-architecture key (`qwen3_5.context_length`,
          // `llama.context_length`, ...), so it cannot be read by a fixed path.
          const modelInfo = (rest.model_info ?? {}) as Record<string, unknown>;
          const contextEntry = Object.entries(modelInfo).find(
            ([key, value]) => key.endsWith(".context_length") && typeof value === "number",
          );
          return structuredResult({
            ...rest,
            max_context_length: (contextEntry?.[1] as number) ?? null,
            ...(includeVerbose ? { license, modelfile } : {}),
          });
        }

        default:
          return parsedResult(await listModels(endpoint));
      }
    } catch (error) {
      return errorResult(error);
    }
  },
);

server.registerTool(
  "route",
  {
    description:
      "Which model WOULD be picked, free — no generation. Skip it before `run_task`, which routes " +
      "internally. CAUTION: `fastest` with no configured policy ranks on capability metadata " +
      "alone, and picked a 0.5B model for code repair here; pass minConfidence:\"medium\" to " +
      "refuse that rather than act on it. Nothing eligible? `search_models` finds and size-checks " +
      "something to install. Needs serve",
    inputSchema: {
      endpoint: endpointParam,
      task: taskParam,
      objective: objectiveParam,
      model: z.string().optional().describe("Force this exact installed model name."),
      sessionId: z.string().optional().describe("Session id for model affinity across calls."),
      contextTokens: z.number().int().positive().optional().describe("Minimum context window required."),
      requiredCapabilities: requiredCapabilitiesParam,
      minConfidence: minConfidenceParam,
    },
    // NOT marked read-only, despite "decision only": passing `sessionId` makes the server bind
    // that session to the selected model (packages/rust-core/src/platform/mod.rs `route()` -> `sessions.write().bind`),
    // which is a real state change. Without `sessionId` it is pure computation, but annotations
    // are per-tool, not per-argument, so the honest answer is the conservative one. Additive
    // (never removes anything) and idempotent (re-binding the same pair is a no-op).
    // Only deviations from the spec defaults are declared; restating a default says nothing.
    // not readOnly: sessionId writes affinity
    annotations: { destructiveHint: false, idempotentHint: true },
  },
  async ({ endpoint, task, objective, model, sessionId, contextTokens, requiredCapabilities, minConfidence }) => {
    try {
      const result = parsedResult(
        // minConfidence is forwarded so the CORE gate refuses, with its actionable message naming
        // the two commands that raise the grade. The belowConfidence() check below stays only as a
        // fallback for servers older than the core gate.
        await route(endpoint, task, objective, model, sessionId, contextTokens, requiredCapabilities, minConfidence),
      );
      if ("structuredContent" in result) {
        const refusal = belowConfidence(result.structuredContent, minConfidence);
        if (refusal) return refusal;
      }
      return result;
    } catch (error) {
      return errorResult(error);
    }
  },
);


server.registerTool(
  "search_models",
  {
    description:
      "Browse the public Ollama library (ollama.com) for models NOT yet installed — the " +
      "complement to `models`, which only sees what is already installed.\n" +
      "TWO STEPS, both required before pulling anything:\n" +
      "1. SEARCH — omit `model`. Returns FAMILY names (e.g. \"gemma4\"). Popular-ordered by " +
      'default; `order:"newest"` is rarely wanted, a new model has no track record. Site rank is ' +
      "NOT pull count (a 26K-pull model can outrank a 1.1M one) — judge with `pulls`, not " +
      "position. `cloudOnly` means it runs on Ollama's HOSTED service and cannot run locally.\n" +
      "2. INSPECT — pass `model: \"<family>\"`. A family is NOT pullable; you pull a TAG, and only " +
      "the tag carries the size that decides whether it fits. Returns every tag with size, " +
      "context window, modalities, and `fitsInMemory` computed against this machine. NEVER pull " +
      "from step 1 alone — you would be guessing the size.\n" +
      "Downloads nothing; `ollama_manage` action \"pull\" does that, only after a human approves.",
    inputSchema: {
      model: z
        .string()
        .optional()
        .describe('step 2: a family name from step 1, e.g. "gemma4" — returns its pullable tags'),
      query: z.string().optional().describe('step 1: free text, e.g. "qwen", "embed"'),
      capabilities: z
        .array(z.enum(["vision", "tools", "thinking", "embedding", "cloud"]))
        .optional()
        .describe("filter chips; combined as AND by the site"),
      order: z
        .enum(["popular", "newest"])
        .optional()
        .describe('default "popular" — prefer it, "newest" has no track record'),
      limit: z.number().int().positive().max(50).optional().describe("default 10"),
    },
    // Only deviations from the spec defaults are declared; restating a default says nothing.
    // reads a public page; changes nothing
    annotations: { readOnlyHint: true },
  },
  async ({ model, query, capabilities, order, limit }) => {
    const fetchPage = async (url: string) => {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS * 1000);
      try {
        const response = await fetch(url, { signal: controller.signal });
        if (!response.ok) throw new Error(`ollama.com returned HTTP ${response.status}`);
        return await response.text();
      } finally {
        clearTimeout(timer);
      }
    };
    // Best-effort local context. Neither failure should sink the lookup: the library is public and
    // useful even with Ollama down.
    const localState = async () => {
      let installed = new Set<string>();
      let memoryBytes: number | null = null;
      try {
        const tags = (await ollamaFetch(undefined, "/api/tags")) as { models?: Array<{ name?: string }> };
        installed = new Set((tags.models ?? []).map((m) => m.name ?? ""));
      } catch {
        /* Ollama unreachable */
      }
      try {
        memoryBytes = JSON.parse(await machine(undefined)).unified_memory_bytes ?? null;
      } catch {
        /* serve unreachable */
      }
      return { installed, memoryBytes };
    };

    try {
      if (model) {
        const family = model.split(":")[0];
        const familyPage = await fetchPage(`https://ollama.com/library/${family}`);
        const { tags } = parseModelTags(familyPage, family);
        const { installed, memoryBytes } = await localState();
        // "Fits" is not "size < RAM". A model needs room for its KV cache and for whatever else is
        // resident, and this machine has crashed by co-residenting two large models. 60% of total
        // memory is the ceiling used here for a comfortable single-model fit.
        const budget = memoryBytes ? memoryBytes * 0.6 : null;
        const annotated = tags.map((t) => ({
          ...t,
          installed: installed.has(t.tag),
          fitsInMemory: budget && t.sizeBytes ? t.sizeBytes <= budget : null,
        }));
        // Fail CLOSED on an unknown budget. With `serve` unreachable there is no machine profile,
        // so `fitsInMemory` is null for every tag — and "largest tag that isn't known not to fit"
        // then happily recommends a 143GB model on a 48GB machine. A fit recommendation without a
        // memory figure is worse than none, so only rank when the budget is actually known.
        const runnable = annotated.filter((t) => t.fitsInMemory === true && !/cloud/.test(t.tag));
        // "Bigger is better" is a *generative* rule and applying it to an embedding family
        // inverted this repo's own measurement. Measured here on real retrieval over this repo
        // (152 chunks, 6 questions with known-correct files): nomic-embed-text at 274MB scored
        // 5/6 recall@3 in 4.2s, while qwen3-embedding:0.6b — 2.3x the size — scored 4/6 at 3.5x
        // the indexing cost. Recommending the largest embedding tag that fits therefore pointed
        // at a 4.7GB model, ~17x the measured winner, for worse recall. Embeddings have no
        // accuracy cliff to clear: there is no sampling, so the size premium buys nothing.
        // Two independent signals, because either alone misfires: a generative model's blurb can
        // mention embeddings in passing (so require several hits — measured 82 on
        // /library/qwen3-embedding versus 0 on /library/gemma3), and not every embedding family
        // is named for it (`all-minilm` is not).
        const embeddingMentions = (familyPage.match(/embedding/gi) ?? []).length;
        const isEmbeddingFamily = embeddingMentions >= 3 || /embed/i.test(family);
        const sized = runnable.filter((t) => t.sizeBytes);
        const best = budget
          ? isEmbeddingFamily
            ? sized.sort((a, b) => (a.sizeBytes ?? 0) - (b.sizeBytes ?? 0))[0]
            : sized.sort((a, b) => (b.sizeBytes ?? 0) - (a.sizeBytes ?? 0))[0]
          : undefined;
        return structuredResult({
          family,
          page: `https://ollama.com/library/${family}`,
          // Zero tags is ambiguous on its own — the family may be cloud-only (its page renders no
          // downloadable rows), the name may be wrong, or Ollama may have changed its markup.
          // Say so rather than returning a bare empty list that reads like "nothing available".
          ...(tags.length === 0
            ? {
                tagsUnavailable:
                  `No pullable tags found for "${family}". Either it is cloud-only (no local ` +
                  "download), the family name is wrong, or ollama.com changed its markup. Open " +
                  "the page above to check before concluding the model does not exist.",
              }
            : {}),
          machineMemoryBytes: memoryBytes,
          fitBudgetBytes: budget,
          tags: annotated,
          // Stated, not silent: a caller must be able to tell "nothing fits" from "I could not check".
          recommendationUnavailable: budget
            ? undefined
            : "No machine profile (freellama serve unreachable), so memory fit could not be checked and no tag is recommended. Start serve, or read the sizes yourself.",
          recommendation: best
            ? isEmbeddingFamily
              ? {
                  tag: best.tag,
                  why:
                    "Smallest tag that fits — this is an embedding family, where bigger is NOT " +
                    "better. Measured on real retrieval over this repo: nomic-embed-text (274MB) " +
                    "scored 5/6 recall@3 in 4.2s, while qwen3-embedding:0.6b (2.3x the size) " +
                    "scored 4/6 at 3.5x the indexing cost. Embeddings do no sampling, so there is " +
                    "no accuracy cliff for size to buy you past.",
                  configure:
                    'Batch your inputs in one `run_task` call and use keepAlive:"0" — an embedding ' +
                    "model has no reason to hold memory after the vectors are computed. Leave " +
                    "returnEmbeddings off unless you are storing the vectors yourself.",
                  caution:
                    "Index once, query many times, and own the staleness: FreeLlama stores no " +
                    "vectors, and a stale index fails silently by returning confidently wrong " +
                    "files. Also, if you know the keyword, grep beat embedding search here on " +
                    "accuracy, latency and cost at the same time — reach for embeddings when " +
                    "there is no keyword to search for.",
                }
              : {
                  tag: best.tag,
                  why:
                    `Largest tag that fits the ~60% memory budget${memoryBytes ? ` of ${Math.round(memoryBytes / 1e9)}GB` : ""}. ` +
                    "Bigger is better here — research accuracy collapses below ~12B.",
                  configure:
                    "Send an explicit num_ctx rather than inheriting Ollama's default, which is " +
                    'VRAM-tiered and reaches 256K on a 48GB machine. Use keepAlive:"0" for one-off ' +
                    "calls so it does not hold memory after.",
                  caution:
                    "Do not co-resident this with another large model — that has crashed this " +
                    "machine. Check `models{view:\"resident\"}` first. Also note this repo measured a " +
                    "GGUF build BEATING its -mlx counterpart for the same family, so do not assume " +
                    "a suffix is better; benchmark the packaging, don't infer it from the name.",
                }
            : null,
        });
      }

      const params = new URLSearchParams();
      for (const c of capabilities ?? []) params.append("c", c);
      if (query) params.set("q", query);
      // Only send `o` for "newest": omitting it IS the popular ordering (verified against the
      // live site — no-`o` and `o=popular` return identical rankings).
      if (order === "newest") params.set("o", "newest");
      const url = `https://ollama.com/search?${params.toString()}`;
      const parsed = parseModelSearch(await fetchPage(url)).slice(0, limit ?? 10);
      const { installed } = await localState();
      const installedFamilies = new Set([...installed].map((n) => n.split(":")[0]));
      return structuredResult({
        query: url,
        order: order ?? "popular",
        count: parsed.length,
        nextStep: 'Not pullable yet. Call again with model:"<name>" to get tags, sizes and memory fit.',
        models: parsed.map((m) => ({ ...m, installed: installedFamilies.has(m.name) })),
      });
    } catch (error) {
      return errorResult(error);
    }
  },
);

// `recommend` was removed from the MCP surface (the server route /_freellama/v1/recommendations
// and the CLI subcommand both remain). It read a hand-maintained catalog and, asked for a vision
// model, returned exactly one suggestion — `gemma3:4b` — a 4B model, below the ~12B floor this
// project measured for research. A curated list that must be updated by hand goes stale, and
// `search_models` now covers the same ground from the live library with per-tag memory fit.
// Restore by re-registering a tool that calls recommend() from the native binding.


server.registerTool(
  "run_task",
  {
    description:
      "Routes AND executes chat/generate/embed/OCR from content YOU pass in — output tokens land " +
      "on the local model. NOT GROUNDED: no file access, and verified inventing wrong facts about " +
      "this repo when given none; use `delegate_research` if it needs a real file read. Images work " +
      '(name `model: "qwen3.8:27b-mlx"`). Needs serve',
    inputSchema: {
      endpoint: endpointParam,
      task: taskParam,
      objective: objectiveParam,
      model: z.string().optional().describe("Force this exact installed model name."),
      sessionId: z.string().optional().describe("Session id for model affinity across calls."),
      contextTokens: z.number().int().positive().optional().describe("Minimum context window required."),
      requiredCapabilities: requiredCapabilitiesParam,
      prompt: z.string().optional(),
      images: z.array(z.string()).optional().describe('base64, no data-URI prefix. Pair with an explicit vision model'),
      messages: z
        .array(z.object({ role: z.string(), content: z.string() }))
        .optional()
        .describe("wins over prompt"),
      input: z.union([z.string(), z.array(z.string())]).optional().describe("batch it — batching is far cheaper than one call per item"),
      tools: z.array(z.record(z.unknown())).optional(),
      keepAlive: z.string().optional().describe('"0" unloads now, "-1" pins, default 5m'),
      minConfidence: minConfidenceParam,
      returnEmbeddings: z.boolean().optional().describe("false (default) withholds the raw vectors; they are large and unreadable to a model"),
    },
    // Loads a model, spends compute, and binds session affinity when `sessionId` is set — but
    // adds only; it never removes an installed model or overwrites stored data. Not idempotent:
    // generation is sampled, so the same arguments give different output.
    // Only deviations from the spec defaults are declared; restating a default says nothing.
    // additive; sampled, so not idempotent
    annotations: { destructiveHint: false },
  },
  async ({
    endpoint,
    task,
    objective,
    model,
    sessionId,
    contextTokens,
    requiredCapabilities,
    prompt,
    images,
    messages,
    input,
    tools,
    keepAlive,
    minConfidence,
    returnEmbeddings,
  }) => {
    try {
      // Gating after the fact would be useless here: by the time `run_task` returns, the tokens
      // are spent. So when the caller sets a floor, preview the decision with a `route` call
      // first — free, no generation — and refuse before anything runs. Only costs the extra round
      // trip when the option is actually used.
      if (minConfidence) {
        const preview = parsedResult(
          // minConfidence is forwarded so the CORE gate refuses, with its actionable message naming
        // the two commands that raise the grade. The belowConfidence() check below stays only as a
        // fallback for servers older than the core gate.
        await route(endpoint, task, objective, model, sessionId, contextTokens, requiredCapabilities, minConfidence),
        );
        if ("structuredContent" in preview) {
          const refusal = belowConfidence(preview.structuredContent, minConfidence);
          if (refusal) return refusal;
        }
      }
      const result = parsedResult(
        await runTask(
          endpoint,
          task,
          objective,
          model,
          sessionId,
          contextTokens,
          requiredCapabilities,
          prompt,
          images,
          messages,
          input,
          tools,
          keepAlive,
          minConfidence,
        ),
      );
      if (!returnEmbeddings && "structuredContent" in result) {
        const trimmed = summarizeEmbeddings(result.structuredContent);
        if (trimmed) return structuredResult(trimmed);
      }
      return result;
    } catch (error) {
      return errorResult(error);
    }
  },
);

// `natural_route` was removed from the MCP surface (the server route `/_freellama/v1/natural-routes`
// still exists for CLI/HTTP callers). Three reasons, in order of weight:
//   1. Its consumer here is an LLM, which already knows the task kind — its own description said
//      "otherwise call `route` directly, one fewer round trip", i.e. it argued against itself.
//   2. It depends on a separately-installed small intent model. Deleting `qwen2.5:0.5b` broke it
//      outright (404 from /api/chat), and it stayed broken silently.
//   3. It cost ~400 tokens of schema on every request to serve a case that never applies here.
// Restore by re-registering a tool that calls `naturalRoute()` from the native binding, which is
// still exported and still works.

// --- Direct Ollama lifecycle tools ---
// Unlike machine/route/recommend/natural_route above, these talk to Ollama directly (no
// `freellama serve` required) — plain passthrough, no FreeLlama routing logic involved, so
// there's no reason to route them through the Rust/NAPI layer. The read-only inspection tools
// that used to live here (`list_models`, `ollama_ps`, `ollama_show`) are now views of the single
// `models` tool: they shared identical annotations, so merging them costs no honesty and removes
// two tools' worth of schema from every request. `ollama_delete` deliberately does NOT join a
// merged lifecycle tool — annotations are per-tool, so folding a destructive action in with pull
// and stop would force `destructiveHint: true` onto all three, or lie about delete.

server.registerTool(
  "ollama_manage",
  {
    description:
      "`pull`: real multi-GB download, only after a human approves a recommended model — never " +
      "speculatively on a route failure. `stop`: frees VRAM now instead of waiting out keep_alive; " +
      "reversible, it reloads on next use. Both additive and idempotent. Deleting is NOT here",
    inputSchema: {
      action: z.enum(["pull", "stop"]).describe('"pull" = disk, "stop" = memory'),
      model: z.string(),
      ollamaEndpoint: ollamaEndpointParam,
      timeoutSeconds: z
        .number()
        .int()
        .positive()
        .optional()
        .describe(`"pull" only. Defaults to ${DEFAULT_PULL_TIMEOUT_SECONDS}s.`),
    },
    // Both actions only ever add or free — neither removes an installed model or loses data, and
    // repeating either is a no-op. Identical annotations are exactly why the merge is safe.
    // Only deviations from the spec defaults are declared; restating a default says nothing.
    // pull and stop only add or free
    annotations: { destructiveHint: false, idempotentHint: true },
  },
  async ({ action, model, ollamaEndpoint, timeoutSeconds }) => {
    try {
      const data =
        action === "pull"
          ? await ollamaFetch(ollamaEndpoint, "/api/pull", {
              method: "POST",
              body: { name: model, stream: false },
              timeoutMs: (timeoutSeconds ?? DEFAULT_PULL_TIMEOUT_SECONDS) * 1000,
            })
          : await ollamaFetch(ollamaEndpoint, "/api/generate", {
              method: "POST",
              body: { model, keep_alive: 0 },
            });
      return structuredResult(data as Record<string, unknown>);
    } catch (error) {
      return errorResult(error);
    }
  },
);

server.registerTool(
  "ollama_delete",
  {
    description:
      "DESTRUCTIVE AND IRREVERSIBLE. Permanently removes an installed model; only a re-download " +
      "brings it back. Also carries `destructiveHint: true` so a client can gate it without " +
      "parsing this text. NEVER call it on a staleness heuristic " +
      "(\"unused N days\"): idle does not mean unneeded. ONLY call it after a human has named this " +
      "exact model for deletion in the current conversation",
    inputSchema: {
      model: z.string(),
      ollamaEndpoint: ollamaEndpointParam,
    },
    // Ollama answers DELETE /api/delete with an empty body, so there is nothing to echo — the
    // useful structured answer is which model this call removed.
    // The only tool here that sets `destructiveHint` to TRUE (four others set it false).
    // Machine-readable on purpose: the prose warning above can't be acted on by a client deciding
    // whether to prompt a human. `destructiveHint: true` is the spec default and is restated
    // anyway — this is the one tool where silence is the wrong kind of terse.
    annotations: { destructiveHint: true, idempotentHint: true },
  },
  async ({ model, ollamaEndpoint }) => {
    try {
      await ollamaFetch(ollamaEndpoint, "/api/delete", {
        method: "DELETE",
        body: { name: model },
      });
      return structuredResult({ deleted: model });
    } catch (error) {
      return errorResult(error);
    }
  },
);

/**
 * What this repo has actually measured for each model on grounded code research.
 *
 * Added after an eval exposed a real flaw in the first version of `assessDelegatedAnswer`: it
 * quoted qwen-27B's 98.9% base rate no matter which model had answered, and returned `accept` for
 * a 3B model that was right 38% of the time. A trust signal that ignores the model is worse than
 * none, because it launders a weak answer through a confident-looking verdict.
 *
 * Measured 2026-08-30, 8 grounded single-file lookups against this repo, `bash` adapter:
 *   qwen3.8:27b-mlx 8/8 | gemma4:12b-mlx 6/8 | llama3.2:3b 3/8 | qwen2.5:7b 2/8 | qwen2.5:0.5b 0/8
 */

server.registerTool(
  "delegate_research",
  {
    description:
      "One narrow, self-contained question answered by a local model reading files under " +
      "`workspacePath`. Returns the answer, an evidence trail, and a `verification` verdict " +
      "(accept/verify/escalate) computed from what the run did AND which model ran — not the " +
      "model's self-report. `escalate` = it read no files at all (recall, not research) OR the " +
      "model is measured too weak to trust here. Best shape: " +
      "1-5 files, lookup not judgment. ~98% token reduction. Read `structuredContent` rather than " +
      "the prose: `verification` gives recommendation + why, and `citations` gives the full " +
      "unclipped commands and paths behind the answer. Can fail outright; check isError and " +
      "retry once before concluding it's unanswerable",
    inputSchema: {
      question: z
        .string()
.describe("narrow and self-contained, answerable by reading files"),
      workspacePath: z
        .string()
.describe("absolute path; must be inside FREELLAMA_MCP_ALLOWED_ROOTS"),
      adapter: z
        .enum(["bash", "octocode"])
        .optional()
.describe('"bash" (default) beat "octocode" on every model measured, and is faster'),
      model: z
        .string()
        .optional()
.describe("default qwen3.8:27b-mlx. Below ~12B accuracy collapses — never go smaller"),
      ollamaEndpoint: z
        .string()
        .optional()
.describe("default :11435, the proxy (has retry/backoff)"),
    },
    // Spawns a subprocess, reads files under `workspacePath`, and loads a model into VRAM — real
    // effects, so not read-only. Additive only (it never writes to the workspace) and
    // non-idempotent (a local model generates the answer).
    // Only deviations from the spec defaults are declared; restating a default says nothing.
    // reads files, writes nothing
    annotations: { destructiveHint: false },
  },
  async ({ question, workspacePath, adapter, model, ollamaEndpoint }) => {
    const chosenAdapter: ResearchAdapter = adapter ?? DEFAULT_RESEARCH_ADAPTER;
    const chosenModel = model ?? DEFAULT_DELEGATE_MODEL;
    // Pre-flight, not post-hoc: a model this repo measured at 0-38% will not become right by
    // running it, so refuse before spending a model load and 10-40s of wall time on it.
    const known = MODEL_EVIDENCE[chosenModel];
    if (known?.grade === "unusable") {
      return structuredResult({
        adapter: chosenAdapter,
        verification: assessDelegatedAnswer(question, 0, chosenModel),
        answer: "",
        toolCallCount: 0,
        usage: { inputTokens: null, outputTokens: null },
        evidence: [],
        summary:
          `Refused before running: ${chosenModel} is measured unusable for research here ` +
          `(${known.note}). Re-run with qwen3.8:27b-mlx, or answer it yourself.`,
      });
    }
    let resolvedWorkspace: string;
    try {
      resolvedWorkspace = await assertAllowedWorkspace(workspacePath);
    } catch (error) {
      return errorResult(error);
    }
    const dir = await mkdtemp(path.join(tmpdir(), "freellama-delegate-"));
    const promptFile = path.join(dir, "prompt.md");
    const resultFile = path.join(dir, "result.json");
    try {
      await writeFile(promptFile, `${question}\n`, "utf8");
      const adapter = RESEARCH_ADAPTERS[chosenAdapter];
      if (!existsSync(adapter)) {
        return errorResult(
          new Error(
            `research adapter not found at ${adapter}. In a published install it should be bundled ` +
              "under <package>/adapters; in-repo it comes from benchmark/local/scripts. Reinstall, " +
              "or run `npm run build` from packages/mcp/ to re-copy it.",
          ),
        );
      }
      const running = execFileAsync("python3", [adapter], {
        env: {
          ...process.env,
          FREELLAMA_TARGET_MODEL: chosenModel,
          FREELLAMA_OLLAMA_ENDPOINT: ollamaEndpoint ?? DEFAULT_SERVE_ENDPOINT,
          FREELLAMA_BENCH_WORKSPACE: resolvedWorkspace,
          FREELLAMA_BENCH_PROMPT: promptFile,
          FREELLAMA_AGENT_RESULT: resultFile,
          FREELLAMA_AGENT_MAX_TURNS: String(DEFAULT_DELEGATE_MAX_TURNS),
        },
        timeout: DEFAULT_DELEGATE_TIMEOUT_SECONDS * 1000,
        // The answer is read from `resultFile`, never from stdout — but execFile's default 1 MB
        // maxBuffer still applies to the agent's own progress logging, and overflowing it kills
        // the subprocess and surfaces as a research failure with no explanation. Give the logs
        // room; nothing here is proportional to the size of the answer.
        maxBuffer: 32 * 1024 * 1024,
        // On timeout, don't negotiate: SIGTERM can be swallowed by a python process blocked in a
        // long model call, which would leave the child (and its VRAM) alive past the deadline the
        // caller was promised.
        killSignal: "SIGKILL",
      });
      liveDelegates.add(running.child);
      // The adapter exits non-zero for its *own* failures — the model returned prose instead of
      // JSON, the endpoint was unreachable — but it still writes result.json first, with the real
      // diagnosis in `final_answer`. Letting the exec rejection propagate replaced that diagnosis
      // with "Command failed: python3 …", which names the wrong layer and hides the evidence trail
      // showing how far the run actually got. Capture it and prefer the adapter's own account.
      let adapterError: unknown = null;
      try {
        await running;
      } catch (error) {
        adapterError = error;
      } finally {
        liveDelegates.delete(running.child);
      }
      type AdapterResult = {
        final_answer: string;
        tool_calls: Array<{
          raw_name?: string;
          status?: string;
          arguments?: { tool?: string; command?: string; queries?: { path?: string } };
        }>;
        usage: { input_tokens: number | null; output_tokens: number | null };
      };
      let result: AdapterResult;
      try {
        // Read AND parse under one guard. A SIGKILL at the timeout can land mid-write, leaving a
        // truncated result.json — reading it succeeds and only the parse fails, so guarding the
        // read alone reported "Unexpected end of JSON input" and threw away the timeout diagnosis
        // that actually explains the run.
        result = JSON.parse(await readFile(resultFile, "utf8")) as AdapterResult;
      } catch {
        // No usable result file: a hard kill (the SIGKILL timeout above) or a crash before the
        // adapter could finish writing. Here the exec error genuinely is the best account.
        return errorResult(
          adapterError ??
            new Error(
              "research adapter exited without writing a readable result file — it was killed " +
                `before it could report. Check that the model is loadable and that ${DEFAULT_DELEGATE_TIMEOUT_SECONDS}s ` +
                "is enough for this question.",
            ),
        );
      }
      // Surface the evidence trail (which tool, which path) so the orchestrator can spot-check
      // *how* the answer was reached without re-deriving it — verifier independence in practice,
      // not just in principle. A citable but unread-through answer is exactly the failure mode
      // task-delegation.md warns about for judgment-heavy tasks.
      // The two adapters describe their calls differently — octocode puts the tool under
      // `arguments.tool` with a path in `arguments.queries.path`, bash reports `shell` with the
      // command line in `arguments.command`. Normalize both so the evidence trail reads the same
      // whichever adapter ran.
      const evidence = result.tool_calls.map((call, index) => {
        const target = call.arguments?.queries?.path;
        return {
          step: index + 1,
          tool: call.arguments?.tool ?? call.raw_name ?? "?",
          // The adapters record "ok" | "error" | "repeat" per call. Carrying it through is what
          // lets a reader tell a run that read three files from one that failed three commands —
          // indistinguishable in the trail before, and graded identically.
          status: call.status ?? "ok",
          path: target ? path.relative(resolvedWorkspace, target) : null,
          detail: call.arguments?.command ?? null,
        };
      });
      const succeeded = evidence.filter((step) => step.status === "ok");
      const failed = evidence.length - succeeded.length;
      // `evidence[].detail` above is the FULL command, deliberately unclipped: the structured half
      // exists to be audited, and a command cut mid-flag cannot be. Only the prose line below is
      // clipped, and it says how much it dropped.
      const evidenceText = evidence
        .map(
          (step) =>
            `  ${step.step}. ${step.tool}${step.status === "ok" ? "" : ` [${step.status}]`}` +
            `${step.path ? ` -> ${step.path}` : ""}` +
            `${step.detail ? `: ${clipText(step.detail, 400)}` : ""}`,
        )
        .join("\n");
      // The compact machine-readable half. Two independent small-model callers asked for exactly
      // this shape — recommendation, why, citations — rather than parsing it back out of the prose.
      // Successful steps only: a failed command is not a citation for anything.
      const citations = succeeded.map((step) => ({
        step: step.step,
        tool: step.tool,
        path: step.path,
        command: step.detail,
      }));
      if (adapterError) {
        return errorResult(
          new Error(
            `research adapter failed: ${result.final_answer}` +
              (evidenceText ? `\nEvidence collected before the failure:\n${evidenceText}` : ""),
          ),
        );
      }
      // Grade on what actually read something. A run of failed commands is ungrounded no matter
      // how many of them there were.
      const verification = assessDelegatedAnswer(question, succeeded.length, chosenModel);
      const summary =
        `${result.final_answer}\n\n` +
        `[delegated: ${result.tool_calls.length} tool call(s)` +
        (failed > 0 ? `, ${failed} of which did not succeed` : "") +
        `, ` +
        `${result.usage.input_tokens ?? "?"} input / ${result.usage.output_tokens ?? "?"} ` +
        "output tokens spent on the local model]\n" +
        (evidenceText
          ? `Evidence trail:\n${evidenceText}`
          : "No tool calls were made — treat this answer as unverified.") +
        `\n\nVerification: ${verification.recommendation.toUpperCase()} — ${verification.why}`;
      // Deliberate deviation from the spec's "SHOULD also return the serialized JSON in a text
      // block": the text block keeps the composed prose summary, because that is what a reading
      // model actually consumes, and the same summary is carried in `structuredContent.summary`
      // so nothing is lost to a client that only reads the structured half.
      return {
        content: [{ type: "text" as const, text: summary }],
        structuredContent: {
          adapter: chosenAdapter,
          verification,
          answer: result.final_answer,
          // recommendation + why live under `verification`; `citations` completes the triple so a
          // caller never has to read `summary` to act on the result.
          citations,
          toolCallCount: result.tool_calls.length,
          successfulToolCallCount: succeeded.length,
          usage: {
            inputTokens: result.usage.input_tokens,
            outputTokens: result.usage.output_tokens,
          },
          evidence,
          summary,
        },
      };
    } catch (error) {
      return errorResult(error);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  },
);

const transport = new StdioServerTransport();
await server.connect(transport);
