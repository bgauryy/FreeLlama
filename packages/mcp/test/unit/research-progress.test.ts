// delegate_research progress: adapter events, the stderr line protocol, and their MCP messages.
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { runDelegate } from "../../src/lifecycle.js";
import {
  PrefixedLineFilter, describeOctocodeCall, describeShellCommand, researchProgressFilter, researchProgressMessage,
} from "../../src/research-progress.js";
import { runAdapter } from "../../src/research/agent-loop.js";
import { BASH_ADAPTER } from "../../src/research/bash-agent.js";
import { PROGRESS_ENV, PROGRESS_PREFIX, progressEmitter, type ResearchProgressEvent } from "../../src/research/progress-events.js";

const WS = "/work/repo";

describe("research progress messages", () => {
  it("maps each loop event to one message", () => {
    const at = { turn: 2, maxTurns: 8 };
    expect(researchProgressMessage({ event: "model", ...at }, WS)).toBe("turn 2/8: model thinking");
    expect(researchProgressMessage({ event: "action", ...at, action: { action: "shell", command: "grep -rn foo src/" } }, WS))
      .toBe("turn 2/8: grep 'foo' in src/");
    expect(researchProgressMessage({ event: "action", ...at, action: { action: "shell", command: "ls" }, repeat: true }, WS))
      .toBe("turn 2/8: list . (repeat; not re-run)");
    expect(researchProgressMessage({ event: "action", ...at, action: { action: "page", step: 1, page: 3 } }, WS))
      .toBe("turn 2/8: page 3 of step 1");
    expect(researchProgressMessage({ event: "repair", ...at }, WS)).toBe("turn 2/8: reply was not a valid action; asking again");
    expect(researchProgressMessage({ event: "retry", attempt: 2, attempts: 3 }, WS)).toBe("model server busy; retrying (attempt 2/3)");
    expect(researchProgressMessage({ event: "finish", ...at }, WS)).toBe("finishing answer");
    expect(researchProgressMessage({ event: "finalize", maxTurns: 8 }, WS)).toBe("turn budget used (8); finishing answer");
  });

  it("ignores unknown or malformed events", () => {
    expect(researchProgressMessage({ event: "future_event", turn: 1, maxTurns: 2 }, WS)).toBeUndefined();
    expect(researchProgressMessage({ event: "model" }, WS)).toBeUndefined();
    expect(researchProgressMessage({ event: "action", turn: 1, maxTurns: 2 }, WS)).toBeUndefined();
    expect(researchProgressMessage("model", WS)).toBeUndefined();
    expect(researchProgressMessage(null, WS)).toBeUndefined();
  });

  it("describes octocode calls with workspace-relative paths", () => {
    expect(describeOctocodeCall("localGetFileContent", { path: `${WS}/src/a.ts`, startLine: 1, endLine: 120 }, WS))
      .toBe("read src/a.ts (lines 1-120)");
    expect(describeOctocodeCall("localGetFileContent", { path: `${WS}/src/a.ts`, matchString: "export" }, WS))
      .toBe("read src/a.ts (around 'export')");
    expect(describeOctocodeCall("localGetFileContent", { path: "src/a.ts", minify: "symbols" }, WS)).toBe("read src/a.ts (outline)");
    expect(describeOctocodeCall("localSearchCode", { path: `${WS}/src`, keywords: "foo" }, WS)).toBe("grep 'foo' in src");
    expect(describeOctocodeCall("localViewStructure", { path: WS, maxDepth: 2 }, WS)).toBe("list . (depth 2)");
    expect(describeOctocodeCall("localFindFiles", { path: WS, names: ["*.ts", "*.md"] }, WS)).toBe("find *.ts, *.md in .");
    expect(describeOctocodeCall("lspGetSemantics", { uri: `${WS}/src/a.ts`, type: "references", symbolName: "run" }, WS))
      .toBe("lsp references 'run' in src/a.ts");
    // A path outside the workspace is shown as given, not as a ../ walk.
    expect(describeOctocodeCall("localSearchCode", { path: "/elsewhere", keywords: "x" }, WS)).toBe("grep 'x' in /elsewhere");
    expect(describeOctocodeCall("otherTool", { a: 1 }, WS)).toBe('otherTool {"a":1}');
  });

  it("describes simple shell reads and searches, and shows anything else whole", () => {
    expect(describeShellCommand("grep -rn \"class Group\" click/src")).toBe("grep 'class Group' in click/src");
    expect(describeShellCommand("grep -rn -A 3 foo . --exclude-dir={node_modules,dist}")).toBe("grep 'foo' in .");
    expect(describeShellCommand("grep -C2 -e needle src lib")).toBe("grep 'needle' in src lib");
    expect(describeShellCommand("rg -g '*.ts' --type ts handler")).toBe("rg 'handler' in .");
    expect(describeShellCommand("git grep -n runAdapter -- src")).toBe("git grep 'runAdapter' in src");
    expect(describeShellCommand("sed -n '1,120p' src/a.ts")).toBe("read src/a.ts (lines 1-120)");
    expect(describeShellCommand("head -n 40 src/a.ts")).toBe("read src/a.ts (lines 1-40)");
    expect(describeShellCommand("head -20 src/a.ts")).toBe("read src/a.ts (lines 1-20)");
    expect(describeShellCommand("tail src/a.ts")).toBe("read src/a.ts (last 10 lines)");
    expect(describeShellCommand("cat a.ts b.ts")).toBe("read a.ts, b.ts");
    expect(describeShellCommand("ls -la src")).toBe("list src");
    const pipeline = "grep -rn foo src | head -n 5";
    expect(describeShellCommand(pipeline)).toBe(`$ ${pipeline}`);
    expect(describeShellCommand("grep 'unbalanced src")).toBe("$ grep 'unbalanced src");
    expect(describeShellCommand("wc -l src/a.ts")).toBe("$ wc -l src/a.ts");
    expect(describeShellCommand("grep -rn")).toBe("$ grep -rn");
  });
});

