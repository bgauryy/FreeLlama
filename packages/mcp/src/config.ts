// Environment-derived configuration and the `delegate_research` workspace boundary.
// Every default is overridable via an env var so no deployment needs a recompile.
import { existsSync } from "node:fs";
import { realpath } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";

/**
 * Repo root, found by walking up to the directory containing `Cargo.toml`.
 *
 * Anchoring on `Cargo.toml` makes the checkout location a non-issue. A published install has
 * no marker in the tarball — do **not** fall back to a relative `../` guess, which from `dist/`
 * resolved to `node_modules` and silently became the default `ALLOWED_RESEARCH_ROOTS`.
 * Unmarked installs must set `FREELLAMA_MCP_ALLOWED_ROOTS`.
 */
function findRepoRoot(): string | undefined {
  const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  let dir = path.dirname(fileURLToPath(import.meta.url));
  for (let depth = 0; depth < 10; depth += 1) {
    // Only a FreeLlama checkout counts: this package must sit at <root>/packages/mcp. Any
    // Cargo.toml used to match, so an npm install inside some other Rust project silently made
    // that project the default research allowlist.
    if (existsSync(path.join(dir, "Cargo.toml")) && path.join(dir, "packages", "mcp") === packageRoot) return dir;
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  // No marker: a published install must not guess. The old relative fallback from `dist/`
  // resolved to `node_modules`, which silently became the default research allowlist.
  return undefined;
}

export const REPO_ROOT = findRepoRoot() ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
// Two interchangeable research adapters. Same env contract and result shape. In-repo
// measurements: bash tied or beat octocode on every model, and was faster — hence the default.
// `octocode` remains for structured search when asked. Numbers: benchmark/local/results.
export const PACKAGE_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
// `npm run build` copies adapters into adapters/ so a published tarball still has them.

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
// No hardcoded tag: the old default (`qwen3.8:27b-mlx`) exists only on Apple Silicon, so every
// Linux/Windows call without `model` failed. Unset means "route to an installed coding model".
// docs/MODEL_SELECTION.md owns the measured recommendation.
export const DEFAULT_DELEGATE_MODEL: string | undefined = process.env.FREELLAMA_MCP_DEFAULT_MODEL?.trim() || undefined;
export const DEFAULT_DELEGATE_MAX_TURNS = envInt("FREELLAMA_MCP_MAX_TURNS", 8);
export const DEFAULT_DELEGATE_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_DELEGATE_TIMEOUT_SECONDS", 180);
export const DEFAULT_PULL_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_PULL_TIMEOUT_SECONDS", 1200);
export const DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS = envInt("FREELLAMA_MCP_FETCH_TIMEOUT_SECONDS", 30);
export const DEFAULT_TOKEN_CALIBRATION_DIR =
  process.env.FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR ??
  path.join(
    process.env.XDG_DATA_HOME ??
      (process.platform === "win32"
        ? (process.env.LOCALAPPDATA ?? path.join(homedir(), "AppData", "Local"))
        : path.join(homedir(), ".local", "share")),
    "freellama",
    "token-calibration",
  );

// `delegate_research` grants a local model read access to whatever directory it's pointed at.
// Without a boundary, an orchestrator (or a bug, or a compromised orchestrator) could point it at
// $HOME or / and have a local model read arbitrary files on the machine — verified live: an
// unconstrained version of this tool happily listed a real $HOME (Desktop, Documents, Library...).
// Default to just this repo; extend via the platform path-list separator, never accept "anything".
// `path.delimiter` is `:` on POSIX and `;` on Windows, where splitting on `:` corrupts drive paths.
export function parseAllowedResearchRoots(raw: string, delimiter = path.delimiter): string[] {
  return raw
    .split(delimiter)
    .filter(Boolean)
    .map((root) => path.resolve(root));
}

const ALLOWED_RESEARCH_ROOTS = parseAllowedResearchRoots(
  process.env.FREELLAMA_MCP_ALLOWED_ROOTS ?? findRepoRoot() ?? "",
);

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
  if (roots.length === 0) {
    throw new Error(
      `workspacePath "${workspacePath}" is rejected because no research roots are configured. ` +
        "Set FREELLAMA_MCP_ALLOWED_ROOTS (a platform-separated list of absolute paths) to the directories " +
        "`delegate_research` may read. In a FreeLlama checkout this defaults to the repo root.",
    );
  }
  const allowed = roots.some(
    (root) => resolved === root || resolved.startsWith(`${root}${path.sep}`),
  );
  if (!allowed) {
    throw new Error(
      `workspacePath "${workspacePath}" resolves to "${resolved}", which is outside the allowed ` +
        `research roots (${roots.join(", ")}). Set FREELLAMA_MCP_ALLOWED_ROOTS ` +
        "(using the platform path-list separator) to extend this if you genuinely need to research another directory.",
    );
  }
  return resolved;
}

// The research adapter runs model-chosen commands, so it gets only what it needs: its own
// FREELLAMA_* settings, locale, temp and the proxy/CA settings `npx octocode` needs. Tokens and
// API keys in the MCP host's environment used to be inherited verbatim.
const DELEGATE_ENV_ALLOWLIST = new Set([
  "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR", "TEMP", "TMP",
  "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "PATHEXT", "APPDATA", "LOCALAPPDATA", "USERPROFILE",
  "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "no_proxy",
  "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "SSL_CERT_DIR", "REQUESTS_CA_BUNDLE",
  "XDG_CACHE_HOME", "XDG_DATA_HOME", "npm_config_cache", "PYTHONPATH",
]);

export function delegateEnvironment(source: NodeJS.ProcessEnv = process.env): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(source)) {
    if (value !== undefined && (DELEGATE_ENV_ALLOWLIST.has(key) || key.startsWith("FREELLAMA_"))) env[key] = value;
  }
  return env;
}
