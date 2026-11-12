import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

// The CLI and gateway run the real run_task handler; only serve and the live inventory are fakes.
const fixture = vi.hoisted(() => ({
  run: vi.fn(),
  live: vi.fn(),
  route: vi.fn(),
  warm: vi.fn(),
}));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: fixture.run, getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: (...args: unknown[]) => fixture.route(...args),
  createSession: vi.fn(), deleteSession: vi.fn(), killSession: vi.fn(), listModels: vi.fn(), warmModelRequest: (...args: unknown[]) => fixture.warm(...args),
  listTaskJobs: vi.fn(), createScope: vi.fn(), getScope: vi.fn(), forkScope: vi.fn(), deleteScope: vi.fn(),
}));
vi.mock("../../src/model-library.js", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  catalogLiveState: (...args: unknown[]) => fixture.live(...args),
}));

import { parseRunArgs, UsageError } from "../../src/agent-cli/args.js";
import { renderResult, refusalOf, splitAnswer } from "../../src/agent-cli/output.js";
import { TOOL_CALL_WITHOUT_TOOLS } from "../../src/result-checks.js";
import { callTool } from "../../src/agent-cli/registry.js";
import { hostAllowed, originAllowed, startGateway } from "../../src/agent-cli/gateway.js";
import { chatTaskArgs, fromOpenAiRequest, openAiCompletion } from "../../src/agent-cli/managed.js";
import { virtualModel, virtualModels } from "../../src/agent-cli/virtual.js";
import { TOOL_REGISTRATIONS } from "../../src/tools/registrations.js";
import { toolRegistry } from "../../src/agent-cli/registry.js";
import { streamRun, streamTarget } from "../../src/agent-cli/stream.js";
import { chatModelName, main } from "../../src/agent-cli/main.js";

const render = { json: false, verbose: false, quiet: false, hideThinking: false };
const answer = (content: string, extra: Record<string, unknown> = {}) => JSON.stringify({
  route: { selected_model: "bonsai-2:9b" },
  response: { message: { role: "assistant", content } },
  metrics: { prompt_tokens: 120, cached_prompt_tokens: 100, output_tokens: 8, total_duration_ns: 2e9 },
  ...extra,
});

let savedAutostart: string | undefined;
beforeAll(() => {
  savedAutostart = process.env.FREELLAMA_MCP_AUTOSTART_SERVE;
  process.env.FREELLAMA_MCP_AUTOSTART_SERVE = "0";
});
afterAll(() => {
  if (savedAutostart === undefined) delete process.env.FREELLAMA_MCP_AUTOSTART_SERVE;
  else process.env.FREELLAMA_MCP_AUTOSTART_SERVE = savedAutostart;
});
beforeEach(() => {
  fixture.run.mockReset();
  fixture.run.mockImplementation(async () => answer("Fixed."));
  fixture.live.mockReset();
  fixture.live.mockResolvedValue([[{ name: "bonsai-2:9b", capabilities: ["completion"] }], 64e9]);
  fixture.route.mockReset();
  fixture.route.mockResolvedValue(JSON.stringify({ selected_model: "bonsai-2:9b", options: { num_ctx: 4096 } }));
  fixture.warm.mockReset();
  fixture.warm.mockResolvedValue(JSON.stringify({ status: "warmed", model: "bonsai-2:9b" }));
});

