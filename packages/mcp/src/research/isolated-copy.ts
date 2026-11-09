// The coder adapter's isolated working copy. The agent edits and runs commands only here; the
// user's working tree is read once, to make the copy, and never written.
//
// A git workspace becomes `git worktree add --detach <dir>/freellama-coder HEAD` (hooks off), with
// the caller's uncommitted changes to tracked files (`git diff HEAD`) applied on top. Untracked
// files are not copied. Any other directory is copied whole, except node_modules, target, dist and
// .git, up to a size cap, and gets a private git directory outside the copy. Either way the
// starting point is recorded as a tree object, so the returned diff holds only the agent's changes.
//
// Every internal git command names its git directory and work tree explicitly: a `.git` file the
// agent rewrote inside the copy is never followed. Dependency directories (node_modules) are
// linked entry by entry from the workspace so allowed commands can run; the links stay out of the
// diff, and a write through one leaves the copy, which the run sandbox refuses.
import { spawnSync } from "node:child_process";
import {
  cpSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, symlinkSync, writeFileSync, type Dirent,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { ValidationError } from "./errors.js";

/** Directory name of the worktree inside the copy directory; git names its metadata after it. */
export const WORKTREE_NAME = "freellama-coder";
/** Trees a plain copy leaves out. */
export const COPY_SKIPPED = ["node_modules", "target", "dist", ".git"];
export const DEFAULT_MAX_COPY_BYTES = 200 * 1024 * 1024;
/** Dot-directories of node_modules that hold tool caches, not packages: never linked. */
const LINK_SKIPPED_DOT = new Set([".bin", ".pnpm"]);
const LINK_MAX_DEPTH = 4;
const LINK_MAX_DIRS = 32;
const GIT_MAX_BUFFER = 512 * 1024 * 1024;
/** Settings for every internal git command: no hooks, no filesystem monitor. */
const GIT_FLAGS = ["-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"];

export interface IsolatedCopy {
  kind: "git_worktree" | "copy";
  /** The directory that holds everything below; removed whole afterwards. */
  container: string;
  /** The copy's top: the repository top for a worktree, the workspace copy otherwise. */
  top: string;
  /** Where the agent works: the copy of the workspace directory itself. */
  root: string;
  /** `root` relative to `top` ("" when the workspace is the top). */
  relative: string;
  /** Scratch the allowed commands may write (their TMPDIR). */
  tmp: string;
  /** Private directory for sandbox profiles; not writable by commands. */
  privateDir: string;
  /** Environment of internal git commands: explicit GIT_DIR and GIT_WORK_TREE. */
  gitEnv: NodeJS.ProcessEnv;
  /** The git directory shared with the user's repository, for the read-only shell (worktree only). */
  commonDir: string | null;
  /** Tree object of the starting point: HEAD plus the caller's uncommitted changes. */
  baseTree: string;
  head: string | null;
  callerChanges: boolean;
  /** Dependency directories linked from the workspace, relative to `top`. */
  linked: string[];
}

export interface ChangedFile {
  path: string;
  status: "modified" | "added" | "deleted" | "type_changed";
}

export interface AgentChanges {
  /** Raw patch bytes (binary hunks included), relative to the workspace. */
  patch: Buffer;
  files: ChangedFile[];
}

/** The inherited environment without GIT_* variables, which would redirect internal git commands. */
function baseGitEnv(): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value !== undefined && !key.startsWith("GIT_")) env[key] = value;
  }
  // No opportunistic index refresh: reading the user's repository must not write its index.
  return { ...env, GIT_TERMINAL_PROMPT: "0", GIT_OPTIONAL_LOCKS: "0", LC_ALL: "C" };
}

interface GitResult { status: number; stdout: Buffer; stderr: string }

function git(args: string[], cwd: string, env: NodeJS.ProcessEnv = baseGitEnv(), input?: Buffer): GitResult {
  const result = spawnSync("git", [...GIT_FLAGS, ...args], { cwd, env, input, maxBuffer: GIT_MAX_BUFFER, stdio: ["pipe", "pipe", "pipe"] });
  if (result.error) throw new ValidationError(`git ${args[0]} could not run: ${result.error.message}`);
  return { status: result.status ?? 1, stdout: result.stdout, stderr: result.stderr.toString("utf8").trim() };
}

function gitOk(args: string[], cwd: string, env?: NodeJS.ProcessEnv, input?: Buffer): Buffer {
  const result = git(args, cwd, env, input);
  if (result.status !== 0) throw new ValidationError(`git ${args.join(" ")} failed: ${result.stderr || `exit ${result.status}`}`);
  return result.stdout;
}

const line = (bytes: Buffer) => bytes.toString("utf8").trim();

/** The repository top of `dir`, or null when it is not inside a work tree with a commit. */
function repositoryTop(dir: string): string | null {
  const top = git(["rev-parse", "--show-toplevel"], dir);
  if (top.status !== 0) return null;
  if (git(["rev-parse", "--verify", "--quiet", "HEAD"], dir).status !== 0) return null;
  return realpathSync(line(top.stdout));
}

