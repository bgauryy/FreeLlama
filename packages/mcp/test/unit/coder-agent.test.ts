// The coder adapter: exact-once edits, confined writes, the run allowlist, bounded run output, and
// a whole session against a scripted model in an isolated copy that leaves the user's tree alone.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { runAdapter } from "../../src/research/agent-loop.js";
import {
  CommandRunner, allowedCommand, applyEdit, headTail, normalizeAllowedCommands, secretPaths, writeWholeFile, WRITE_MAX_BYTES,
} from "../../src/research/coder-actions.js";
import { coderAdapter, parseAction, type CoderOptions } from "../../src/research/coder-agent.js";
import { WORKTREE_NAME, createIsolatedCopy, removeIsolatedCopy } from "../../src/research/isolated-copy.js";
import { coderRefusal } from "../../src/tools/coder.js";

const temp = (prefix: string) => realpathSync(mkdtempSync(path.join(tmpdir(), prefix)));

function git(cwd: string, ...args: string[]): string {
  return execFileSync("git", ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", ...args], {
    cwd, encoding: "utf8", env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1" },
  });
}

/** A repository with two committed files and one uncommitted change of the caller's. */
function callerRepo(): string {
  const repo = temp("freellama-coder-repo-");
  git(repo, "init", "-q");
  mkdirSync(path.join(repo, "sub"));
  writeFileSync(path.join(repo, "file.txt"), "alpha\nbeta\ngamma\n");
  writeFileSync(path.join(repo, "notes.txt"), "one\n");
  writeFileSync(path.join(repo, "sub", "inner.txt"), "inside\n");
  git(repo, "add", ".");
  git(repo, "commit", "-q", "-m", "init");
  writeFileSync(path.join(repo, "notes.txt"), "one\ntwo\n");
  return repo;
}

interface Session {
  result: Record<string, any>;
  sent: string[];
  exit: number;
}

/** Run one coder session with `replies` as the model's turns. */
async function session(workspace: string, replies: unknown[], options: Partial<CoderOptions> = {}): Promise<Session> {
  const io = temp("freellama-coder-io-");
  writeFileSync(path.join(io, "prompt.txt"), "Change beta to BETA in file.txt and check it.");
  const sent: string[] = [];
  const requestJson = async (_url: string, payload: unknown) => {
    sent.push((payload as { messages: { content: string }[] }).messages.at(-1)!.content);
    return { message: { content: JSON.stringify(replies.shift() ?? { action: "finish", answer: "out of replies" }) } };
  };
  const env = {
    FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: workspace, FREELLAMA_BENCH_PROMPT: path.join(io, "prompt.txt"),
    FREELLAMA_AGENT_RESULT: path.join(io, "result.json"), FREELLAMA_AGENT_MAX_TURNS: "8",
  };
  const adapter = coderAdapter({ commands: ["cat file.txt"], allowUnsandboxedRun: true, patchPath: path.join(io, "agent.patch"), ...options });
  const exit = await runAdapter(adapter, { env, requestJson, print: () => undefined });
  return { result: JSON.parse(readFileSync(path.join(io, "result.json"), "utf8")), sent, exit };
}

const worktrees = (repo: string) => git(repo, "worktree", "list", "--porcelain").split("\n").filter((line) => line.startsWith("worktree "));