describe("freellama run flags", () => {
  it("produces run_task arguments the real tool schema accepts, for every documented flag", () => {
    const schema = toolRegistry().get("run_task")!.inputSchema;
    const cases: string[][] = [
      ["-m", "m:1", "hi", "--prefer", "cpu"],
      ["-m", "m:1", "hi", "--prefer", "gpu"],
      ["-w", "fix-bug", "-f", "a.ts", "bug", "--objective", "quality", "--min-confidence", "medium"],
      ["-j", "coding", "-f", "a.ts", "x", "--priority", "background", "--timeout", "30", "--max-wait", "10", "--defer"],
      ["-m", "m:1", "hi", "--think=false", "--keepalive", "5m", "-o", "temperature=0", "-o", "num_ctx=2048", "--schema", "name, age int"],
      ["-m", "m:1", "hi", "-s", "be brief", "--format", "json", "--save-as", "note", "--session", "00000000-0000-4000-8000-000000000000"],
      ["-m", "m:1", "hi", "--tools", '[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{}}}}]'],
      ["-j", "find-code", "-f", "src/a.ts", "--top", "3", "query", "--dimensions", "64"],
      ["--ask", "Is it urgent?", "--choice", "yes=urgent", "--choice", "no", "state"],
      ["-w", "fix-bug", "--preview"],
    ];
    for (const argv of cases) {
      const parsed = schema.safeParse(parseRunArgs(argv).args);
      expect(parsed.success, `${argv.join(" ")}: ${parsed.success ? "" : JSON.stringify(parsed.error.issues)}`).toBe(true);
    }
    expect(parseRunArgs(["-m", "m:1", "hi", "--prefer", "cpu"]).args.executionPreference).toBe("prefer_cpu");
  });

  it("attaches an image path written anywhere in the prompt, as ollama run does", () => {
    const dir = mkdtempSync(join(tmpdir(), "fl-img-"));
    const image = join(dir, "receipt.png");
    writeFileSync(image, "png");
    expect(parseRunArgs(["-m", "vl:1", `What is the total in ${image}?`]).args.files).toEqual([image]);
    expect(parseRunArgs(["-m", "vl:1", `Read "${image}".`]).args.files).toEqual([image]);
    expect(parseRunArgs(["-m", "vl:1", "describe", image, "--image", image]).args.files).toEqual([image]);
    expect(parseRunArgs(["-m", "vl:1", `What is in ${join(dir, "missing.png")}?`]).args.files).toBeUndefined();
  });

  it("reads the first word as a model only when it could be a tag and nothing else names one", () => {
    expect(parseRunArgs(["gemma4", "Why", "blue?"]).args).toMatchObject({ model: "gemma4", prompt: "Why blue?" });
    expect(parseRunArgs(["Why is the sky blue?"]).args).toEqual({ prompt: "Why is the sky blue?" });
    expect(parseRunArgs(["-w", "fix-bug", "parser", "drops", "a", "token"]).args).toEqual({ workflow: "fix-bug", prompt: "parser drops a token" });
  });

  it("keeps ollama's flag meanings: bare --think is true, --keepalive, --format json or a schema", () => {
    const { args } = parseRunArgs(["m:1", "hi", "--think", "--keepalive", "5m", "--format", '{"type":"object","properties":{}}']);
    expect(args).toMatchObject({ think: true, keepAlive: "5m", format: { type: "object", properties: {} } });
    expect(parseRunArgs(["m:1", "hi", "--think=low"]).args.think).toBe("low");
    // Level names are the model's (qwen3.8 lists xhigh); serve validates them against /api/show.
    expect(parseRunArgs(["m:1", "hi", "--think=xhigh"]).args.think).toBe("xhigh");
    expect(() => parseRunArgs(["m:1", "--think=Not A Level"])).toThrow(UsageError);
  });

  it("turns --ask/--choice/--level into decision questions over stdin", () => {
    const { args } = parseRunArgs(
      ["nimble", "--ask", "Refund?", "--ask", "Team?", "--choice", "billing=Payments", "--choice", "other", "--ask", "Urgency?", "--level", "Routine", "--level", "Now"],
      "ticket text",
    );
    // `ollama run nimble --ask …`: the first word is still the model.
    expect(args).toMatchObject({
      model: "nimble", task: "decision", state: "ticket text",
      questions: {
        refund: { type: "noul", instructions: "Refund?" },
        team: { type: "choice", instructions: "Team?", criteria: { billing: "Payments", other: null } },
        urgency: { type: "score", instructions: "Urgency?", criteria: ["Routine", "Now"] },
      },
    });
    expect(() => parseRunArgs(["--choice", "a=b"])).toThrow(/must follow the --ask/);
  });

  it("runs a question with no model on the classify job's tested pick, not on routing's first rank", () => {
    const { args } = parseRunArgs(["--ask", "Refund?"], "charged twice");
    expect(args).toEqual({ job: "classify", state: "charged twice", questions: { refund: { type: "noul", instructions: "Refund?" } } });
    // Names come from the question's words and stay unique.
    expect(Object.keys(parseRunArgs(["--ask", "Is a refund requested?", "--ask", "Is a refund requested?"], "x").args.questions as object)).toEqual(["refund-requested", "refund-requested-2"]);
    expect(parseRunArgs(["--ask", "Refund?", "-t", "decision"], "x").args.job).toBeUndefined();
  });

  it("turns --schema's short form or JSON into a format schema", () => {
    expect(parseRunArgs(["-w", "extract-json", "--schema", "name, age int, tags list: up to five"], "text").args.format).toEqual({
      type: "object",
      properties: { name: { type: "string" }, age: { type: "integer" }, tags: { type: "array", items: { type: "string" }, description: "up to five" } },
      required: ["name", "age", "tags"],
    });
    expect(parseRunArgs(["m:1", "x", "--schema", '{"type":"object","properties":{"a":{"type":"string"}}}']).args.format).toMatchObject({ properties: { a: {} } });
    expect(() => parseRunArgs(["m:1", "x", "--schema", "age years"])).toThrow(/--schema field "age years"/);
    expect(() => parseRunArgs(["m:1", "x", "--schema", "a", "--format", "json"])).toThrow(/remove --format/);
  });

  it("maps stdin and the prompt by task: embedding lines with a nearest query, chat text after the prompt", () => {
    expect(parseRunArgs(["-j", "find-code", "--top", "3", "where are flags parsed"], "a\n\nb\n").args)
      .toMatchObject({ job: "find-code", input: ["a", "b"], nearest: { query: "where are flags parsed", top: 3 } });
    expect(parseRunArgs(["-w", "summarize-log"], "line1\nline2").args.prompt).toBe("line1\nline2");
    expect(parseRunArgs(["-w", "summarize-log", "focus on 500s"], "log").args.prompt).toBe("focus on 500s\n\nlog");
  });

  it("types options and routes num_ctx to contextTokens", () => {
    const { args } = parseRunArgs(["m:1", "hi", "-o", "temperature=0.2", "-o", "num_ctx=8192", "-o", "stop=END", "-o", "mirostat=false"]);
    expect(args).toMatchObject({ contextTokens: 8192, options: { temperature: 0.2, stop: "END", mirostat: false } });
  });

  it("refuses flags whose effect a managed task cannot honor, naming what can", () => {
    // --dimensions now shortens managed embedding vectors; a chat task still refuses it.
    expect(parseRunArgs(["-j", "find-code", "--dimensions", "256", "q"]).args.dimensions).toBe(256);
    expect(() => parseRunArgs(["-w", "fix-bug", "--dimensions", "256", "x"])).toThrow(/embedding/);
    expect(() => parseRunArgs(["m:1", "--voice", "x"])).toThrow(/freellama tool audio/);
    expect(() => parseRunArgs(["m:1", "--truncate=false"])).toThrow(UsageError);
    expect(() => parseRunArgs(["-w", "no-such"])).toThrow(/freellama workflows/);
    expect(() => parseRunArgs(["--bogus"])).toThrow(UsageError);
  });

  it("marks a run with no input as promptless (a chat on a terminal)", () => {
    expect(parseRunArgs(["gemma4"]).promptless).toBe(true);
    expect(parseRunArgs(["gemma4", "-f", "x.ts"]).promptless).toBe(false);
  });
});