describe("PrefixedLineFilter", () => {
  it("diverts prefixed lines split across chunks and keeps every other byte", () => {
    const lines: string[] = [];
    const filter = new PrefixedLineFilter("@p ", (payload) => lines.push(payload));
    const kept = [
      filter.push(Buffer.from("warn: one\n@p {\"a\"")),
      filter.push(Buffer.from(":1}\npartial diag")),
      filter.push(Buffer.from("nostic\n@p tail")),
    ];
    expect(filter.pending).toBe("@p tail".length);
    kept.push(filter.end());
    expect(Buffer.concat(kept).toString()).toBe("warn: one\npartial diagnostic\n");
    expect(lines).toEqual(['{"a":1}', "tail"]);
    expect(filter.pending).toBe(0);
  });

  it("returns an unterminated diagnostic tail at end of stream", () => {
    const filter = new PrefixedLineFilter("@p ", () => undefined);
    expect(filter.push(Buffer.from("Error: boom")).length).toBe(0);
    expect(filter.end().toString()).toBe("Error: boom");
  });

  it("does not let a malformed progress line fail or leak into diagnostics", () => {
    const messages: string[] = [];
    const filter = researchProgressFilter((message) => messages.push(message), WS);
    const kept = filter.push(Buffer.from(`${PROGRESS_PREFIX}{not json\n${PROGRESS_PREFIX}{"event":"finish"}\nreal\n`));
    expect(kept.toString()).toBe("real\n");
    expect(messages).toEqual(["finishing answer"]);
  });
});

