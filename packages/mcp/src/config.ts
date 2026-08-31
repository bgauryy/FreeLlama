// Environment-derived configuration and the `delegate_research` workspace boundary.
// Every default is overridable via an env var so no deployment needs a recompile.
import { existsSync } from "node:fs";
import { realpath } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

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

export const REPO_ROOT = findRepoRoot();
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
export const PACKAGE_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function adapterPath(file: string): string {
  const bundled = path.join(PACKAGE_ROOT, "adapters", file);
  if (existsSync(bundled)) return bundled;
  return path.join(REPO_ROOT, "benchmark/local/scripts", file);
}

export const RESEARCH_ADAPTERS = {
  bash: adapterPath("bash_agent.py"),
  octocode: adapterPath("octocode_agent.py"),
} as const;
export type ResearchAdapter = keyof typeof RESEARCH_ADAPTERS;

export const DEFAULT_RESEARCH_ADAPTER: ResearchAdapter =
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
export const DEFAULT_OLLAMA_ENDPOINT = process.env.FREELLAMA_OLLAMA_ENDPOINT ?? "http://127.0.0.1:11434";
// Same env var name the Rust side (packages/rust-core/src/napi.rs) uses for its own serve-endpoint default — one
// name, one meaning, across both languages.
export const DEFAULT_SERVE_ENDPOINT = process.env.FREELLAMA_SERVE_ENDPOINT ?? "http://127.0.0.1:11435";
export const DEFAULT_DELEGATE_MODEL = process.env.FREELLAMA_MCP_DEFAULT_MODEL ?? "qwen3.8:27b-mlx";
export const DEFAULT_DELEGATE_MAX_TURNS = envInt("FREELLAMA_MCP_MAX_TURNS", 8);
export const DEFAULT_DELEGATE_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS", 180);
export const DEFAULT_PULL_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_PULL_TIMEOUT_SECONDS", 1200);
export const DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS", 30);

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

export async function assertAllowedWorkspace(workspacePath: string): Promise<string> {
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
