// Read-only execution of model-written shell commands.
//
// `delegate_research` runs the bash adapter against the caller's *real* workspace, so a command is
// untrusted input with the user's privileges. Confinement is layered so that no single check
// carries the whole guarantee:
//
// 1. Command validation (always): every pipeline segment starts with an allowlisted read-only tool,
//    tool flags that write or execute (`find -delete`, `sed -i`, `rg --pre` ...) are refused, and
//    shell features that hide a command or a path from validation (command/process substitution,
//    `$`-expansion, ANSI-C quoting, redirection, subshells, background jobs, brace expansion) are
//    refused. Paths, including glob matches, must resolve inside the workspace.
// 2. Restricted bash (always): `bash --restricted` with PATH set to a private directory of symlinks
//    to the allowlisted tools. rbash refuses output redirection, `cd`, slashed command names and
//    PATH changes, so a validation gap still cannot write through the shell or run another binary.
// 3. Scrubbed environment (always): the child gets the locale, a dummy HOME (the workspace) and a
//    private TMPDIR. MCP-server secrets are not inherited.
// 4. OS sandbox (when the host provides one): `sandbox-exec` on macOS (no network, no file writes
//    outside the scratch dir, no reads of other home directories) or `bwrap` on Linux (read-only
//    bind of the workspace, no network). Each is probed once and skipped when unavailable.
//
// Layer 1 lives in sandbox-validate.ts; the differential corpus in
// test/fixtures/shell-sandbox-corpus.json pins every verdict.
import { accessSync, constants as fsConstants, mkdirSync, mkdtempSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { probe, runProcess } from "./proc.js";
import { READ_ONLY_TOOLS, validateCommand } from "./sandbox-validate.js";
import { trimSpace } from "./text.js";
import { isDir, resolvePath } from "./workspace-path.js";

export {
  READ_ONLY_GIT, READ_ONLY_TOOLS, SAFE_BUILTINS, assertCommandConfined, segments, validateCommand,
} from "./sandbox-validate.js";

function macosProfile(workspace: string, scratch: string): string {
  const quote = (value: string) => `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`;
  return `(version 1)
(allow default)
(deny network*)
(deny file-write*)
(allow file-write* (literal "/dev/null") (literal "/dev/tty") (literal "/dev/dtracehelper") (subpath ${quote(scratch)}))
(deny file-read-data (subpath "/Users") (subpath "/Volumes") (subpath "/private/var/root"))
(allow file-read-data (subpath ${quote(workspace)}) (subpath ${quote(scratch)}))
`;
}

/** The first executable `name` on `envPath` (default: the process PATH). */
export function which(name: string, envPath: string | undefined = process.env.PATH): string | null {
  const searchPath = envPath ?? "/bin:/usr/bin";
  if (!searchPath) return null;
  const seen = new Set<string>();
  for (const directory of searchPath.split(":")) {
    if (seen.has(directory)) continue;
    seen.add(directory);
    const candidate = directory ? (directory.endsWith("/") ? directory + name : `${directory}/${name}`) : name;
    try {
      accessSync(candidate, fsConstants.X_OK);
    } catch {
      continue;
    }
    if (!isDir(candidate)) return candidate;
  }
  return null;
}

const KEPT_ENV = new Set(["LANG", "LC_ALL", "LC_CTYPE", "TZ"]);

/** A reusable read-only shell rooted at `workspace`. Call `enter()` before `run`, `close()` after. */
export class ReadOnlyShell {
  workspace: string;
  kind = "restricted-bash";
  prefix: string[] = [];
  env: Record<string, string> = {};
  bash = "/bin/bash";
  private scratch: string | null = null;

  constructor(workspace: string) {
    this.workspace = workspace;
  }

  enter(): this {
    this.workspace = resolvePath(this.workspace);
    const scratch = mkdtempSync(path.join(path.resolve(tmpdir()), "freellama-shell-"));
    this.scratch = scratch;
    const binDir = path.join(scratch, "bin");
    mkdirSync(binDir);
    // Resolve tools against the caller's PATH once, before the environment is scrubbed.
    for (const tool of READ_ONLY_TOOLS) {
      const located = which(tool);
      if (located) symlinkSync(realpathSync(located), path.join(binDir, tool));
    }
    this.bash = which("bash") ?? "/bin/bash";
    const env: Record<string, string> = {};
    for (const [key, value] of Object.entries(process.env)) {
      if (KEPT_ENV.has(key) && value !== undefined) env[key] = value;
    }
    Object.assign(env, {
      PATH: binDir,
      HOME: this.workspace,
      TMPDIR: scratch,
      TERM: "dumb",
      PAGER: "cat",
      GIT_PAGER: "cat",
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_OPTIONAL_LOCKS: "0", // `git status` otherwise rewrites the index
      GIT_TERMINAL_PROMPT: "0",
    });
    this.env = env;
    this.prefix = this.osSandbox(scratch, binDir);
    return this;
  }

  close(): void {
    if (this.scratch !== null) {
      rmSync(this.scratch, { recursive: true, force: true });
      this.scratch = null;
    }
  }

  private osSandbox(scratch: string, binDir: string): string[] {
    if ((process.env.FREELLAMA_AGENT_OS_SANDBOX ?? "auto") === "off") return [];
    const sandboxExec = which("sandbox-exec");
    if (sandboxExec) {
      const profile = path.join(scratch, "profile.sb");
      writeFileSync(profile, macosProfile(this.workspace, scratch), "utf8");
      const prefix = [sandboxExec, "-f", profile];
      if (probe([...prefix, this.bash, "-c", "true"], this.env)) {
        this.kind = "sandbox-exec+restricted-bash";
        return prefix;
      }
    }
    const bwrap = which("bwrap");
    if (bwrap) {
      const prefix = [bwrap, "--unshare-all", "--die-with-parent", "--new-session", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"];
      for (const systemDir of ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc/alternatives", "/etc/ld.so.cache", "/etc/localtime", "/nix/store"]) {
        prefix.push("--ro-bind-try", systemDir, systemDir);
      }
      prefix.push(
        "--ro-bind", this.workspace, this.workspace,
        "--ro-bind", binDir, binDir,
        "--bind", scratch, scratch,
        "--chdir", this.workspace, "--",
      );
      if (probe([...prefix, this.bash, "-c", "true"], this.env)) {
        this.kind = "bwrap+restricted-bash";
        return prefix;
      }
    }
    return [];
  }

  async run(commandText: string, timeoutSeconds: number): Promise<string> {
    const command = validateCommand(this.workspace, commandText);
    const result = await runProcess(
      [...this.prefix, this.bash, "--restricted", "--noprofile", "--norc", "-c", command],
      { cwd: this.workspace, env: this.env, timeoutSeconds },
    );
    let output = result.stdout + result.stderr;
    if (!trimSpace(output)) output = `(no output, exit code ${result.returncode})`;
    return output;
  }
}
