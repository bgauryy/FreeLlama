import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { evidencePaths, groundingRepairNotice, missingPaths, pathsIn, toolErrorHint } from "../../src/research/grounding.js";

const root = mkdtempSync(path.join(tmpdir(), "freellama-grounding-"));
mkdirSync(path.join(root, "sweep"), { recursive: true });
writeFileSync(path.join(root, "sweep", "sources.ts"), "x");

describe("research answer grounding", () => {
  it("finds relative file paths in prose, grep lines and code spans", () => {
    expect(pathsIn("see `sweep/sources.ts` and ./a/b.rs:12, not https or 1.5")).toEqual(["sweep/sources.ts", "a/b.rs"]);
  });

  it("flags a cited path that is not in the workspace and lists the real ones from the evidence", () => {
    const answer = "The parser is in `sweep/tools/parseRobotsTxt.ts`.";
    expect(missingPaths(answer, root)).toEqual(["sweep/tools/parseRobotsTxt.ts"]);
    expect(missingPaths("It is sweep/sources.ts:95.", root)).toEqual([]);
    const evidence = evidencePaths(["./sweep/sources.ts:95:/** Disallowed path prefixes */", "./gone/x.ts:1: y"], root);
    expect(evidence).toEqual(["sweep/sources.ts"]);
    expect(groundingRepairNotice(["sweep/tools/parseRobotsTxt.ts"], evidence)).toMatch(/do not exist.*Paths your own commands returned: sweep\/sources\.ts/);
  });

  it("explains rg's flags when a model passes grep's", () => {
    expect(toolErrorHint("rg: unrecognized flag --exclude-dir")).toMatch(/--glob '!node_modules'/);
    expect(toolErrorHint("other")).toBe("");
  });
});