/** The shared git directory of the repository at `top`, absolute. */
function commonDirOf(top: string): string {
  const raw = line(gitOk(["rev-parse", "--git-common-dir"], top));
  return realpathSync(path.resolve(top, raw));
}

/** Total bytes of regular files under `dir`, skipping `skipped` names; throws past `limit`. */
function treeBytes(dir: string, limit: number): number {
  let total = 0;
  const walk = (current: string) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      if (COPY_SKIPPED.includes(entry.name)) continue;
      const absolute = path.join(current, entry.name);
      if (entry.isDirectory()) walk(absolute);
      else if (entry.isFile()) {
        total += lstatSync(absolute).size;
        if (total > limit) {
          throw new ValidationError(
            `the workspace is larger than the ${limit >= 1024 * 1024 ? `${Math.round(limit / 1024 / 1024)} MB` : `${limit}-byte`} copy limit (node_modules, target, dist and .git excluded); ` +
              "point workspacePath at a smaller directory, or make it a git repository so a worktree is used instead",
          );
        }
      }
    }
  };
  walk(dir);
  return total;
}

/**
 * Link each `node_modules` directory of the workspace (up to four levels deep) into the copy,
 * entry by entry, where the copy has none. The directory itself is real, so a tool's cache
 * (`node_modules/.vite`) is written inside the copy; the packages resolve through the links.
 */
