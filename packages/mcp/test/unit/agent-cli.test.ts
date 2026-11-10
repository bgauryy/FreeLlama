import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

// The CLI and gateway run the real run_task handler; only serve and the live inventory are fakes.
const fixture = vi.hoisted(() => ({
  run: vi.fn(),
  live: vi.fn(),
}));
vi.mock("../../src/serve-client.js", () => ({
  runTaskRequest: fixture.run, getTaskJob: vi.fn(), cancelTaskJob: vi.fn(), route: vi.fn(),
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
    expect(args).toMatchObject({
      task: "decision", state: "nimble\n\nticket text",
      questions: {
        q1: { type: "noul", instructions: "Refund?" },
        q2: { type: "choice", instructions: "Team?", criteria: { billing: "Payments", other: null } },
        q3: { type: "score", instructions: "Urgency?", criteria: ["Routine", "Now"] },
      },
    });
    expect(() => parseRunArgs(["--choice", "a=b"])).toThrow(/must follow the --ask/);
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
      model: "m", content: "", toolCalls: [{ function: { name: "f", arguments: { a: 1 } } }], checks: ["c"], metrics: { prompt_tokens: 3, output_tokens: 2 }, receipt: {},
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
  beforeAll(async () => {
    gateway = await startGateway({ port: 0, serveEndpoint: "http://127.0.0.1:9" });
  });
  afterAll(() => gateway.close());
  const post = (path: string, body: unknown, headers: Record<string, string> = {}) =>
    fetch(`${gateway.url}${path}`, { method: "POST", body: JSON.stringify(body), headers });

  it("streams an Ollama chat answer as NDJSON with the receipt's stats and checks", async () => {
    const reply = await post("/api/chat", { model: "freellama/fix-bug:latest", messages: [{ role: "user", content: "crash" }] });
    const lines = (await reply.text()).trim().split("\n").map((line) => JSON.parse(line));
    expect(lines[0]).toMatchObject({ model: "freellama/fix-bug:latest", message: { role: "assistant", content: "Fixed." }, done: false });
    expect(lines[1]).toMatchObject({ done: true, done_reason: "stop", prompt_eval_count: 120, eval_count: 8, freellama: { model: "lfm3:1.7b-q4_K_M", cached_prompt_tokens: 100 } });
  });

  it("answers the ollama CLI's preload and show without running a model", async () => {
    expect(await (await post("/api/chat", { model: "freellama/fix-bug:latest", messages: [] })).json()).toMatchObject({ done: true, done_reason: "load" });
    expect(await (await post("/api/show", { model: "freellama/describe-screen" })).json()).toMatchObject({ capabilities: ["completion", "vision"] });
    expect(fixture.run).not.toHaveBeenCalled();
  });

  it("serves OpenAI chat completions, streamed as SSE when asked", async () => {
    const plain = await (await post("/v1/chat/completions", { model: "freellama/review-code", messages: [{ role: "user", content: "x" }] })).json();
    expect(plain).toMatchObject({ object: "chat.completion", choices: [{ message: { content: "Fixed." }, finish_reason: "stop" }] });
    const stream = await (await post("/v1/chat/completions", { model: "freellama/review-code", stream: true, messages: [{ role: "user", content: "x" }] })).text();
    expect(stream.trim().split("\n\n").at(-1)).toBe("data: [DONE]");
  });

  it("reports a job that cannot pick a model as 503 with its message", async () => {
    fixture.live.mockResolvedValue([null, null]);
    const reply = await post("/api/chat", { model: "freellama/fix-bug", stream: false, messages: [{ role: "user", content: "x" }] });
    expect(reply.status).toBe(503);
    expect((await reply.json()).error).toMatch(/installed models could not be read .*job_inventory_unavailable/);
  });

  it("lists virtual models with a digest the ollama CLI can print, and refuses other origins", async () => {
    const tags = await (await fetch(`${gateway.url}/api/tags`)).json();
    expect(tags.models[0].digest).toMatch(/^[0-9a-f]{64}$/);
    expect((await fetch(`${gateway.url}/api/tags`, { headers: { origin: "https://evil.example" } })).status).toBe(403);
  });
});