describe("freellama output", () => {
  it("puts only the answer on stdout and check:/handoff lines on stderr", () => {
    const text = "[lfm3 · loaded] line one\nline two\ncheck: SEARCH block 1 does not match the file\nhandoff_file: /tmp/h.md";
    expect(splitAnswer(text)).toEqual({ label: "lfm3 · loaded", answer: "line one\nline two\nhandoff_file: /tmp/h.md", checks: ["check: SEARCH block 1 does not match the file"] });
    const rendered = renderResult({ content: [{ type: "text", text }], structuredContent: {} }, render);
    expect(rendered).toEqual({ stdout: "line one\nline two", stderr: ["handoff_file: /tmp/h.md", "check: SEARCH block 1 does not match the file"], exitCode: 0 });
    expect(renderResult({ content: [{ type: "text", text }], structuredContent: {} }, { ...render, quiet: true }).stderr).toEqual([]);
  });

  it("shows the model, filled fields and cache hits with --verbose", () => {
    const rendered = renderResult({
      content: [{ type: "text", text: "[m] ok" }],
      structuredContent: { job_profile: { job: "coding", workflow: "fix-bug", model: "m", applied: ["model", "systemPrompt"] }, metrics: { prompt_tokens: 120, cached_prompt_tokens: 100, output_tokens: 8 } },
    }, { ...render, verbose: true });
    expect(rendered.stderr).toEqual(["[m]", "job coding/fix-bug: model m; filled model, systemPrompt", "prompt 120 tokens (100 cached), output 8 tokens"]);
  });

  it("prints a JSON refusal's message, keeps it whole for --json, and exits 1", () => {
    const refusal = JSON.stringify({ code: "job_no_model_fits", error: "no catalog pick fits now" });
    // A refusal that waiting can fix exits 75 (EX_TEMPFAIL) and says when to retry.
    expect(renderResult({ isError: true, content: [{ type: "text", text: refusal }] }, render)).toEqual({ stdout: "", stderr: ["no catalog pick fits now (job_no_model_fits)", "freellama: retryable (exit 75)"], exitCode: 75 });
    expect(renderResult({ isError: true, content: [{ type: "text", text: refusal }] }, { ...render, json: true })).toMatchObject({ stdout: refusal, exitCode: 75 });
    const busy = JSON.stringify({ code: "admission_timeout", error: "server busy", retry_after_seconds: 3 });
    expect(renderResult({ isError: true, content: [{ type: "text", text: busy }] }, render).stderr.at(-1)).toBe("freellama: retryable; retry after 3s (exit 75)");
    expect(renderResult({ isError: true, content: [{ type: "text", text: "bad schema" }] }, render).exitCode).toBe(1);
    expect(refusalOf(refusal).unavailable).toBe(true);
    expect(refusalOf("model output was invalid").unavailable).toBe(false);
  });

  it("prints only the first code block with --extract, or the whole answer with a note", () => {
    const text = "[m] Here is the fix:\n```ts\nconst a = 1;\n\nconst b = 2;\n```\nand another\n```\nx\n```";
    expect(renderResult({ content: [{ type: "text", text }], structuredContent: {} }, { ...render, extract: true }).stdout).toBe("const a = 1;\n\nconst b = 2;");
    const plain = renderResult({ content: [{ type: "text", text: "[m] no code" }], structuredContent: {} }, { ...render, extract: true });
    expect(plain).toMatchObject({ stdout: "no code", stderr: ["freellama: --extract found no code block; printing the whole answer"] });
  });

  it("fails an empty, thinking-only answer with the CLI flag that fixes it", () => {
    const text = "[m] (empty answer: the model spent its output (200 tokens) thinking; pass think:false or raise options.num_predict)";
    expect(renderResult({ content: [{ type: "text", text }], structuredContent: {} }, render)).toEqual({
      stdout: "", stderr: ["freellama: empty answer: the model spent its output thinking; rerun with --think=false or raise -o num_predict=N"], exitCode: 1,
    });
  });

  it("fails a reply that is only a tool call nobody asked for, so a pipe never takes it as the answer", () => {
    const text = `[m] tool_calls: [{"function":{"name":"app.log","arguments":{"file":"app.log"}}}]\ncheck: ${TOOL_CALL_WITHOUT_TOOLS}`;
    const rendered = renderResult({ content: [{ type: "text", text }], structuredContent: {} }, render);
    expect(rendered.stdout).toBe("");
    expect(rendered.exitCode).toBe(1);
    expect(rendered.stderr.at(-1)).toMatch(/no answer: the model replied with a tool call although no tools were sent/);
  });

  it("prints a deferred job's id on stdout", () => {
    const rendered = renderResult({ structuredContent: { deferred: true, job: { id: "abc", status: "queued" } } }, render);
    expect(rendered).toEqual({ stdout: "abc", stderr: ["deferred; wait for it with: freellama job abc --wait"], exitCode: 0 });
  });
});