describe("coder edit", () => {
  const root = temp("freellama-coder-edit-");
  writeFileSync(path.join(root, "a.ts"), "const x = 1;\nconst y = 2;\nconst x2 = 1;\n");

  it("replaces text that matches exactly once and shows the new lines", () => {
    const out = applyEdit(root, { path: "a.ts", search: "const y = 2;", replace: "const y = 3;" });
    expect(out).toMatch(/edited a\.ts: 1 line\(s\) at line 2 replaced by 1 line\(s\)\n2  const y = 3;/);
    expect(readFileSync(path.join(root, "a.ts"), "utf8")).toBe("const x = 1;\nconst y = 3;\nconst x2 = 1;\n");
  });

  it("refuses a search with no match or several, naming the count and changing nothing", () => {
    expect(() => applyEdit(root, { path: "a.ts", search: "const z", replace: "q" })).toThrow(/not found in a\.ts \(0 matches\)/);
    expect(() => applyEdit(root, { path: "a.ts", search: " = 1;", replace: " = 9;" })).toThrow(/matches 2 times in a\.ts \(lines 1, 3\)/);
    expect(readFileSync(path.join(root, "a.ts"), "utf8")).toBe("const x = 1;\nconst y = 3;\nconst x2 = 1;\n");
  });

  it("points at the line when only the indentation differs", () => {
    writeFileSync(path.join(root, "b.ts"), "function f() {\n  return 1;\n}\n");
    expect(() => applyEdit(root, { path: "b.ts", search: "    return 1;", replace: "    return 2;" })).toThrow(/matches line 2 if indentation is ignored/);
  });

  it("edits a CRLF file with a search copied as bare newlines", () => {
    writeFileSync(path.join(root, "crlf.txt"), "a\r\nb\r\nc\r\n");
    applyEdit(root, { path: "crlf.txt", search: "a\nb", replace: "a\nB" });
    expect(readFileSync(path.join(root, "crlf.txt"), "utf8")).toBe("a\r\nB\r\nc\r\n");
  });

  it("parses edit, write and run actions and rejects a no-op edit", () => {
    expect(parseAction('{"action":"edit","path":"a.ts","search":"x","replace":""}')).toMatchObject({ action: "edit", replace: "" });
    expect(() => parseAction('{"action":"edit","path":"a.ts","search":"x","replace":"x"}')).toThrow(/identical/);
    expect(() => parseAction('{"action":"edit","path":"a.ts","replace":"x"}')).toThrow(/non-empty string search/);
    expect(() => parseAction('{"action":"write","path":"a.ts"}')).toThrow(/string content/);
    expect(() => parseAction('{"action":"delete","path":"a.ts"}')).toThrow(/unsupported action for coder adapter/);
  });
});

describe("coder write confinement", () => {
  const root = temp("freellama-coder-write-");
  const outside = temp("freellama-coder-outside-");
  symlinkSync(outside, path.join(root, "escape"));

  it("creates nested files inside the copy", () => {
    expect(writeWholeFile(root, { path: "src/new/file.ts", content: "export {};\n" })).toBe("created src/new/file.ts (1 lines, 11 bytes)");
    expect(readFileSync(path.join(root, "src", "new", "file.ts"), "utf8")).toBe("export {};\n");
  });

  it("refuses paths outside the copy, through a symlink, inside .git, or over the size cap", () => {
    expect(() => writeWholeFile(root, { path: "../x.txt", content: "" })).toThrow(/outside the copy/);
    expect(() => writeWholeFile(root, { path: path.join(outside, "x.txt"), content: "" })).toThrow(/outside the copy/);
    expect(() => writeWholeFile(root, { path: "escape/x.txt", content: "" })).toThrow(/leaves the copy through a symlink/);
    expect(() => writeWholeFile(root, { path: ".git/config", content: "" })).toThrow(/inside \.git/);
    expect(() => writeWholeFile(root, { path: "big.txt", content: "x".repeat(WRITE_MAX_BYTES + 1) })).toThrow(/at most 262144/);
    expect(() => applyEdit(root, { path: "../a.ts", search: "a", replace: "b" })).toThrow(/outside the workspace/);
    expect(readdirSync(outside)).toEqual([]);
  });
});

