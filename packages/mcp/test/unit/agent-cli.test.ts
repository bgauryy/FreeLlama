import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

// The CLI and gateway run the real run_task handler; only serve and the live inventory are fakes.
const fixture = vi.hoisted(() => ({
  run: vi.fn(),
  live: vi.fn(),
  route: vi.fn(),
}));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: fixture.run, getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: (...args: unknown[]) => fixture.route(...args),
  createSession: vi.fn(), deleteSession: vi.fn(), killSession: vi.fn(), listModels: vi.fn(), warmModelRequest: vi.fn(),
  listTaskJobs: vi.fn(), createScope: vi.fn(), getScope: vi.fn(), forkScope: vi.fn(), deleteScope: vi.fn(),
}));
vi.mock("../../src/model-library.js", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  catalogLiveState: (...args: unknown[]) => fixture.live(...args),
}));

import { parseRunArgs, UsageError } from "../../src/agent-cli/args.js";
import { renderResult, refusalOf, splitAnswer } from "../../src/agent-cli/output.js";
import { callTool } from "../../src/agent-cli/registry.js";
import { chatTaskArgs, fromOpenAiRequest, openAiCompletion, originAllowed, startGateway, virtualModel, virtualModels } from "../../src/agent-cli/gateway.js";
import { chatModelName, main } from "../../src/agent-cli/main.js";

const render = { json: false, verbose: false, quiet: false, hideThinking: false };
const answer = (content: string, extra: Record<string, unknown> = {}) => JSON.stringify({
  route: { selected_model: "lfm3:1.7b-q4_K_M" },
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
  fixture.live.mockResolvedValue([[{ name: "lfm3:1.7b-q4_K_M", capabilities: ["completion"] }], 64e9]);
  fixture.route.mockReset();
  fixture.route.mockResolvedValue(JSON.stringify({ selected_model: "lfm3:1.7b-q4_K_M", options: { num_ctx: 4096 } }));
});

describe("fllama run flags", () => {
  it("reads the first word as a model only when it could be a tag and nothing else names one", () => {
    expect(parseRunArgs(["gemma4", "Why", "blue?"]).args).toMatchObject({ model: "gemma4", prompt: "Why blue?" });
    expect(parseRunArgs(["Why is the sky blue?"]).args).toEqual({ prompt: "Why is the sky blue?" });
    expect(parseRunArgs(["-w", "fix-bug", "parser", "drops", "a", "token"]).args).toEqual({ workflow: "fix-bug", prompt: "parser drops a token" });
  });

  it("keeps ollama's flag meanings: bare --think is true, --keepalive, --format json or a schema", () => {
    const { args } = parseRunArgs(["m:1", "hi", "--think", "--keepalive", "5m", "--format", '{"type":"object","properties":{}}']);
    expect(args).toMatchObject({ think: true, keepAlive: "5m", format: { type: "object", properties: {} } });
    expect(parseRunArgs(["m:1", "hi", "--think=low"]).args.think).toBe("low");
    expect(() => parseRunArgs(["m:1", "--think=maybe"])).toThrow(UsageError);
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
        q1: { type: "noul", instructions: "Refund?" },
        q2: { type: "choice", instructions: "Team?", criteria: { billing: "Payments", other: null } },
        q3: { type: "score", instructions: "Urgency?", criteria: ["Routine", "Now"] },
      },
    });
    expect(() => parseRunArgs(["--choice", "a=b"])).toThrow(/must follow the --ask/);
  });

  it("runs a question with no model on the classify job's tested pick, not on routing's first rank", () => {
    const { args } = parseRunArgs(["--ask", "Refund?"], "charged twice");
    expect(args).toEqual({ job: "classify", state: "charged twice", questions: { q1: { type: "noul", instructions: "Refund?" } } });
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
    expect(() => parseRunArgs(["m:1", "--dimensions", "256"])).toThrow(/ollama run MODEL --dimensions/);
    expect(() => parseRunArgs(["m:1", "--voice", "x"])).toThrow(/fllama tool audio/);
    expect(() => parseRunArgs(["m:1", "--truncate=false"])).toThrow(UsageError);
    expect(() => parseRunArgs(["-w", "no-such"])).toThrow(/fllama workflows/);
    expect(() => parseRunArgs(["--bogus"])).toThrow(UsageError);
  });

  it("marks a run with no input as promptless (a chat on a terminal)", () => {
    expect(parseRunArgs(["gemma4"]).promptless).toBe(true);
    expect(parseRunArgs(["gemma4", "-f", "x.ts"]).promptless).toBe(false);
  });
});