describe("freellama tool registry", () => {
  it("runs the real run_task handler: a workflow gets the job's pick and system prompt", async () => {
    const result = await callTool("run_task", parseRunArgs(["-w", "fix-bug", "parser drops a token"]).args);
    expect(result.isError).toBeFalsy();
    const request = fixture.run.mock.calls[0]![1];
    expect(request.model).toBe("bonsai-2:9b");
    expect(request.messages[0].role).toBe("system");
    expect(request.messages.at(-1).content).toContain("parser drops a token");
    expect(result.structuredContent?.job_profile).toMatchObject({ job: "coding", workflow: "fix-bug", model: "bonsai-2:9b" });
  });

  it("refuses what MCP refuses, with the tool's own schema", async () => {
    await expect(callTool("run_task", { bogus: 1 })).rejects.toThrow(/run_task: .*bogus/);
    await expect(callTool("no_such_tool", {})).rejects.toThrow(/freellama tools/);
  });

  it.each([
    ["job", "--help"], ["job", "-h"],
    ["tool", "--help"], ["tool", "-h"],
  ])("prints %s %s usage without running a task", async (command, flag) => {
    const out = vi.fn();
    const err = vi.fn();
    expect(await main([command, flag], { out, err })).toBe(0);
    expect(out.mock.calls.map(([text]) => text).join("\n")).toContain(`usage: freellama ${command}`);
    expect(err).not.toHaveBeenCalled();
    expect(fixture.run).not.toHaveBeenCalled();
    expect(fixture.live).not.toHaveBeenCalled();
  });

  it.each(["--help", "-h"])("keeps named-tool %s help as its input schema", async (flag) => {
    const out = vi.fn();
    const err = vi.fn();
    expect(await main(["tool", "doctor", flag], { out, err })).toBe(0);
    expect(out.mock.calls.map(([text]) => text).join("\n")).toContain("input schema:");
    expect(err).not.toHaveBeenCalled();
  });

  it("exits 2 on a usage error and 0 on help", async () => {
    const lines: string[] = [];
    const io = { out: (text: string) => lines.push(text), err: (text: string) => lines.push(text) };
    expect(await main(["run", "-w", "fix-bug", "--dimensions", "3", "x"], io)).toBe(2);
    expect(await main(["--help"], io)).toBe(0);
    expect(await main(["nope"], io)).toBe(2);
  });
});

describe("virtual models", () => {
  it("lists chat-shaped workflows and jobs, and accepts the tag the ollama CLI adds", () => {
    const names = virtualModels().map((model) => model.name);
    expect(names).toContain("freellama/fix-bug");
    expect(names).toContain("freellama/auto");
    // Decision and embedding jobs need fields a chat cannot carry.
    expect(names).not.toContain("freellama/classify");
    expect(names).not.toContain("freellama/find-code");
    expect(virtualModel("freellama/review-code:latest")?.workflow).toBe("review-code");
    expect(virtualModel("gemma4:latest")).toBeUndefined();
    expect(virtualModel("freellama/describe-screen")?.vision).toBe(true);
    expect(chatModelName("review-code")).toBe("freellama/review-code");
    expect(chatModelName("gemma4:2b")).toBe("gemma4:2b");
  });

  it("runs freellama/auto on the chat pick that fits now, not on routing's first rank", async () => {
    expect(await chatTaskArgs(virtualModel("freellama/auto")!, { messages: [{ role: "user", content: "hi" }] }))
      .toEqual({ task: "completion", objective: "balanced", model: "bonsai-2:9b", prompt: "hi" });
  });

  it("sends one user turn as a workflow prompt, so job mode applies everything", async () => {
    const args = await chatTaskArgs(virtualModel("freellama/fix-bug")!, { messages: [{ role: "user", content: "it crashes" }], options: { num_ctx: 4096, temperature: 0 } });
    expect(args).toEqual({ workflow: "fix-bug", prompt: "it crashes", contextTokens: 4096, options: { temperature: 0 } });
  });

  it("prepends the workflow's system prompt to a longer conversation only when the caller sent none", async () => {
    const messages = [{ role: "user", content: "a" }, { role: "assistant", content: "b" }, { role: "user", content: "c" }];
    const without = await chatTaskArgs(virtualModel("freellama/explain-code")!, { messages });
    expect(without.model).toBe("bonsai-2:9b");
    expect((without.messages as Array<{ role: string }>)[0]!.role).toBe("system");
    expect((without.messages as unknown[]).length).toBe(4);
    const own = [{ role: "system", content: "mine" }, ...messages];
    expect((await chatTaskArgs(virtualModel("freellama/explain-code")!, { messages: own })).messages).toEqual(own);
  });

  it("maps OpenAI requests and responses", () => {
    const request = fromOpenAiRequest({
      messages: [{ role: "user", content: [{ type: "text", text: "what is this" }, { type: "image_url", image_url: { url: "data:image/png;base64,AAAA" } }] }],
      max_tokens: 64, temperature: 0.1, response_format: { type: "json_object" }, reasoning_effort: "low",
    });
    expect(request).toMatchObject({ messages: [{ role: "user", content: "what is this", images: ["AAAA"] }], options: { num_predict: 64, temperature: 0.1 }, format: "json", think: "low" });
    expect(() => fromOpenAiRequest({ messages: [{ role: "user", content: [{ type: "image_url", image_url: { url: "https://x/y.png" } }] }] })).toThrow(/fetches nothing/);
    const completion = openAiCompletion({
      model: "m", content: "", toolCalls: [{ function: { name: "f", arguments: { a: 1 } } }], checks: ["c"], doneReason: "stop", metrics: { prompt_tokens: 3, output_tokens: 2 }, receipt: {},
    }, "freellama/auto");
    expect(completion).toMatchObject({
      model: "freellama/auto", usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 },
      choices: [{ finish_reason: "tool_calls", message: { tool_calls: [{ type: "function", function: { name: "f", arguments: '{"a":1}' } }] } }],
      freellama: { model: "m", checks: ["c"] },
    });
  });

  it("refuses browser pages from other origins and rebound DNS names", () => {
    expect(originAllowed(undefined)).toBe(true);
    expect(originAllowed("http://localhost:3000")).toBe(true);
    expect(originAllowed("https://evil.example")).toBe(false);
    expect(hostAllowed("127.0.0.1:11437")).toBe(true);
    expect(hostAllowed("[::1]:11437")).toBe(true);
    expect(hostAllowed("evil.example:11437")).toBe(false);
  });

  it("treats OpenAI developer messages as system and reasoning_effort none as thinking off", () => {
    const request = fromOpenAiRequest({ messages: [{ role: "developer", content: "be brief" }, { role: "user", content: "x" }], reasoning_effort: "none", stop: "END" });
    expect(request).toMatchObject({ messages: [{ role: "system", content: "be brief" }, { role: "user" }], think: false, options: { stop: ["END"] } });
  });

  it("leaves an image-only turn to the workflow's own instruction", async () => {
    expect(await chatTaskArgs(virtualModel("freellama/describe-screen")!, { messages: [{ role: "user", content: "", images: ["AAAA"] }] }))
      .toEqual({ workflow: "describe-screen", images: ["AAAA"] });
  });

  it("registers the same tools for MCP and the CLI, from one list", () => {
    const names: string[] = [];
    for (const register of TOOL_REGISTRATIONS) register({ registerTool: (name: string) => names.push(name) } as never);
    expect([...toolRegistry().keys()]).toEqual(names);
    expect(names).toContain("run_task");
  });
});