describe("adapter progress emitter", () => {
  it("emits only when the environment asks for stderr progress", () => {
    expect(progressEmitter({})).toBeUndefined();
    expect(progressEmitter({ [PROGRESS_ENV]: "1" })).toBeUndefined();
    const written: string[] = [];
    progressEmitter({ [PROGRESS_ENV]: "stderr" }, (line) => written.push(line))!({ event: "model", turn: 1, maxTurns: 4 });
    expect(written).toEqual([`${PROGRESS_PREFIX}{"event":"model","turn":1,"maxTurns":4}\n`]);
  });

  it("never throws when the write fails", () => {
    const emit = progressEmitter({ [PROGRESS_ENV]: "stderr" }, () => {
      throw new Error("EPIPE");
    })!;
    expect(() => emit({ event: "finish", turn: 1, maxTurns: 1 })).not.toThrow();
  });
});

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

describe("research loop progress", () => {
  it("reports each turn, tool step, repeat, page and finish in order", async () => {
    const root = mkdtempSync(path.join(tmpdir(), "freellama-progress-test-"));
    roots.push(root);
    writeFileSync(path.join(root, "prompt.txt"), "Where is foo?", "utf8");
    const replies = [
      { action: "shell", command: "grep -rn foo src" },
      "not json",
      { action: "shell", command: "grep -rn foo src" },
      { action: "page", step: 1, page: 1 },
      { action: "finish", answer: "src/a.ts" },
    ];
    const requestJson = async () => {
      const next = replies.shift()!;
      return { message: { content: typeof next === "string" ? next : JSON.stringify(next) } };
    };
    const events: ResearchProgressEvent[] = [];
    const env = {
      FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: root, FREELLAMA_BENCH_PROMPT: path.join(root, "prompt.txt"),
      FREELLAMA_AGENT_RESULT: path.join(root, "result.json"), FREELLAMA_AGENT_MAX_TURNS: "8",
    };
    const code = await runAdapter(BASH_ADAPTER, {
      env, requestJson, print: () => undefined, runTool: () => "src/a.ts:1:foo", progress: (event) => events.push(event),
    });
    expect(code).toBe(0);
    const messages = events.map((event) => researchProgressMessage(event, root));
    expect(messages).toEqual([
      "turn 1/8: model thinking",
      "turn 1/8: grep 'foo' in src",
      "turn 2/8: model thinking",
      "turn 2/8: reply was not a valid action; asking again",
      "turn 3/8: model thinking",
      "turn 3/8: grep 'foo' in src (repeat; not re-run)",
      "turn 4/8: model thinking",
      "turn 4/8: page 1 of step 1",
      "turn 5/8: model thinking",
      "finishing answer",
    ]);
  });
});

describe.skipIf(process.platform === "win32")("runDelegate stderr filter", () => {
  const base = { env: process.env, timeoutMs: 10_000, maxBuffer: 1024 * 1024 };
  const script = [
    `printf '%s\\n' 'diag one' '${PROGRESS_PREFIX}{"event":"model","turn":1,"maxTurns":2}'`,
    `printf '%s\\n' '${PROGRESS_PREFIX}{"event":"finish","turn":1,"maxTurns":2}' 'diag two' >&2`,
  ].join(" >&2; ");

  it("reports progress lines and keeps them out of captured stderr", async () => {
    const messages: string[] = [];
    const run = runDelegate("sh", ["-c", `echo out; ${script}`], {
      ...base, stderrFilter: researchProgressFilter((message) => messages.push(message), WS),
    });
    await expect(run.done).resolves.toEqual({ stdout: "out\n", stderr: "diag one\ndiag two\n" });
    expect(messages).toEqual(["turn 1/2: model thinking", "finishing answer"]);
  });

  it("quotes only diagnostics in a failure message", async () => {
    const run = runDelegate("sh", ["-c", `${script}; exit 3`], {
      ...base, stderrFilter: researchProgressFilter(() => undefined, WS),
    });
    const error = (await run.done.catch((caught: unknown) => caught)) as Error & { stderr: string };
    // The first line names the command (which here spells the lines out); stderr follows it.
    expect(error.message.split("\n").slice(1).join("\n")).toBe("diag one\ndiag two\n");
    expect(error).toMatchObject({ code: 3, stderr: "diag one\ndiag two\n" });
  });
});