describe("research loop grounding and repeats", async () => {
  const { runAdapter } = await import("../../src/research/agent-loop.js");
  const { BASH_ADAPTER } = await import("../../src/research/bash-agent.js");
  const { readFileSync } = await import("node:fs");

  async function run(replies: unknown[], maxTurns = 8, spec: Parameters<typeof runAdapter>[0] = BASH_ADAPTER, toolOutput = "./sweep/sources.ts:95:robots") {
    const workspace = mkdtempSync(path.join(tmpdir(), "freellama-loop-"));
    mkdirSync(path.join(workspace, "sweep"), { recursive: true });
    writeFileSync(path.join(workspace, "sweep", "sources.ts"), "robots");
    writeFileSync(path.join(workspace, "prompt.txt"), "Which file parses robots.txt?");
    const sent: string[] = [];
    const requestJson = async (_url: string, payload: unknown) => {
      const messages = (payload as { messages: { content: string }[] }).messages;
      sent.push(messages.at(-1)!.content);
      return { message: { content: JSON.stringify(replies.shift() ?? { action: "finish", answer: "out of replies" }) } };
    };
    const env = {
      FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: workspace, FREELLAMA_BENCH_PROMPT: path.join(workspace, "prompt.txt"),
      FREELLAMA_AGENT_RESULT: path.join(workspace, "result.json"), FREELLAMA_AGENT_MAX_TURNS: String(maxTurns),
    };
    await runAdapter(spec, { env, requestJson, print: () => undefined, runTool: () => toolOutput });
    return { result: JSON.parse(readFileSync(path.join(workspace, "result.json"), "utf8")), sent };
  }

  it("gives one repair turn for a cited path that does not exist, and keeps the corrected answer", async () => {
    const { result, sent } = await run([
      { action: "shell", command: "grep -rn robots ." },
      { action: "finish", answer: "It is sweep/tools/parseRobotsTxt.ts" },
      { action: "finish", answer: "It is sweep/sources.ts" },
    ]);
    expect(sent.some((message) => /do not exist.*sweep\/sources\.ts/s.test(message))).toBe(true);
    expect(result.final_answer).toBe("It is sweep/sources.ts");
  });

  it("marks an answer that still cites a missing path as unverified", async () => {
    const { result } = await run([
      { action: "finish", answer: "It is sweep/tools/a.ts" },
      { action: "finish", answer: "It is sweep/tools/a.ts" },
    ]);
    expect(result.final_answer).toMatch(/not found in the workspace: sweep\/tools\/a\.ts; treat this answer as unverified/);
  });

  it("asks once for the source when an answer names no file but the results did", async () => {
    const { result, sent } = await run([
      { action: "shell", command: "grep -rn robots ." },
      { action: "finish", answer: "The parser." },
      { action: "finish", answer: "The parser. Source: sweep/sources.ts:95" },
    ]);
    expect(sent.some((message) => /names no source file.*sweep\/sources\.ts/s.test(message))).toBe(true);
    expect(result.final_answer).toBe("The parser. Source: sweep/sources.ts:95");
  });

  describe("adapter with observed grounding (GitHub)", () => {
    const remote = { ...BASH_ADAPTER, grounding: "observed" as const };
    const seen = "path: packages/client/src/client/stdio.ts\nmatches: StdioClientTransport";
    const search = { action: "shell", command: "gh search" };

    it("accepts a remote path its own tool output showed: no repair turn, no note", async () => {
      const { result, sent } = await run([search, { action: "finish", answer: "Defined in `packages/client/src/client/stdio.ts`." }], 8, remote, seen);
      expect(sent.some((message) => /did not appear|do not exist/.test(message))).toBe(false);
      expect(result.final_answer).toBe("Defined in `packages/client/src/client/stdio.ts`.");
      expect(result.final_answer).not.toMatch(/freellama:/);
    });

    it("still flags a path no tool output showed, with a repair turn listing the seen paths", async () => {
      const { result, sent } = await run([
        search,
        { action: "finish", answer: "It is packages/core/src/made/up.ts" },
        { action: "finish", answer: "It is packages/core/src/made/up.ts" },
      ], 8, remote, seen);
      expect(sent.some((message) => /did not appear in any tool result.*packages\/client\/src\/client\/stdio\.ts/s.test(message))).toBe(true);
      expect(result.final_answer).toMatch(/not seen in any tool result: packages\/core\/src\/made\/up\.ts; treat this answer as unverified/);
    });

    it("keeps the workspace check for the bash adapter even when the output names the path", async () => {
      const { result } = await run([search, { action: "finish", answer: "It is packages/client/src/client/stdio.ts" }, { action: "finish", answer: "It is packages/client/src/client/stdio.ts" }], 8, BASH_ADAPTER, seen);
      expect(result.final_answer).toMatch(/not found in the workspace: packages\/client\/src\/client\/stdio\.ts/);
    });
  });

  it("stops running tools after the second repeated command and asks for the answer", async () => {
    const shell = { action: "shell", command: "grep -rn robots ." };
    const { result, sent } = await run([shell, shell, shell, { action: "finish", answer: "sweep/sources.ts" }, shell, shell]);
    expect(result.final_answer).toBe("sweep/sources.ts");
    // Three shell turns (the second and third are repeats), then the forced finish: no fourth tool turn.
    expect(sent).toHaveLength(4);
    expect(sent[2]).toMatch(/identical call already made/);
    expect(sent[3]).toMatch(/Return exactly a finish JSON action now/);
  });
});

describe("search-hit citations", async () => {
  const { searchHitCitations } = await import("../../src/research/grounding.js");
  it("cites the first grep hit in each file the answer names", () => {
    const steps = [
      { step: 1, status: "error", result: "./sweep/sources.ts:1:x" },
      { step: 2, status: "ok", result: "./tools/sweep.ts:99: robots\n./sweep/sources.ts:95:/** robots */\n./sweep/sources.ts:195: get" },
    ];
    expect(searchHitCitations(steps, "It is ./sweep/sources.ts", root)).toEqual([
      { step: 2, tool: "shell", path: "sweep/sources.ts", line: 95, kind: "search_hit" },
    ]);
    expect(searchHitCitations(steps, "no path here", root)).toEqual([]);
  });
});

describe("research time budget", async () => {
  const { researchTimeoutSeconds } = await import("../../src/config.js");
  it("gives bigger models more time, caps it, and lets the caller choose", () => {
    expect(researchTimeoutSeconds(3.2e9, undefined)).toBe(180);
    expect(researchTimeoutSeconds(18.2e9, undefined)).toBe(464);
    expect(researchTimeoutSeconds(80e9, undefined)).toBe(1200);
    expect(researchTimeoutSeconds(undefined, undefined)).toBe(180);
    expect(researchTimeoutSeconds(3.2e9, 600)).toBe(600);
    expect(researchTimeoutSeconds(3.2e9, 5000)).toBe(1800);
  });
});