describe("gateway over HTTP", () => {
  let gateway: Awaited<ReturnType<typeof startGateway>>;
  // A stand-in for serve's Ollama passthrough: records what it is sent and answers in Ollama's shapes.
  const upstream: Array<{ path: string; body: Record<string, any>; headers: Record<string, unknown> }> = [];
  // What the fake Ollama reports as loaded (`/api/ps`).
  let loaded: Array<{ name: string; context_length: number }> = [];
  let serve: import("node:http").Server;
  beforeAll(async () => {
    const http = await import("node:http");
    serve = http.createServer((request, response) => {
      let raw = "";
      request.on("data", (chunk) => { raw += chunk; });
      request.on("end", () => {
        if (request.url === "/api/ps") {
          response.writeHead(200, { "content-type": "application/json" });
          return void response.end(JSON.stringify({ models: loaded }));
        }
        if (request.url === "/_freellama/v1/config") {
          response.writeHead(200, { "content-type": "application/json" });
          return void response.end(JSON.stringify({ effective: { context: { context_shift: "off" } } }));
        }
        upstream.push({ path: request.url ?? "", body: JSON.parse(raw || "{}"), headers: request.headers });
        if (request.url === "/api/chat") {
          response.writeHead(200, { "content-type": "application/x-ndjson" });
          const second = JSON.stringify({ model: "bonsai-2:9b", message: { role: "assistant", content: "xed." }, done: false });
          response.write(`${JSON.stringify({ model: "bonsai-2:9b", message: { role: "assistant", content: "Fi" }, done: false })}\n${second.slice(0, 20)}`);
          // The second line arrives split across two chunks.
          setTimeout(() => {
            response.write(`${second.slice(20)}\n${JSON.stringify({ model: "bonsai-2:9b", message: { role: "assistant", content: "" }, done: true, done_reason: "stop", eval_count: 2 })}\n`);
            response.end();
          }, 20);
        } else if (request.url === "/v1/chat/completions" || request.url === "/v1/responses") {
          // A slow SSE stream: a client may leave in the middle of it.
          response.writeHead(200, { "content-type": "text/event-stream" });
          response.write('data: {"choices":[{"delta":{"content":"a"}}]}\n\n');
          const timer = setInterval(() => response.write('data: {"choices":[{"delta":{"content":"b"}}]}\n\n'), 10);
          setTimeout(() => { clearInterval(timer); response.end("data: [DONE]\n\n"); }, 200);
          response.on("close", () => clearInterval(timer));
        } else if (request.url === "/v1/messages") {
          response.writeHead(200, { "content-type": "text/event-stream" });
          response.end('event: message_start\ndata: {"type":"message_start"}\n\nevent: message_stop\ndata: {"type":"message_stop"}\n\n');
        } else {
          response.writeHead(404, { "content-type": "application/json" });
          response.end('{"error":"not found"}');
        }
      });
    });
    await new Promise<void>((resolve) => serve.listen(0, "127.0.0.1", () => resolve()));
    const address = serve.address() as { port: number };
    gateway = await startGateway({ port: 0, serveEndpoint: `http://127.0.0.1:${address.port}` });
  });
  afterAll(async () => {
    await gateway.close();
    await new Promise<void>((resolve) => serve.close(() => resolve()));
  });
  beforeEach(() => { upstream.length = 0; loaded = []; });
  const post = (path: string, body: unknown, headers: Record<string, string> = {}, signal?: AbortSignal) =>
    fetch(`${gateway.url}${path}`, { method: "POST", body: JSON.stringify(body), headers, signal });

  it("streams a virtual Ollama chat through Ollama itself, rewritten with the pick, system prompt, options and context", async () => {
    const reply = await post("/api/chat", { model: "freellama/fix-bug:latest", messages: [{ role: "user", content: "crash" }], options: { temperature: 0 } });
    const lines = (await reply.text()).trim().split("\n").map((line) => JSON.parse(line));
    expect(lines.map((line) => line.message?.content)).toEqual(["Fi", "xed.", ""]);
    expect(lines.at(-1)).toMatchObject({ done: true, freellama: { model: "bonsai-2:9b", job: "coding", num_ctx: 4096 } });
    const sent = upstream[0]!;
    expect(sent.path).toBe("/api/chat");
    expect(sent.body.model).toBe("bonsai-2:9b");
    expect(sent.body.messages[0].role).toBe("system");
    expect(sent.body.messages.at(-1)).toEqual({ role: "user", content: "crash" });
    // The caller's temperature wins; the routed window (raised to hold the turn) and the job's other options fill in.
    expect(sent.body.options.temperature).toBe(0);
    expect(sent.body.options.num_ctx).toBeGreaterThanOrEqual(4096);
    expect(fixture.run).not.toHaveBeenCalled();
  });

  it("sends serve's context-shift setting on native streams, so they reuse managed runners", async () => {
    await (await post("/api/chat", { model: "freellama/summarize", messages: [{ role: "user", content: "x" }] })).text();
    expect(upstream[0]!.body.shift).toBe(false);
  });

  it("checks the memory assessment before an OpenAI-layer stream loads a model, and refuses a held load", async () => {
    fixture.route.mockResolvedValue(JSON.stringify({
      selected_model: "bonsai-2:9b", options: { num_ctx: 4096 },
      execution: { resource_assessment: { status: "held_insufficient_capacity", reason: "needs 9 GB, 4 GB free" } },
    }));
    const reply = await post("/v1/chat/completions", { model: "freellama/review-code", stream: true, messages: [{ role: "user", content: "x" }] });
    expect(reply.status).toBe(503);
    expect((await reply.json()).error).toMatch(/held_insufficient_capacity: needs 9 GB/);
    expect(upstream).toHaveLength(0);
    expect(fixture.warm).not.toHaveBeenCalled();
  });

  it("loads a model that is not resident through managed admission before streaming", async () => {
    await (await post("/api/chat", { model: "freellama/summarize", messages: [{ role: "user", content: "x" }] })).text();
    expect(fixture.warm).toHaveBeenCalledTimes(1);
    expect(fixture.warm.mock.calls[0]![1]).toMatchObject({ model: "bonsai-2:9b", context_tokens: upstream[0]!.body.options.num_ctx });
  });

  it("keeps a loaded runner's window, so nothing reloads and its prompt cache survives", async () => {
    loaded = [{ name: "bonsai-2:9b", context_length: 16384 }];
    const lines = (await (await post("/api/chat", { model: "freellama/summarize", messages: [{ role: "user", content: "x" }] })).text()).trim().split("\n");
    expect(upstream[0]!.body.options.num_ctx).toBe(16384);
    expect(JSON.parse(lines.at(-1)!).freellama).toMatchObject({ num_ctx: 16384, context_source: "resident_runner" });
    expect(fixture.warm).not.toHaveBeenCalled();
  });

  it("resizes a runner more than 4x bigger than the task, instead of holding its K/V memory", async () => {
    loaded = [{ name: "bonsai-2:9b", context_length: 131072 }];
    await (await post("/api/chat", { model: "freellama/summarize", messages: [{ role: "user", content: "x" }] })).text();
    expect(upstream[0]!.body.options.num_ctx).toBeLessThan(131072);
    expect(fixture.warm).toHaveBeenCalledTimes(1);
  });

  it("refuses a stream whose load the memory gate refuses, without reaching Ollama", async () => {
    fixture.warm.mockRejectedValue(new Error(JSON.stringify({ code: "resource_admission_unavailable", error: "host resource admission deadline exceeded; needs 9 GB, 4 GB free" })));
    const reply = await post("/api/chat", { model: "freellama/summarize", messages: [{ role: "user", content: "x" }] });
    expect(reply.status).toBe(503);
    expect((await reply.json()).error).toMatch(/needs 9 GB/);
    expect(upstream).toHaveLength(0);
  });

  it("survives a client that leaves mid-stream, and keeps serving", async () => {
    const controller = new AbortController();
    const reply = await post("/v1/chat/completions", { model: "freellama/review-code", stream: true, messages: [{ role: "user", content: "x" }] }, {}, controller.signal);
    const reader = reply.body!.getReader();
    await reader.read();
    controller.abort();
    await new Promise((resolve) => setTimeout(resolve, 50));
    const tags = await fetch(`${gateway.url}/api/tags`);
    expect(tags.status).toBe(200);
  });

  it("maps a JSON workflow's format and thinking off onto a streamed OpenAI request", async () => {
    await (await post("/v1/chat/completions", { model: "freellama/extract-json", stream: true, messages: [{ role: "user", content: '{"type":"object","properties":{"a":{"type":"string"}}}' }] })).text();
    const sent = upstream[0]!.body;
    expect(sent.response_format).toMatchObject({ type: "json_schema", json_schema: { schema: { properties: { a: {} } } } });
    expect(sent.reasoning_effort).toBe("none");
  });

  it("leaves a caller's own system message and think setting alone", async () => {
    fixture.live.mockResolvedValue([[{ name: "bonsai-2:9b", capabilities: ["completion", "thinking"] }], 64e9]);
    await (await post("/api/chat", { model: "freellama/summarize", think: true, messages: [{ role: "system", content: "mine" }, { role: "user", content: "x" }] })).text();
    expect(upstream[0]!.body.messages).toEqual([{ role: "system", content: "mine" }, { role: "user", content: "x" }]);
    expect(upstream[0]!.body.think).toBe(true);
  });

  it("serves Anthropic Messages for a virtual model through Ollama's own endpoint", async () => {
    const reply = await post("/v1/messages", { model: "freellama/review-code", max_tokens: 100, stream: true, messages: [{ role: "user", content: "def f(): pass" }] }, { "x-api-key": "ollama", "anthropic-version": "2023-06-01" });
    expect(await reply.text()).toContain("message_stop");
    const sent = upstream[0]!;
    expect(sent.body).toMatchObject({ model: "bonsai-2:9b", stream: true });
    expect(typeof sent.body.system).toBe("string");
    expect(sent.headers["anthropic-version"]).toBe("2023-06-01");
  });

  it("runs a non-streaming Ollama chat as a managed task with its receipt's stats", async () => {
    const reply = await post("/api/chat", { model: "freellama/fix-bug:latest", stream: false, messages: [{ role: "user", content: "crash" }] });
    expect(await reply.json()).toMatchObject({ message: { content: "Fixed." }, done: true, prompt_eval_count: 120, freellama: { model: "bonsai-2:9b", cached_prompt_tokens: 100 } });
    expect(upstream).toHaveLength(0);
  });

  it("answers the ollama CLI's preload and show without running a model", async () => {
    expect(await (await post("/api/chat", { model: "freellama/fix-bug:latest", messages: [] })).json()).toMatchObject({ done: true, done_reason: "load" });
    expect(await (await post("/api/show", { model: "freellama/describe-screen" })).json()).toMatchObject({ capabilities: ["completion", "vision"] });
    expect(fixture.run).not.toHaveBeenCalled();
    expect(upstream).toHaveLength(0);
  });

  it("serves OpenAI chat completions as a managed task when not streamed", async () => {
    const plain = await (await post("/v1/chat/completions", { model: "freellama/review-code", messages: [{ role: "user", content: "x" }] })).json();
    expect(plain).toMatchObject({ object: "chat.completion", choices: [{ message: { content: "Fixed." }, finish_reason: "stop" }] });
  });

  it("reports a spent output budget as finish_reason length", async () => {
    fixture.run.mockImplementation(async () => answer("", { response: { message: { role: "assistant", content: "", thinking: "hmm" }, done_reason: "length" } }));
    const reply = await (await post("/v1/chat/completions", { model: "freellama/auto", max_tokens: 5, messages: [{ role: "user", content: "x" }] })).json();
    expect(reply.choices[0]).toMatchObject({ finish_reason: "length", message: { content: "", reasoning: "hmm" } });
  });

  it("reports a job that cannot pick a model as 503 with its message, streamed or not", async () => {
    fixture.live.mockResolvedValue([null, null]);
    for (const stream of [false, true]) {
      const reply = await post("/api/chat", { model: "freellama/fix-bug", stream, messages: [{ role: "user", content: "x" }] });
      expect(reply.status).toBe(503);
      expect((await reply.json()).error).toMatch(/installed models could not be read .*job_inventory_unavailable/);
    }
  });

  it("lists virtual models with a digest the ollama CLI can print, and refuses other origins", async () => {
    const tags = await (await fetch(`${gateway.url}/api/tags`)).json();
    expect(tags.models[0].digest).toMatch(/^[0-9a-f]{64}$/);
    expect((await fetch(`${gateway.url}/api/tags`, { headers: { origin: "https://evil.example" } })).status).toBe(403);
    expect(await (await post("/api/generate", { model: "freellama/describe-screen", stream: false, images: ["AAAA"] })).json()).not.toMatchObject({ done_reason: "load" });
  });
});