describe("coder run allowlist", () => {
  const allowed = ["yarn test:js", "cargo test -p freellama-core"];

  it("runs only a command equal to an allowed one", () => {
    expect(allowedCommand("yarn test:js", allowed)).toBe("yarn test:js");
    expect(allowedCommand("  cargo test -p freellama-core ", allowed)).toBe("cargo test -p freellama-core");
    for (const near of ["yarn test:js --watch", "yarn  test:js", "yarn test:js; rm -rf .", "YARN TEST:JS", "cargo test"]) {
      expect(() => allowedCommand(near, allowed), near).toThrow(/command not allowed.*Run exactly one of: "yarn test:js", "cargo test -p freellama-core"/);
    }
    expect(() => allowedCommand("yarn test:js", [])).toThrow(/no commands are allowed/);
  });

  it("accepts at most ten one-line commands", () => {
    expect(normalizeAllowedCommands([" a ", "a", "b"])).toEqual(["a", "b"]);
    expect(() => normalizeAllowedCommands(Array.from({ length: 11 }, (_, index) => `c${index}`))).toThrow(/at most 10/);
    expect(() => normalizeAllowedCommands(["a\nb"])).toThrow(/one non-empty line/);
  });

  it("is opt-in for the operator and refuses commands for other adapters", () => {
    expect(coderRefusal("coder", ["yarn test"], {})?.message).toMatch(/FREELLAMA_MCP_ALLOW_CODER=1/);
    expect(coderRefusal("coder", ["yarn test"], { FREELLAMA_MCP_ALLOW_CODER: "1" })).toBeUndefined();
    expect(coderRefusal("bash", ["yarn test"], { FREELLAMA_MCP_ALLOW_CODER: "1" })?.message).toMatch(/only to adapter "coder"/);
    expect(coderRefusal("bash", undefined, {})).toBeUndefined();
  });
});

describe("coder run sandbox", () => {
  it("finds credential locations under HOME and, when sandboxed, hides them from an allowed command", async () => {
    const home = temp("freellama-coder-home-");
    mkdirSync(path.join(home, ".ssh"));
    writeFileSync(path.join(home, ".ssh", "id_test"), "SECRET-KEY\n");
    writeFileSync(path.join(home, ".netrc"), "machine x password SECRET-NETRC\n");
    const hidden = secretPaths({ HOME: home });
    expect(hidden.sort()).toEqual([path.join(home, ".netrc"), path.join(home, ".ssh")].sort());
    const work = temp("freellama-coder-run-");
    const privateDir = temp("freellama-coder-private-");
    const runner = new CommandRunner(work, [work], { PATH: process.env.PATH }, { privateDir, osSandbox: "auto", allowUnsandboxed: true, hidden }).enter();
    const outcome = await runner.run(`cat ${home}/.ssh/id_test ${home}/.netrc; echo ok > inside.txt; echo end`, 30);
    expect(outcome.observed).toMatch(/end/);
    expect(readFileSync(path.join(work, "inside.txt"), "utf8")).toBe("ok\n");
    if (runner.kind !== "none") {
      expect(outcome.observed).not.toMatch(/SECRET/);
    }
  });

  it("refuses to run without an OS sandbox unless the operator allows it", async () => {
    const work = temp("freellama-coder-nosandbox-");
    const runner = new CommandRunner(work, [work], { PATH: process.env.PATH }, { privateDir: work, osSandbox: "off", allowUnsandboxed: false }).enter();
    expect(runner.kind).toBe("none");
    await expect(runner.run("true", 5)).rejects.toThrow(/no OS sandbox.*FREELLAMA_CODER_UNSANDBOXED_RUN=1/);
  });

  it("kills a command at its timeout and keeps its output", async () => {
    const work = temp("freellama-coder-timeout-");
    const runner = new CommandRunner(work, [work], { PATH: process.env.PATH }, { privateDir: work, osSandbox: "off", allowUnsandboxed: true }).enter();
    const outcome = await runner.run("echo started; sleep 30", 0.5);
    expect(outcome.timedOut).toBe(true);
    expect(outcome.observed).toMatch(/started/);
    expect(outcome.durationMs).toBeLessThan(10_000);
  });
});