describe("fllama output", () => {
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
    expect(renderResult({ isError: true, content: [{ type: "text", text: refusal }] }, render)).toEqual({ stdout: "", stderr: ["no catalog pick fits now (job_no_model_fits)"], exitCode: 1 });
    expect(renderResult({ isError: true, content: [{ type: "text", text: refusal }] }, { ...render, json: true }).stdout).toBe(refusal);
    expect(refusalOf(refusal).unavailable).toBe(true);
    expect(refusalOf("model output was invalid").unavailable).toBe(false);
  });

  it("prints only the first code block with --extract, or the whole answer with a note", () => {
    const text = "[m] Here is the fix:\n```ts\nconst a = 1;\n\nconst b = 2;\n```\nand another\n```\nx\n```";
    expect(renderResult({ content: [{ type: "text", text }], structuredContent: {} }, { ...render, extract: true }).stdout).toBe("const a = 1;\n\nconst b = 2;");
    const plain = renderResult({ content: [{ type: "text", text: "[m] no code" }], structuredContent: {} }, { ...render, extract: true });
    expect(plain).toMatchObject({ stdout: "no code", stderr: ["fllama: --extract found no code block; printing the whole answer"] });
  });

  it("prints a deferred job's id on stdout", () => {
    const rendered = renderResult({ structuredContent: { deferred: true, job: { id: "abc", status: "queued" } } }, render);
    expect(rendered).toEqual({ stdout: "abc", stderr: ["deferred; poll with: fllama job abc"], exitCode: 0 });
  });
});

describe("fllama tool registry", () => {
  it("runs the real run_task handler: a workflow gets the job's pick and system prompt", async () => {
    const result = await callTool("run_task", parseRunArgs(["-w", "fix-bug", "parser drops a token"]).args);
    expect(result.isError).toBeFalsy();
    const request = fixture.run.mock.calls[0]![1];
    expect(request.model).toBe("lfm3:1.7b-q4_K_M");
    expect(request.messages[0].role).toBe("system");
    expect(request.messages.at(-1).content).toContain("parser drops a token");
    expect(result.structuredContent?.job_profile).toMatchObject({ job: "coding", workflow: "fix-bug", model: "lfm3:1.7b-q4_K_M" });
  });

  it("refuses what MCP refuses, with the tool's own schema", async () => {
    await expect(callTool("run_task", { bogus: 1 })).rejects.toThrow(/run_task: .*bogus/);
    await expect(callTool("no_such_tool", {})).rejects.toThrow(/fllama tools/);
  });

  it("exits 2 on a usage error and 0 on help", async () => {
    const lines: string[] = [];
    const io = { out: (text: string) => lines.push(text), err: (text: string) => lines.push(text) };
    expect(await main(["run", "--dimensions", "3", "x"], io)).toBe(2);
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

  it("sends one user turn as a workflow prompt, so job mode applies everything", async () => {
    const args = await chatTaskArgs(virtualModel("freellama/fix-bug")!, { messages: [{ role: "user", content: "it crashes" }], options: { num_ctx: 4096, temperature: 0 } });
    expect(args).toEqual({ workflow: "fix-bug", prompt: "it crashes", contextTokens: 4096, options: { temperature: 0 } });
  });

  it("prepends the workflow's system prompt to a longer conversation only when the caller sent none", async () => {
    const messages = [{ role: "user", content: "a" }, { role: "assistant", content: "b" }, { role: "user", content: "c" }];
    const without = await chatTaskArgs(virtualModel("freellama/explain-code")!, { messages });
    expect(without.model).toBe("lfm3:1.7b-q4_K_M");
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
    }, "freellama/auto", 1);
    expect(completion).toMatchObject({
      model: "freellama/auto", usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 },
      choices: [{ finish_reason: "tool_calls", message: { tool_calls: [{ type: "function", function: { name: "f", arguments: '{"a":1}' } }] } }],
      freellama: { model: "m", checks: ["c"] },
    });
  });

  it("refuses browser pages from other origins", () => {
    expect(originAllowed(undefined)).toBe(true);
    expect(originAllowed("http://localhost:3000")).toBe(true);
    expect(originAllowed("https://evil.example")).toBe(false);
  });
});

