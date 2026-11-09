import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { assessDelegatedAnswer, parseAdapterResult } from "../../src/delegate.js";
import { runAdapter } from "../../src/research/agent-loop.js";
import { BASH_ADAPTER, runShell } from "../../src/research/bash-agent.js";
import { ToolExitError } from "../../src/research/errors.js";
import { runGithub, runOctocode } from "../../src/research/octocode-agent.js";
import * as proc from "../../src/research/proc.js";
import { sourceBearingLookup } from "../../src/research/source-evidence.js";

let root: string;
beforeEach(() => {
  root = realpathSync(mkdtempSync(path.join(tmpdir(), "freellama-evidence-")));
  writeFileSync(path.join(root, "source.txt"), "source fact\n");
  vi.stubEnv("FREELLAMA_AGENT_OS_SANDBOX", "off");
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllEnvs();
  rmSync(root, { recursive: true, force: true });
});

describe("research subprocess outcomes", () => {
  it("preserves failed-read diagnostics even when a later pipeline stage exits zero", async () => {
    for (const command of ["cat missing.txt", "cat missing.txt | head -n 1"]) {
      await expect(runShell(root, command, 5)).rejects.toMatchObject({
        status: "error", exitCode: 1, output: expect.stringContaining("missing.txt"),
      });
    }
  });

  it("distinguishes rg/grep no matches from bad syntax and missing files", async () => {
    for (const command of ["rg absent source.txt", "grep absent source.txt"]) {
      await expect(runShell(root, command, 5)).rejects.toMatchObject({ status: "empty", exitCode: 1 });
    }
    await expect(runShell(root, "rg '[' source.txt", 5)).rejects.toMatchObject({ status: "error", exitCode: 2 });
    await expect(runShell(root, "rg fact missing.txt", 5)).rejects.toMatchObject({ status: "error", exitCode: 2 });
    expect(await runShell(root, "rg fact source.txt", 5)).toContain("source fact");
  });

  it("never reads exit one as no-match unless the command is one plain search", async () => {
    // pipefail makes the pipeline exit 1, but an earlier stage may be what failed.
    for (const command of ["rg absent source.txt | cat", "cat missing.txt | rg fact", "echo x | grep absent"]) {
      await expect(runShell(root, command, 5)).rejects.toMatchObject({ status: "error" });
    }
    // Other commands exit 1 as errors, with the diagnostic kept.
    await expect(runShell(root, "cat missing.txt", 5)).rejects.toMatchObject({ status: "error", exitCode: 1 });
  }, 20_000); // spawns sandboxed processes; slow on a loaded machine

  it("keeps diff exit one as successful comparison output", async () => {
    writeFileSync(path.join(root, "other.txt"), "different fact\n");
    expect(await runShell(root, "diff source.txt other.txt", 5)).toContain("different fact");
  });

  it.each(["cat missing.txt; cat source.txt", "cat missing.txt || cat source.txt", "cat missing.txt && true; cat source.txt"])(
    "refuses a command list that could mask a failed read: %s", async (command) => {
      await expect(runShell(root, command, 5)).rejects.toThrow("one pipeline per turn");
    },
  );

  it.each([2, 3, -9])("never treats an octocode partial batch with exit %i as successful evidence", async (returncode) => {
    vi.spyOn(proc, "runProcess").mockResolvedValue({ stdout: "row 1: source fact\n", stderr: "row 2: failed query\n", returncode });
    const queries = [{ path: ".", matchString: ["fact"] }, { path: "missing" }];
    await expect(runOctocode(root, "localSearch", queries, 5)).rejects.toMatchObject({
      status: "error", exitCode: returncode, output: "row 1: source fact\nrow 2: failed query",
    });
    await expect(runGithub(root, "ghSearchCode", [{ repo: "one" }, { repo: "two" }], 5))
      .rejects.toBeInstanceOf(ToolExitError);
  });

  it("records octocode exit one as empty, preserving diagnostic notes", async () => {
    vi.spyOn(proc, "runProcess").mockResolvedValue({ stdout: "# status: empty\n# widen scope\n", stderr: "", returncode: 1 });
    await expect(runOctocode(root, "localSearch", [{ path: "." }], 5)).rejects.toMatchObject({
      status: "empty", exitCode: 1, output: "# status: empty\n# widen scope",
    });
  });
});

describe("research evidence accounting", () => {
  it("requires a source-bearing first stage and retains diagnostics without grounding them", async () => {
    const call = (command: string, result: string) => ({ raw_name: "shell", status: "ok", arguments: { command }, result });
    for (const command of ["echo invented", "printf invented", "true", "echo invented | rg invented"]) {
      const output = await runShell(root, command, 5);
      expect(sourceBearingLookup(call(command, output), root), command).toBe(false);
    }
    // A synthetic final stage may succeed or cause SIGPIPE in its producer; neither is evidence.
    expect(sourceBearingLookup(call("cat source.txt | printf invented", "invented"), root)).toBe(false);
    expect(sourceBearingLookup(call("cat source.txt | jq -n '\"invented\"'", "invented"), root)).toBe(false);
    for (const command of ["cat source.txt", "rg -n fact source.txt", "cat source.txt | head -n 1", "sed -n '1,2p' source.txt"]) {
      expect(sourceBearingLookup(call(command, await runShell(root, command, 5)), root), command).toBe(true);
    }
    expect(sourceBearingLookup(call("rg -e source.txt", "not a source operand"), root)).toBe(false);
    expect(sourceBearingLookup({ status: "ok", arguments: { tool: "localFetch", queries: [{ path: "source.txt" }] }, result: "source.txt:1:source fact" }, root)).toBe(true);
    expect(sourceBearingLookup({ status: "ok", arguments: { tool: "lspSearch", queries: [{ path: "source.txt" }] }, result: "# matchCount: 0" }, root)).toBe(false);
  });
  it("persists distinct failure/empty outcomes and full observations without grounding the answer", async () => {
    const resultPath = path.join(root, "result.json");
    const replies = [
      { action: "shell", command: "cat missing.txt" },
      { action: "shell", command: "rg absent source.txt" },
      { action: "finish", answer: "I could not establish the requested fact." },
    ];
    const messages: string[] = [];
    const code = await runAdapter(BASH_ADAPTER, {
      env: {
        FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: root,
        FREELLAMA_BENCH_PROMPT: path.join(root, "source.txt"), FREELLAMA_AGENT_RESULT: resultPath,
        FREELLAMA_AGENT_MAX_TURNS: "4",
      },
      requestJson: (_url, payload) => {
        messages.push(JSON.stringify(payload));
        return { message: { content: JSON.stringify(replies.shift()) } };
      },
      print: () => undefined,
    });
    expect(code).toBe(0);
    const result = parseAdapterResult(readFileSync(resultPath, "utf8"));
    expect(result.tool_calls).toMatchObject([{ status: "error", exit_code: 1 }, { status: "empty", exit_code: 1 }]);
    expect(result.tool_calls[0]!.result).toContain("missing.txt");
    expect(result.tool_calls[1]!.result).toContain("nothing found");
    expect(messages[2]).toContain('\\"status\\":\\"empty\\"');
    const succeeded = result.tool_calls.filter((call) => call.status === "ok");
    expect(assessDelegatedAnswer("Where is the fact?", succeeded.length, "qwen3.5:27b").grounded).toBe(false);
  });

  it("does not invent success for custom adapters that omit status", () => {
    const result = parseAdapterResult(JSON.stringify({ final_answer: "A claim", tool_calls: [{ raw_name: "shell" }] }));
    expect(result.tool_calls[0]!.status).toBe("unknown");
  });
});