describe("coder run output", () => {
  const options = { head: 10, tail: 40, lineChars: 20 };

  it("keeps the head and tail with an omitted-lines marker", () => {
    const text = Array.from({ length: 100 }, (_, index) => `line ${index + 1}`).join("\n") + "\n";
    const lines = headTail(text, options).split("\n");
    expect(lines).toHaveLength(51);
    expect(lines[9]).toBe("line 10");
    expect(lines[10]).toBe("… 50 lines omitted …");
    expect(lines[11]).toBe("line 61");
    expect(lines.at(-1)).toBe("line 100");
  });

  it("leaves short output whole, clips long lines, and counts lines dropped during capture", () => {
    expect(headTail("a\nb\n", options)).toBe("a\nb");
    expect(headTail("x".repeat(30), options)).toBe(`${"x".repeat(20)}…`);
    expect(headTail("a\nb", options, { line: 1, lines: 7 })).toBe("a\n… 7 lines omitted …\nb");
    const long = Array.from({ length: 60 }, (_, index) => `l${index}`).join("\n");
    expect(headTail(long, options, { line: 15, lines: 1000 }).split("\n")[10]).toBe("… 1010 lines omitted …");
  });
});

describe("coder session in an isolated copy", () => {
  it("edits and runs in a worktree, returns only the agent's diff, and leaves the user's tree and git metadata as they were", async () => {
    const repo = callerRepo();
    const statusBefore = git(repo, "status", "--porcelain");
    const { result, sent, exit } = await session(repo, [
      { action: "edit", path: "file.txt", search: "beta", replace: "BETA" },
      { action: "shell", command: "git diff --stat" },
      { action: "run", command: "cat file.txt" },
      { action: "run", command: "rm file.txt" },
      { action: "finish", answer: "Changed beta to BETA; cat shows it." },
    ]);
    expect(exit).toBe(0);
    expect(result.final_answer).toBe("Changed beta to BETA; cat shows it.");
    // The read-only shell reads the worktree's git metadata: only the agent's change is unstaged.
    expect(sent[2]).toMatch(/file\.txt \| 2 \+-/);
    expect(sent[2]).not.toMatch(/notes\.txt/);
    // The run saw the edited copy.
    expect(sent[3]).toMatch(/\$ cat file\.txt\nexit code 0 \([\d.]+ s\)\nalpha\nBETA\ngamma/);
    expect(sent[4]).toMatch(/command not allowed: "rm file\.txt"/);
    // A finish naming no file is not sent back for a source.
    expect(sent.some((message) => /names no source file/.test(message))).toBe(false);
    const coder = result.coder;
    expect(coder.isolation).toBe("git_worktree");
    expect(coder.base.caller_changes).toBe(true);
    expect(coder.changed_files).toEqual([{ path: "file.txt", status: "modified" }]);
    expect(coder.diff).toContain("-beta\n+BETA\n");
    expect(coder.diff).not.toContain("notes.txt");
    expect(coder.runs).toMatchObject([{ command: "cat file.txt", exit_code: 0, timed_out: false }]);
    expect(result.tool_calls.map((call: { name: string; status: string }) => `${call.name}:${call.status}`)).toEqual(["edit:ok", "shell:ok", "run:ok", "run:error"]);
    // The user's tree is untouched, and the copy and its worktree entry are gone.
    expect(readFileSync(path.join(repo, "file.txt"), "utf8")).toBe("alpha\nbeta\ngamma\n");
    expect(readFileSync(path.join(repo, "notes.txt"), "utf8")).toBe("one\ntwo\n");
    expect(git(repo, "status", "--porcelain")).toBe(statusBefore);
    expect(worktrees(repo)).toEqual([`worktree ${repo}`]);
    expect(existsSync(path.join(repo, ".git", "worktrees")) ? readdirSync(path.join(repo, ".git", "worktrees")) : []).toEqual([]);
    // The saved patch applies to the user's tree as it is.
    expect(coder.patch_file).toMatch(/agent\.patch$/);
    git(repo, "apply", "--check", coder.patch_file);
  });

  it("lets an allowed command write the copy but, under an OS sandbox, nothing outside it", async () => {
    const repo = callerRepo();
    const outside = temp("freellama-coder-target-");
    const command = `echo y > made.txt && echo x > ${outside}/escape.txt; echo z > ${repo}/pwned.txt; echo done`;
    const { result } = await session(repo, [{ action: "run", command }, { action: "finish", answer: "Ran it." }], { commands: [command] });
    expect(result.coder.changed_files).toEqual([{ path: "made.txt", status: "added" }]);
    expect(existsSync(path.join(repo, "made.txt"))).toBe(false);
    if (result.coder.run_sandbox !== "none") {
      expect(result.coder.runs[0].output).toMatch(/done/);
      expect(existsSync(path.join(outside, "escape.txt"))).toBe(false);
      expect(existsSync(path.join(repo, "pwned.txt"))).toBe(false);
    }
  });

  it("scopes the diff to a subdirectory workspace so git -C <workspace> apply works", async () => {
    const repo = callerRepo();
    const workspace = path.join(repo, "sub");
    const { result } = await session(workspace, [
      { action: "write", path: "added.txt", content: "new\n" },
      { action: "edit", path: "inner.txt", search: "inside", replace: "inside!" },
      { action: "edit", path: "inner.txt", search: "inside", replace: "inside!" },
      { action: "finish", answer: "Added added.txt and edited inner.txt." },
    ]);
    expect(result.coder.changed_files).toEqual([{ path: "added.txt", status: "added" }, { path: "inner.txt", status: "modified" }]);
    expect(result.coder.diff).toContain("+++ b/inner.txt");
    expect(result.tool_calls.at(-1)).toMatchObject({ name: "edit", status: "error" });
    git(workspace, "apply", "--check", result.coder.patch_file);
    expect(existsSync(path.join(workspace, "added.txt"))).toBe(false);
  });

  it("copies a non-git workspace without node_modules, links its packages, and keeps them out of the diff", async () => {
    const workspace = temp("freellama-coder-plain-");
    writeFileSync(path.join(workspace, "file.txt"), "alpha\nbeta\ngamma\n");
    mkdirSync(path.join(workspace, "node_modules", "pkg"), { recursive: true });
    writeFileSync(path.join(workspace, "node_modules", "pkg", "index.js"), "module.exports = 1;\n");
    const container = temp("freellama-coder-box-");
    const { result } = await session(workspace, [
      { action: "list", path: "." },
      { action: "edit", path: "file.txt", search: "gamma", replace: "delta" },
      { action: "finish", answer: "Replaced gamma." },
    ], { container });
    expect(result.coder.isolation).toBe("copy");
    expect(result.coder.linked_dependencies).toEqual(["node_modules"]);
    expect(result.coder.changed_files).toEqual([{ path: "file.txt", status: "modified" }]);
    expect(result.coder.diff).not.toContain("node_modules");
    expect(readFileSync(path.join(workspace, "file.txt"), "utf8")).toBe("alpha\nbeta\ngamma\n");
    expect(existsSync(container)).toBe(false);
  });

  it("refuses a plain copy over the size cap and leaves nothing behind", () => {
    const workspace = temp("freellama-coder-big-");
    writeFileSync(path.join(workspace, "big.bin"), Buffer.alloc(4096));
    const container = path.join(temp("freellama-coder-cap-"), "box");
    expect(() => createIsolatedCopy(workspace, container, { maxCopyBytes: 1024 })).toThrow(/larger than the 1024-byte copy limit/);
    expect(existsSync(container)).toBe(false);
  });

  it("removes a worktree whose adapter was killed, by its own entry only", () => {
    const repo = callerRepo();
    const container = temp("freellama-coder-killed-");
    const copy = createIsolatedCopy(repo, container);
    expect(copy.top).toBe(path.join(container, WORKTREE_NAME));
    expect(worktrees(repo)).toHaveLength(2);
    removeIsolatedCopy(repo, container);
    removeIsolatedCopy(repo, container);
    expect(existsSync(container)).toBe(false);
    expect(worktrees(repo)).toEqual([`worktree ${repo}`]);
  });
});
