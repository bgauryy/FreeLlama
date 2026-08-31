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
import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

const execFileAsync = promisify(execFile);
/**
 * Repo root, found by walking up to the directory containing `Cargo.toml`.
 *
 * This used to be `path.resolve(import.meta.url, "../../../")` — a hardcoded depth, which meant
 * relocating this package (say under a `packages/` monorepo layout) would silently resolve
 * REPO_ROOT one level short. That is not a cosmetic bug: REPO_ROOT is the default for
 * `ALLOWED_RESEARCH_ROOTS`, so a wrong value silently widens the directory boundary
 * `delegate_research` is allowed to read. Anchoring on a marker file makes the location a
 * non-issue. Falls back to the old relative guess if no marker is found.
 */
function findRepoRoot(): string {
  let dir = path.dirname(fileURLToPath(import.meta.url));
  for (let depth = 0; depth < 10; depth += 1) {
    if (existsSync(path.join(dir, "Cargo.toml"))) return dir;
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return path.resolve(fileURLToPath(import.meta.url), "../../../");
}

const REPO_ROOT = findRepoRoot();
// Two interchangeable research adapters. They take an identical env interface and write an
// identical result shape, so which one runs is purely a routing decision — and this repo's own
// benchmark settles it. From `benchmark/local/results/*/aggregate.json` (30 questions x 3 repos,
// same model, same tasks, one variable):
//
//   model                  bash pass@1   octocode pass@1   bash median   octocode median
//   qwen3.8:27b-mlx           86.7%          86.7%            19.6s          55.6s
//   muse-glimmer:30b-mlx      96.7%          63.3%            28.3s         103.0s
//   gemma4:12b-mlx             6.7%           0.0%              —              —
//
// bash wins or ties on every model, at 116.5 vs 53.8 successful tasks/hour. Confirmed again live
// on a single question: 15.7s / 791 input tokens (bash) vs ~40s / 7,761 (octocode). Hence the
// default below. `octocode` stays available because its structured search may still suit
// questions the flat 30-question suite doesn't represent — but it has to be asked for.
//
// Resolution order matters for a PUBLISHED install. In-repo the adapters live in `benchmark/`,
// which is their single source of truth; `npm run build` copies them into `adapters/` so the
// packed tarball carries them too. Without that copy `delegate_research` is dead on arrival once
// installed from npm — `files` ships only `dist`/`native`, so the python would simply not be there
// and every call would fail with ENOENT.
const PACKAGE_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function adapterPath(file: string): string {
  const bundled = path.join(PACKAGE_ROOT, "adapters", file);
  if (existsSync(bundled)) return bundled;
  return path.join(REPO_ROOT, "benchmark/local/scripts", file);
}

const RESEARCH_ADAPTERS = {
  bash: adapterPath("bash_agent.py"),
  octocode: adapterPath("octocode_agent.py"),
} as const;
type ResearchAdapter = keyof typeof RESEARCH_ADAPTERS;

const DEFAULT_RESEARCH_ADAPTER: ResearchAdapter =
  process.env.FREELLAMA_MCP_DEFAULT_ADAPTER === "octocode" ? "octocode" : "bash";

function envInt(name: string, fallback: number): number {
  const raw = process.env[name];
  if (!raw) return fallback;
  const parsed = Number.parseInt(raw, 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

// Every default below is a starting point, not a structural assumption — override via env var
// rather than editing source, so a different deployment (different port, different default
// model, tighter/looser timeouts) never needs a recompile.
const DEFAULT_OLLAMA_ENDPOINT = process.env.FREELLAMA_OLLAMA_ENDPOINT ?? "http://127.0.0.1:11434";
// Same env var name the Rust side (packages/rust-core/src/napi.rs) uses for its own serve-endpoint default — one
// name, one meaning, across both languages.
const DEFAULT_SERVE_ENDPOINT = process.env.FREELLAMA_SERVE_ENDPOINT ?? "http://127.0.0.1:11435";
const DEFAULT_DELEGATE_MODEL = process.env.FREELLAMA_MCP_DEFAULT_MODEL ?? "qwen3.8:27b-mlx";
const DEFAULT_DELEGATE_MAX_TURNS = envInt("FREELLAMA_MCP_MAX_TURNS", 8);
const DEFAULT_DELEGATE_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS", 180);
const DEFAULT_PULL_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_PULL_TIMEOUT_SECONDS", 1200);
const DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS", 30);

// `delegate_research` grants a local model read access to whatever directory it's pointed at.
// Without a boundary, an orchestrator (or a bug, or a compromised orchestrator) could point it at
// $HOME or / and have a local model read arbitrary files on the machine — verified live: an
// unconstrained version of this tool happily listed a real $HOME (Desktop, Documents, Library...).
// Default to just this repo; extend via a colon-separated allowlist, never accept "anything".
const ALLOWED_RESEARCH_ROOTS = (process.env.FREELLAMA_MCP_ALLOWED_ROOTS ?? REPO_ROOT)
  .split(":")
  .filter(Boolean)
  .map((root) => path.resolve(root));

// Resolved once, lazily, and through symlinks — see `assertAllowedWorkspace`. A root that can't
// be resolved (typo'd env var, deleted directory) falls back to its lexical form rather than
// disappearing from the allowlist, so a broken entry can never silently widen the boundary.
let resolvedRootsPromise: Promise<string[]> | null = null;
function allowedResearchRoots(): Promise<string[]> {
  resolvedRootsPromise ??= Promise.all(
    ALLOWED_RESEARCH_ROOTS.map(async (root) => {
      try {
        return await realpath(root);
      } catch {
        return root;
      }
    }),
  );
  return resolvedRootsPromise;
}

async function assertAllowedWorkspace(workspacePath: string): Promise<string> {
  // `realpath`, not just `path.resolve`: resolve() is pure string arithmetic, so a symlink placed
  // inside an allowed root and pointing at $HOME (or /) passes a prefix check while actually
  // handing the local model everything on the other side of the link. The roots go through
  // realpath too, or the comparison would fail legitimately on macOS, where paths like /tmp are
  // themselves symlinks.
  let resolved: string;
  try {
    resolved = await realpath(path.resolve(workspacePath));
  } catch {
    throw new Error(
      `workspacePath "${workspacePath}" does not exist or is not readable. It must be an ` +
        "absolute path to a directory that exists on this machine.",
    );
  }
  const roots = await allowedResearchRoots();
  const allowed = roots.some(
    (root) => resolved === root || resolved.startsWith(`${root}${path.sep}`),
  );
  if (!allowed) {
    throw new Error(
      `workspacePath "${workspacePath}" resolves to "${resolved}", which is outside the allowed ` +
        `research roots (${roots.join(", ")}). Set FREELLAMA_MCP_ALLOWED_ROOTS ` +
        "(colon-separated) to extend this if you genuinely need to research another directory.",
    );
  }
  return resolved;
}

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

// Default (CJS) import — the native binding module doesn't declare static named exports
// (`index.js` re-exports a `require()`'d `.node` file), so a named `import { doctor } from ...`
// is not reliably detectable by Node's CJS/ESM interop. Destructuring after a default import
// sidesteps that entirely.
import native from "../native/index.js";
const { doctor, machine, listModels, route, runTask } = native as {
  doctor: (endpoint?: string | null) => Promise<string>;
  machine: (endpoint?: string | null) => Promise<string>;
  listModels: (endpoint?: string | null) => Promise<string>;
  route: (
    endpoint: string | null | undefined,
    task: string,
    objective?: string | null,
    model?: string | null,
    sessionId?: string | null,
    contextTokens?: number | null,
    requiredCapabilities?: string[] | null,
    minConfidence?: string | null,
  ) => Promise<string>;
  runTask: (
    endpoint: string | null | undefined,
    task: string,
    objective?: string | null,
    model?: string | null,
    sessionId?: string | null,
    contextTokens?: number | null,
    requiredCapabilities?: string[] | null,
    prompt?: string | null,
    images?: string[] | null,
    messages?: unknown | null,
    input?: unknown | null,
    tools?: unknown | null,
    keepAlive?: string | null,
    minConfidence?: string | null,
  ) => Promise<string>;
};

// Single source of truth for the version — a hardcoded literal here silently drifts from the
// package it ships in (it already had: this file said 0.1.0 while the crate it wraps was 0.2.0).
// package.json is always present in an npm tarball regardless of the `files` allowlist, and
// `../package.json` resolves to the package root from `dist/index.js`.
const { version: SERVER_VERSION } = createRequire(import.meta.url)("../package.json") as {
  version: string;
};

// Guidance that applies ACROSS tools lives here, not repeated in each description. Measured
// motivation: the tool list is re-sent on every request — it was 7,431 tokens across 13 tools,
// dwarfing anything a single delegated call saves. Shared caveats stated once here, tool-specific
// facts in the descriptions.
const INSTRUCTIONS = `Offload token-heavy, non-reasoning work to local Ollama models.
Optimise for quality and token reduction; latency is the tiebreak.
For the full orchestration playbook — tiering work across you / a cheap cloud model / local Ollama,
and what each tier must never be given — read the freellama skill (skills/freellama/SKILL.md).

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

async function ollamaFetch(
  endpoint: string | undefined,
  path: string,
  init: { method?: string; body?: unknown; timeoutMs?: number } = {},
): Promise<unknown> {
  const base = (endpoint ?? DEFAULT_OLLAMA_ENDPOINT).replace(/\/$/, "");
  const controller = new AbortController();
  const timeout = setTimeout(
    () => controller.abort(),
    init.timeoutMs ?? DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS * 1000,
  );
  try {
    const response = await fetch(`${base}${path}`, {
      method: init.method ?? "GET",
      headers: init.body ? { "content-type": "application/json" } : undefined,
      body: init.body ? JSON.stringify(init.body) : undefined,
      signal: controller.signal,
    });
    const text = await response.text();
    if (!response.ok) {
      throw new Error(`Ollama ${init.method ?? "GET"} ${path} -> HTTP ${response.status}: ${text}`);
    }
    return text ? JSON.parse(text) : {};
  } finally {
    clearTimeout(timeout);
  }
}

// Self-evident params carry no `.describe()`: the name says it, the default is in the server
// instructions, and every description is re-sent on every request. Only params whose BEHAVIOUR
// isn't obvious from the name keep one.
const endpointParam = z.string().optional().describe("serve endpoint, default :11435");
const ollamaEndpointParam = z.string().optional().describe("Ollama endpoint, default :11434");
const taskParam = z.string().describe('e.g. completion, code_repair, vision, embedding');
const objectiveParam = z
  .enum(["fastest", "balanced", "quality"])
  .optional()
  .describe('"balanced"/"quality" need a configured policy; "fastest" does not');
// The server grades every route decision (`route_evidence` in packages/rust-core/src/platform/routing.rs): "medium" only
// when the task has BOTH a configured policy and benchmark data, "low" otherwise — there is no
// "high". A "low"/capability_metadata_only decision is exactly what returned `qwen2.5:0.5b` for
// code repair on this machine. Unchecked, that answer comes back looking like any other.
const CONFIDENCE_RANK: Record<string, number> = { low: 1, medium: 2 };

const minConfidenceParam = z
  .enum(["low", "medium"])
  .optional()
  .describe('Fail closed below this. "medium" needs a policy AND benchmark data; "low" (default) accepts capability metadata alone. The refusal names what was missing');

/**
 * Fail closed when a route decision isn't backed well enough for what the caller asked.
 *
 * Returns an error result rather than throwing, so the refusal reaches the caller as a normal
 * tool result carrying the rejected decision — the point is to hand back enough to decide what to
 * do next, not merely to say no.
 */
function belowConfidence(decision: Record<string, unknown>, minConfidence?: "low" | "medium") {
  if (!minConfidence) return null;
  const actual = typeof decision.confidence === "string" ? decision.confidence : "low";
  if ((CONFIDENCE_RANK[actual] ?? 1) >= (CONFIDENCE_RANK[minConfidence] ?? 1)) return null;
  return errorResult(
    new Error(
      `Route refused: confidence is "${actual}" (evidence: ${decision.evidence}), below the ` +
        `requested minimum "${minConfidence}". Selected model would have been ` +
        `"${decision.selected_model}" for reasons [${(decision.reasons as string[] | undefined)?.join(", ")}].\n` +
        "This is a fail-closed refusal, not a failure: the local router cannot justify this pick. " +
        "Escalate to your own model, pass an explicit `model`, or run `bench-all` and configure a " +
        "task policy to raise the evidence level.",
    ),
  );
}

const requiredCapabilitiesParam = z
  .array(z.string())
  .optional()
  .describe('e.g. ["vision"], ["tools"]. Fails closed rather than picking a model that can\'t do it');

// Pretty-printing is easier for a model to read but is pure overhead once a payload is large:
// a single 768-dim embedding measured 10,293 bytes compact vs 17,471 pretty — ~1,800 wasted
// tokens of indentation for zero information. Stay pretty while it's cheap, go compact when it
// isn't.
/**
 * Clip text to `limit` characters, keeping BOTH ends and saying how much was dropped.
 *
 * Four call sites each rolled their own `slice()`, three of them with no marker — so a reader could
 * not tell a complete value from a cut one. That is how a `delegate_research` evidence line came
 * back as `... --exclude-dir={node_modules,target,.venv,__pycach`: a hard 120-char head slice
 * through the middle of a command, which is unauditable precisely when auditing matters.
 *
 * Head-biased because the start of a command or message identifies it, with a tail so the target
 * of a long `grep` survives. Mirrors `agent_context.clip` on the adapter side, deliberately: the
 * same rule on both sides of the boundary.
 */
function clipText(text: string, limit: number): string {
  if (text.length <= limit) return text;
  const marker = `… [${text.length - limit} more chars] …`;
  const usable = Math.max(limit - marker.length, 0);
  if (usable === 0) return text.slice(0, limit);
  const head = Math.floor((usable * 2) / 3);
  return text.slice(0, head) + marker + text.slice(-(usable - head));
}

const PRETTY_PRINT_MAX_BYTES = 8 * 1024;

function serialize(value: unknown): string {
  const compact = JSON.stringify(value);
  return compact.length > PRETTY_PRINT_MAX_BYTES ? compact : JSON.stringify(value, null, 2);
}

// Results carry both `structuredContent` (the parsed object) and the serialized JSON as a text
// block, per the spec's backwards-compatibility SHOULD. No `outputSchema` is declared — see below.
function structuredResult(value: Record<string, unknown>) {
  return {
    content: [{ type: "text" as const, text: serialize(value) }],
    structuredContent: value,
  };
}

// The Rust/native layer and Ollama both return JSON text. Parse it once here so the object can be
// handed back as `structuredContent` instead of being re-parsed by every consumer. A payload that
// doesn't parse is a genuine failure, not something to pass through as opaque text — and error
// results are exempt from output-schema validation, so reporting it that way is also the only
// shape that can't turn into a protocol-level McpError.
function parsedResult(raw: string) {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch (error) {
    return errorResult(
      new Error(
        `upstream returned a payload that is not valid JSON: ${
          error instanceof Error ? error.message : String(error)
        }\n${clipText(raw, 2000)}`,
      ),
    );
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return errorResult(new Error(`expected a JSON object from upstream, got: ${clipText(raw, 500)}`));
  }
  return structuredResult(value as Record<string, unknown>);
}

function errorResult(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return { content: [{ type: "text" as const, text: message }], isError: true };
}

// Output schemas were removed deliberately. They cost ~1,086 tokens on EVERY request to buy
// client-side JSON-Schema validation — and that validation was itself a hazard: a strict schema
// turned any undeclared upstream field into a hard `McpError` (caught live when /api/show returned
// an undocumented `requires` field). Verified against the SDK: `structuredContent` still reaches
// the client with no `outputSchema` declared, so callers keep the parsed object and lose only the
// validation. Reinstate per-tool if a consumer needs machine-checked output.


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

/**
 * Parse ollama.com/search result cards.
 *
 * There is no JSON API — `Accept: application/json` still returns HTML, and `/api/search`,
 * `/search.json`, and the registry `_catalog` endpoint all 404. So this parses the rendered page,
 * which means it is inherently coupled to Ollama's markup and can break on a redesign. Failures
 * surface as "0 results" rather than an exception, so the tool degrades to unhelpful instead of
 * broken; the shape it depends on is one <li> per model, each containing a /library/<name> link.
 */
function parseModelSearch(html: string) {
  const results = [];
  for (const block of html.split(/<li\s/).slice(1)) {
    const name = block.match(/href="\/library\/([^"]+)"/)?.[1];
    if (!name) continue;
    const description =
      block
        .match(/<p class="max-w-lg[^"]*">([\s\S]*?)<\/p>/)?.[1]
        ?.replace(/<[^>]+>/g, "")
        .replace(/&#39;/g, "'")
        .replace(/&amp;/g, "&")
        .replace(/&quot;/g, '"')
        .replace(/\s+/g, " ")
        .trim() ?? "";
    // Indigo chips are runtime capabilities; the cyan "cloud" chip means the model runs on
    // Ollama's hosted service, NOT on this machine — the distinction that matters most here.
    const capabilities = [...block.matchAll(/text-(?:indigo-600|cyan-500)[^>]*>([a-z]+)<\/span>/g)].map(
      (m) => m[1],
    );
    const stat = (label: string) =>
      block.match(
        new RegExp(`<span >([\\d.,KMB]+)<\\/span>\\s*<span class="hidden sm:flex">&nbsp;${label}`),
      )?.[1] ?? null;
    results.push({
      name,
      description: clipText(description, 160),
      capabilities,
      pulls: stat("Pulls"),
      tags: stat("Tag"),
      cloudOnly: capabilities.includes("cloud"),
    });
  }
  return results;
}

/**
 * Parse the tag table on ollama.com/library/<name>.
 *
 * This is the step search cannot cover: search returns FAMILY names (`gemma4`), and a family is
 * not pullable — you pull a tag (`gemma4:12b`), and only the tag carries the size that decides
 * whether it fits in memory. The page renders each tag twice (a mobile row and a desktop grid);
 * the mobile row is the one with everything on a single line, so it is what gets parsed.
 */
function parseModelTags(html: string, family: string) {
  const tags = [];
  const seen = new Set<string>();
  const row = /<a href="\/library\/([^"]+)" class="sm:hidden[\s\S]*?<p class="flex text-neutral-500">([^<]*)<\/p>/g;
  for (const m of html.matchAll(row)) {
    const tag = m[1];
    if (seen.has(tag)) continue;
    seen.add(tag);
    const meta = m[2].replace(/&middot;/g, "·").split("·").map((x) => x.trim());
    const sizeText = meta.find((x) => /^[\d.]+\s?[MG]B$/i.test(x)) ?? null;
    const bytes = sizeText
      ? Number.parseFloat(sizeText) * (/GB/i.test(sizeText) ? 1e9 : 1e6)
      : null;
    tags.push({
      tag,
      size: sizeText,
      sizeBytes: bytes,
      context: meta.find((x) => /context window/i.test(x))?.replace(/\s*context window\s*/i, "") ?? null,
      modalities: meta.find((x) => /^(Text|Image|Audio)/i.test(x)) ?? null,
      updated: meta.at(-1) ?? null,
    });
  }
  return { family, tags };
}

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

/**
 * Strip the raw embedding matrix out of a `run_task` result, leaving enough to verify the call
 * worked (how many vectors, what dimensionality, the leading values of the first one).
 *
 * `run_task` advertises that the orchestrator "pays only the JSON wrapper (~hundreds of tokens,
 * regardless of output size)". For embeddings that was not true: measured against a live server,
 * one 768-dim `nomic-embed-text` vector came back as 17,471 bytes of pretty-printed JSON — about
 * 4,400 tokens, into the context of the model that was supposed to be *offloading* work. A batch
 * of twenty inputs would have been ~90k. Vectors are also the one kind of output a language model
 * can do nothing useful with by reading it.
 *
 * Returns `null` when there is no embedding matrix to strip, so non-embedding tasks pass through
 * untouched.
 */
function summarizeEmbeddings(payload: Record<string, unknown>): Record<string, unknown> | null {
  const response = payload.response;
  if (response === null || typeof response !== "object") return null;
  const { embeddings, ...rest } = response as Record<string, unknown>;
  if (!Array.isArray(embeddings) || embeddings.length === 0) return null;
  const first = embeddings[0];
  const dimensions = Array.isArray(first) ? first.length : null;
  return {
    ...payload,
    response: {
      ...rest,
      embeddings_omitted: {
        count: embeddings.length,
        dimensions,
        preview: Array.isArray(first) ? first.slice(0, 8) : null,
        note:
          "Vectors withheld to keep them out of the orchestrator's context — pass " +
          "`returnEmbeddings: true` to get the full matrix (a single 768-dim vector is ~4,400 " +
          "tokens). `preview` is the first 8 values of the first vector, enough to confirm the " +
          "call really produced numbers.",
      },
    },
  };
}

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
/**
 * Per-model research grades, loaded from disk — never compiled in.
 *
 * These are one machine's benchmark results. Baking them into a shipped server would make the
 * binary carry someone else's measurements as if they were universal, and they would rot silently
 * the moment models changed. The server carries the *mechanism*; the data lives in
 * `benchmark/evidence/model-evidence.json` (override with FREELLAMA_MCP_MODEL_EVIDENCE).
 *
 * Empty by default. A model with no entry is treated as unmeasured, which yields a `verify`
 * verdict — the correct answer when nothing is known, and a safer default than assuming strength.
 */
type ModelGrade = { grade: "strong" | "weak" | "unusable"; note: string };

const MODEL_EVIDENCE: Record<string, ModelGrade> = (() => {
  const configured = process.env.FREELLAMA_MCP_MODEL_EVIDENCE;
  const candidates = [
    configured,
    path.join(REPO_ROOT, "benchmark/evidence/model-evidence.json"),
  ].filter((x): x is string => Boolean(x));
  for (const file of candidates) {
    try {
      if (!existsSync(file)) continue;
      const parsed = JSON.parse(readFileSync(file, "utf8")) as { models?: Record<string, ModelGrade> };
      return parsed.models ?? {};
    } catch {
      // A malformed evidence file must not stop the server: an empty table degrades every verdict
      // to `verify`, which is conservative rather than wrong.
    }
  }
  return {};
})();

/**
 * Classify how far a delegated answer should be trusted, from observable facts only.
 *
 * The measured problem this exists to solve: the local model is 98.9% accurate on grounded
 * lookups and only ~67% on judgment calls, and it uses **the same confident tone for both** — so
 * the answer text alone carries no signal about which one you got. Everything here is derived
 * from what actually happened (did it read any files? how many?) plus one clearly-labelled
 * heuristic on the question's shape. Nothing is inferred from the model's own self-report, which
 * is exactly the thing that isn't reliable.
 *
 * This never escalates on its own. It emits a recommendation the orchestrator acts on, matching
 * how the rest of this server treats state-changing decisions.
 */
function assessDelegatedAnswer(
  question: string,
  /**
   * Count of tool calls that actually **succeeded**. Not the raw call count: a run whose commands
   * all errored, or which only repeated an earlier call, read nothing — and a verdict computed
   * from "it made 3 calls" would have graded that `accept` while the answer was pure recall. That
   * is precisely the failure this function exists to catch, so it must not be fed a number that
   * counts failures as evidence.
   */
  evidenceCount: number,
  model: string,
): {
  recommendation: "accept" | "verify" | "escalate";
  grounded: boolean;
  why: string;
  measuredBaseRate: string;
} {
  const grounded = evidenceCount > 0;
  const evidence = MODEL_EVIDENCE[model];

  // The model gates everything else. No amount of grounding rescues a model measured at 0-38%,
  // and an unmeasured model has no base rate to quote in the first place.
  if (evidence?.grade === "unusable") {
    return {
      recommendation: "escalate",
      grounded,
      why:
        `${model} is not viable for research on this machine (${evidence.note}). It answers fast ` +
        "and confidently while being wrong — a fast wrong answer is not a speed win. Re-run with " +
        "qwen3.8:27b-mlx, or answer it yourself.",
      measuredBaseRate: `${model}: ${evidence.note}`,
    };
  }
  if (!evidence) {
    return {
      recommendation: "verify",
      grounded,
      why:
        `${model} has no measured accuracy in this repo's benchmarks, so no base rate applies to ` +
        "this answer. Treat it as unverified until it has been evaluated — accuracy fell off a " +
        "cliff below ~12B in the models that were measured.",
      measuredBaseRate: "no measured base rate for this model",
    };
  }
  if (evidence.grade === "weak" && grounded) {
    return {
      recommendation: "verify",
      grounded,
      why:
        `${model} holds up on simple single-file lookups but not beyond (${evidence.note}). ` +
        "Check the evidence trail, or re-run on qwen3.8:27b-mlx if the answer matters.",
      measuredBaseRate: `${model}: ${evidence.note}`,
    };
  }
  if (!grounded) {
    return {
      recommendation: "escalate",
      grounded: false,
      why:
        "The model answered without reading a single file, so this is parametric recall, not " +
        "research — the failure mode where `run_task` was verified inventing wrong facts about " +
        "this project's own architecture. Re-ask with a narrower question, or answer it yourself.",
      measuredBaseRate: "ungrounded answers have no measured accuracy — they were never the tested path",
    };
  }
  // Judgment questions are the ~67% bucket. This is a keyword heuristic, not a classifier, and is
  // labelled as one: it errs toward asking for verification, which costs a read, not a wrong answer.
  const judgmentSignals = /\b(should|better|best|worth|review|assess|evaluate|improve|opinion|recommend|why is|is it (good|safe|correct)|design|refactor)\b/i;
  if (judgmentSignals.test(question)) {
    return {
      recommendation: "verify",
      grounded: true,
      why:
        "The question reads as a judgment call (keyword heuristic), which is the ~67%-accurate " +
        "bucket rather than the 98.9% one — and the tone is identical either way. Check the " +
        "evidence trail against the claim before acting on it.",
      measuredBaseRate: `${model}: ~67% on judgment calls vs 98.9% on grounded lookups`,
    };
  }
  if (evidenceCount > 5) {
    return {
      recommendation: "verify",
      grounded: true,
      why:
        `${evidenceCount} tool calls is outside the 1-5 file envelope this tool was measured on. ` +
        "Wide searches are where it drifts; spot-check the evidence trail.",
      measuredBaseRate: `${model}: 98.9% on grounded lookups, measured within a 1-5 file scope`,
    };
  }
  return {
    recommendation: "accept",
    grounded: true,
    why:
      `Grounded in ${evidenceCount} tool call(s) within the measured 1-5 file envelope, the ` +
      `question is lookup-shaped, and ${model} is measured strong for this (${evidence.note}).`,
    measuredBaseRate: `${model}: 98.9% on grounded lookups (100+ questions)`,
  };
}

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