export function linkDependencies(sourceTop: string, copyTop: string): string[] {
  const linked: string[] = [];
  const walk = (rel: string, depth: number) => {
    if (depth > LINK_MAX_DEPTH || linked.length >= LINK_MAX_DIRS) return;
    let entries: Dirent[];
    try {
      entries = readdirSync(path.join(sourceTop, rel), { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      const child = rel ? path.join(rel, entry.name) : entry.name;
      if (entry.name === "node_modules") {
        const target = path.join(copyTop, child);
        if (existsSync(target) || !existsSync(path.join(copyTop, rel))) continue;
        mkdirSync(target);
        for (const item of readdirSync(path.join(sourceTop, child), { withFileTypes: true })) {
          if (item.name.startsWith(".") && item.isDirectory() && !LINK_SKIPPED_DOT.has(item.name)) continue;
          symlinkSync(path.join(sourceTop, child, item.name), path.join(target, item.name));
        }
        linked.push(child);
        if (linked.length >= LINK_MAX_DIRS) return;
        continue;
      }
      if (entry.name.startsWith(".") || COPY_SKIPPED.includes(entry.name)) continue;
      walk(child, depth + 1);
    }
  };
  walk("", 0);
  return linked;
}

/** A new empty directory for one copy, under the system temp directory, by its real path. */
export function newCopyContainer(): string {
  return realpathSync(mkdtempSync(path.join(tmpdir(), "freellama-coder-")));
}

/**
 * Make the isolated copy of `workspace` inside `container` (an empty directory, created when
 * missing). On failure everything made so far is removed before the error is thrown.
 */
export function createIsolatedCopy(workspace: string, container: string, options: { maxCopyBytes?: number; linkDependencies?: boolean } = {}): IsolatedCopy {
  const source = realpathSync(workspace);
  mkdirSync(container, { recursive: true });
  const box = realpathSync(container);
  if (readdirSync(box).length > 0) throw new ValidationError(`copy directory ${box} is not empty`);
  const tmp = path.join(box, "tmp");
  const privateDir = path.join(box, "private");
  mkdirSync(tmp);
  mkdirSync(privateDir, { mode: 0o700 });
  try {
    const top = repositoryTop(source);
    const copy = top ? worktreeCopy(source, top, box) : plainCopy(source, box, options.maxCopyBytes ?? DEFAULT_MAX_COPY_BYTES);
    const linked = options.linkDependencies === false ? [] : linkDependencies(top ?? source, copy.top);
    return { ...copy, container: box, tmp, privateDir, linked };
  } catch (error) {
    removeIsolatedCopy(source, box);
    throw error;
  }
}

type CopyCore = Omit<IsolatedCopy, "container" | "tmp" | "privateDir" | "linked">;

function worktreeCopy(source: string, repoTop: string, box: string): CopyCore {
  const work = path.join(box, WORKTREE_NAME);
  gitOk(["worktree", "add", "--detach", "--quiet", work, "HEAD"], repoTop);
  const top = realpathSync(work);
  const gitDir = realpathSync(line(gitOk(["rev-parse", "--absolute-git-dir"], top)));
  const gitEnv = { ...baseGitEnv(), GIT_DIR: gitDir, GIT_WORK_TREE: top };
  const head = line(gitOk(["rev-parse", "HEAD"], top, gitEnv));
  // The caller's uncommitted changes to tracked files, staged or not; submodule pointers excluded.
  const changes = gitOk(["diff", "HEAD", "--binary", "--no-color", "--no-ext-diff", "--no-textconv", "--ignore-submodules=all",
    "--src-prefix=a/", "--dst-prefix=b/"], repoTop);
  if (changes.length > 0) {
    const applied = git(["apply", "--index", "--whitespace=nowarn", "-"], top, gitEnv, changes);
    if (applied.status !== 0) {
      throw new ValidationError(`could not reproduce your uncommitted changes in the isolated copy (${applied.stderr}); commit or stash them and retry`);
    }
  }
  const baseTree = line(gitOk(["write-tree"], top, gitEnv));
  const relative = path.relative(repoTop, source);
  if (relative && !existsSync(path.join(top, relative))) {
    throw new ValidationError(`${relative} holds no file tracked at HEAD, so a worktree of the repository has no copy of it; commit its files, or point workspacePath at a tracked directory`);
  }
  return {
    kind: "git_worktree", top, root: relative ? path.join(top, relative) : top, relative, gitEnv,
    commonDir: commonDirOf(repoTop), baseTree, head, callerChanges: changes.length > 0,
  };
}

function plainCopy(source: string, box: string, maxCopyBytes: number): CopyCore {
  treeBytes(source, maxCopyBytes);
  const top = path.join(box, "copy");
  cpSync(source, top, {
    recursive: true,
    verbatimSymlinks: true,
    filter: (from) => from === source || !COPY_SKIPPED.includes(path.basename(from)),
  });
  // A private repository outside the copy records the starting point; it reads no user config.
  const gitDir = path.join(box, "private", "git");
  const gitEnv = { ...baseGitEnv(), GIT_DIR: gitDir, GIT_WORK_TREE: top, GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null" };
  gitOk(["-c", "init.templateDir=", "init", "--quiet"], top, gitEnv);
  mkdirSync(path.join(gitDir, "info"), { recursive: true });
  writeFileSync(path.join(gitDir, "info", "exclude"), COPY_SKIPPED.map((name) => `${name}/`).join("\n") + "\n");
  gitOk(["-c", "core.autocrlf=false", "add", "-A", "."], top, gitEnv);
  const baseTree = line(gitOk(["write-tree"], top, gitEnv));
  return { kind: "copy", top, root: top, relative: "", gitEnv, commonDir: null, baseTree, head: null, callerChanges: false };
}

const STATUS: Record<string, ChangedFile["status"]> = { M: "modified", A: "added", D: "deleted", T: "type_changed" };

/** The agent's changes since the starting point, relative to the workspace directory. */
export function agentChanges(copy: IsolatedCopy): AgentChanges {
  const scope = copy.relative ? [`--relative=${copy.relative}`] : [];
  // Linked dependency directories stay out of the diff. Most are ignored already, and naming an
  // ignored path, even to exclude it, makes `git add` fail.
  const excluded = copy.linked
    .filter((rel) => git(["check-ignore", "-q", "--", rel], copy.top, copy.gitEnv).status !== 0)
    .map((rel) => `:(exclude)${rel}`);
  gitOk(["-c", "core.autocrlf=false", "add", "-A", "--", copy.relative || ".", ...excluded], copy.top, copy.gitEnv);
  const common = ["diff", "--cached", "--no-renames", "--no-color", "--no-ext-diff", "--no-textconv", ...scope];
  const patch = gitOk([...common, "--binary", "--src-prefix=a/", "--dst-prefix=b/", copy.baseTree], copy.top, copy.gitEnv);
  const names = gitOk([...common, "--name-status", "-z", copy.baseTree], copy.top, copy.gitEnv).toString("utf8").split("\0");
  const files: ChangedFile[] = [];
  for (let index = 0; index + 1 < names.length; index += 2) {
    files.push({ path: names[index + 1]!, status: STATUS[names[index]!.charAt(0)] ?? "modified" });
  }
  return { patch, files };
}

/**
 * Remove the copy in `container`: `git worktree remove --force` for a worktree, then the directory
 * itself. When git cannot remove it (the copy is already gone, or its `.git` file was rewritten),
 * only this worktree's own metadata entry is pruned: a repository-wide `git worktree prune` would
 * also drop the user's worktrees on unmounted volumes. Idempotent; never throws.
 */
export function removeIsolatedCopy(workspace: string, container: string): void {
  const work = path.join(container, WORKTREE_NAME);
  let commonDir: string | null = null;
  try {
    const top = repositoryTop(realpathSync(workspace));
    commonDir = top ? commonDirOf(top) : null;
    if (top && existsSync(work)) git(["worktree", "remove", "--force", work], top);
  } catch {
    // Not a repository, or git is unavailable: the directory removal below still runs.
  }
  try {
    rmSync(container, { recursive: true, force: true });
  } catch {
    // Best effort; the directory is under the system temp directory.
  }
  if (!commonDir) return;
  const entries = path.join(commonDir, "worktrees");
  let names: string[];
  try {
    names = readdirSync(entries);
  } catch {
    return;
  }
  const ours = new Set([path.join(work, ".git"), path.join(container, WORKTREE_NAME, ".git")]);
  for (const name of names) {
    try {
      if (ours.has(readFileSync(path.join(entries, name, "gitdir"), "utf8").trim())) {
        rmSync(path.join(entries, name), { recursive: true, force: true });
      }
    } catch {
      // Another worktree's entry, or one that is already gone.
    }
  }
}