describe("freellama run streaming", () => {
  let serve: import("node:http").Server;
  let endpoint = "";
  const sent: Array<Record<string, any>> = [];
  let reply: string[] = [];
  beforeAll(async () => {
    const http = await import("node:http");
    serve = http.createServer((request, response) => {
      let raw = "";
      request.on("data", (chunk) => { raw += chunk; });
      request.on("end", () => {
        if (request.url === "/api/ps") return void response.end(JSON.stringify({ models: [] }));
        if (request.url === "/_freellama/v1/config") return void response.end(JSON.stringify({ context_shift: "off" }));
        sent.push({ ...JSON.parse(raw || "{}"), _path: request.url });
        response.writeHead(200, { "content-type": "application/x-ndjson" });
        response.end(reply.map((line) => `${line}\n`).join(""));
      });
    });
    await new Promise<void>((resolve) => serve.listen(0, "127.0.0.1", () => resolve()));
    endpoint = `http://127.0.0.1:${(serve.address() as { port: number }).port}`;
  });
  afterAll(() => new Promise<void>((resolve) => serve.close(() => resolve())));
  beforeEach(() => {
    sent.length = 0;
    reply = [
      JSON.stringify({ message: { role: "assistant", content: "Hel" }, done: false }),
      JSON.stringify({ message: { role: "assistant", content: "lo" }, done: false }),
      JSON.stringify({ message: { role: "assistant", content: "" }, done: true, done_reason: "stop", eval_count: 2, eval_duration: 1e8, prompt_eval_count: 5 }),
    ];
  });
  const capture = () => {
    const out: string[] = [];
    const err: string[] = [];
    return { out, err, io: { out: (text: string) => out.push(`${text}\n`), err: (text: string) => err.push(text), write: (text: string) => out.push(text) } };
  };

  it("streams a direct tag's tokens in order, with serve's shift setting and an admitted load", async () => {
    const { out, io } = capture();
    const code = await streamRun(streamTarget({ model: "bonsai-2:9b", prompt: "hi" })!, { model: "bonsai-2:9b", prompt: "hi", endpoint }, render, io, new AbortController().signal, false);
    expect(code).toBe(0);
    expect(out.join("")).toBe("Hello\n");
    expect(sent[0]).toMatchObject({ model: "bonsai-2:9b", stream: true, shift: false, messages: [{ role: "user", content: "hi" }] });
    expect(fixture.warm).toHaveBeenCalledTimes(1);
  });

  it("streams a workflow on its pick with the workflow's system prompt, and prints stats with --verbose", async () => {
    const { err, io } = capture();
    await streamRun(streamTarget({ workflow: "summarize-text", prompt: "text" })!, { workflow: "summarize-text", prompt: "text", endpoint }, { ...render, verbose: true }, io, new AbortController().signal, false);
    expect(sent[0]!.messages[0].role).toBe("system");
    expect(err.at(-1)).toMatch(/output 2 tokens, 20\.0 tokens\/s/);
  });

  it("fills in the middle on /api/generate with the prompt and --suffix", async () => {
    reply = [
      JSON.stringify({ response: "  return a + b", done: false }),
      JSON.stringify({ response: "", done: true, done_reason: "stop" }),
    ];
    const { out, io } = capture();
    const args = { model: "qwen-coder:1", prompt: "def add(a, b):\n", suffix: "\n\nprint(add(1, 2))", endpoint };
    expect(await streamRun(streamTarget(args)!, args, render, io, new AbortController().signal, false)).toBe(0);
    expect(out.join("")).toBe("  return a + b\n");
    expect(sent[0]).toMatchObject({ _path: "/api/generate", model: "qwen-coder:1", prompt: "def add(a, b):\n", suffix: "\n\nprint(add(1, 2))" });
    expect(streamTarget({ suffix: "x", prompt: "y" })).toBeUndefined();
    expect(() => parseRunArgs(["-w", "fix-bug", "--suffix", "x", "y"])).toThrow(/fills in the middle/);
  });

  it("fails an empty answer and keeps decisions, batches and tools on the managed path", async () => {
    reply = [JSON.stringify({ message: { role: "assistant", content: "" }, done: true, done_reason: "length" })];
    const { err, io } = capture();
    expect(await streamRun(streamTarget({ model: "m:1", prompt: "x" })!, { model: "m:1", prompt: "x", endpoint }, render, io, new AbortController().signal, false)).toBe(1);
    expect(err.join("\n")).toMatch(/empty answer: the output budget ran out/);
    expect(streamTarget({ job: "classify", questions: {} })).toBeUndefined();
    expect(streamTarget({ prompt: "x", tools: [{}] })).toBeUndefined();
    expect(streamTarget({ prompt: "x", defer: true })).toBeUndefined();
  });
});