describe("gateway over HTTP", () => {
  let gateway: Awaited<ReturnType<typeof startGateway>>;
  // A stand-in for serve's Ollama passthrough: records what it is sent and answers in Ollama's shapes.
  const upstream: Array<{ path: string; body: Record<string, any>; headers: Record<string, unknown> }> = [];
  let serve: import("node:http").Server;
  beforeAll(async () => {
    const http = await import("node:http");
    serve = http.createServer((request, response) => {
      let raw = "";
      request.on("data", (chunk) => { raw += chunk; });
      request.on("end", () => {
        upstream.push({ path: request.url ?? "", body: JSON.parse(raw || "{}"), headers: request.headers });
        if (request.url === "/api/chat") {
          response.writeHead(200, { "content-type": "application/x-ndjson" });
          response.write(`${JSON.stringify({ model: "lfm3:1.7b-q4_K_M", message: { role: "assistant", content: "Fi" }, done: false })}\n`);
          response.write(`${JSON.stringify({ model: "lfm3:1.7b-q4_K_M", message: { role: "assistant", content: "xed." }, done: false })}\n${JSON.stringify({ model: "lfm3:1.7b-q4_K_M", message: { role: "assistant", content: "" }, done: true, done_reason: "stop", eval_count: 2 })}\n`);
          response.end();
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
  beforeEach(() => { upstream.length = 0; });
  const post = (path: string, body: unknown, headers: Record<string, string> = {}) =>
    fetch(`${gateway.url}${path}`, { method: "POST", body: JSON.stringify(body), headers });

  it("streams a virtual Ollama chat through Ollama itself, rewritten with the pick, system prompt, options and context", async () => {
    const reply = await post("/api/chat", { model: "freellama/fix-bug:latest", messages: [{ role: "user", content: "crash" }], options: { temperature: 0 } });
    const lines = (await reply.text()).trim().split("\n").map((line) => JSON.parse(line));
    expect(lines.map((line) => line.message?.content)).toEqual(["Fi", "xed.", ""]);
    expect(lines.at(-1)).toMatchObject({ done: true, freellama: { model: "lfm3:1.7b-q4_K_M", job: "coding", num_ctx: 4096 } });
    const sent = upstream[0]!;
    expect(sent.path).toBe("/api/chat");
    expect(sent.body.model).toBe("lfm3:1.7b-q4_K_M");
    expect(sent.body.messages[0].role).toBe("system");
    expect(sent.body.messages.at(-1)).toEqual({ role: "user", content: "crash" });
    // The caller's temperature wins; the routed window and the job's other options fill in.
    expect(sent.body.options).toMatchObject({ temperature: 0, num_ctx: 4096 });
    expect(fixture.run).not.toHaveBeenCalled();
  });

  it("leaves a caller's own system message and think setting alone", async () => {
    fixture.live.mockResolvedValue([[{ name: "lfm3:1.7b-q4_K_M", capabilities: ["completion", "thinking"] }], 64e9]);
    await (await post("/api/chat", { model: "freellama/summarize", think: true, messages: [{ role: "system", content: "mine" }, { role: "user", content: "x" }] })).text();
    expect(upstream[0]!.body.messages).toEqual([{ role: "system", content: "mine" }, { role: "user", content: "x" }]);
    expect(upstream[0]!.body.think).toBe(true);
  });

  it("serves Anthropic Messages for a virtual model through Ollama's own endpoint", async () => {
    const reply = await post("/v1/messages", { model: "freellama/review-code", max_tokens: 100, stream: true, messages: [{ role: "user", content: "def f(): pass" }] }, { "x-api-key": "ollama", "anthropic-version": "2023-06-01" });
    expect(await reply.text()).toContain("message_stop");
    const sent = upstream[0]!;
    expect(sent.body).toMatchObject({ model: "lfm3:1.7b-q4_K_M", stream: true });
    expect(typeof sent.body.system).toBe("string");
    expect(sent.headers["anthropic-version"]).toBe("2023-06-01");
  });

  it("runs a non-streaming Ollama chat as a managed task with its receipt's stats", async () => {
    const reply = await post("/api/chat", { model: "freellama/fix-bug:latest", stream: false, messages: [{ role: "user", content: "crash" }] });
    expect(await reply.json()).toMatchObject({ message: { content: "Fixed." }, done: true, prompt_eval_count: 120, freellama: { model: "lfm3:1.7b-q4_K_M", cached_prompt_tokens: 100 } });
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
  });
});