describe("agent guide", () => {
  it("names every workflow, so a new one cannot be left out of the freellama prompt", async () => {
    const { AGENT_GUIDE, WORKFLOW_NAMES } = await import("../../src/local-prompts.js");
    for (const name of WORKFLOW_NAMES) expect(AGENT_GUIDE).toContain(name);
  });
});

describe("run_task's advertised schema is loose, its accepted one is not", () => {
  it.each([
    [{ workflow: "no-such-workflow", prompt: "x" }, /workflow/],
    [{ task: "decision", state: "x", questions: { q: { type: "noul" } } }, /instructions/],
    [{ task: "embedding", input: "x", nearest: { query: "q", bogus: 1 } }, /nearest/],
    [{ prompt: "x", sessionId: "zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz" }, /sessionId/],
    [{ prompt: "x", tools: [{ type: "function" }] }, /tools/],
    [{ messages: [{ role: "wizard", content: "x" }] }, /messages/],
  ])("refuses %j", async (args, message) => {
    // Refused either by the advertised schema (a usage error) or by the handler's strict check.
    let text: string;
    try {
      const result = await callTool("run_task", args as Record<string, unknown>);
      expect(result.isError).toBe(true);
      text = resultTextOf(result);
    } catch (error) {
      text = (error as Error).message;
    }
    expect(text).toMatch(message);
  });
});

function resultTextOf(result: { content?: Array<{ text?: string }> }): string {
  return (result.content ?? []).map((part) => part.text ?? "").join("\n");
}
