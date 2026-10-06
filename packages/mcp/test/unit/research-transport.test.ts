// Managed/direct transport contracts for both research adapters.
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { createServer, type IncomingMessage } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { runAdapter, type AdapterSpec } from "../../src/research/agent-loop.js";
import { BASH_ADAPTER } from "../../src/research/bash-agent.js";
import { ObservationStore } from "../../src/research/observations.js";
import { OCTOCODE_ADAPTER } from "../../src/research/octocode-agent.js";
import { cpLength } from "../../src/research/text.js";
import {
  HTTPError, PromptCacheUsage, RequestTimeoutError, URLError, chatErrorDetails, chatRequest, requestHeaders,
  retryableChatError, unwrapChatResponse,
} from "../../src/research/transport.js";

const ADAPTERS: Array<[string, AdapterSpec]> = [["bash-agent", BASH_ADAPTER], ["octocode-agent", OCTOCODE_ADAPTER]];
const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function workspace(question: string): { root: string; prompt: string; result: string } {
  const root = mkdtempSync(path.join(tmpdir(), "freellama-transport-test-"));
  roots.push(root);
  const prompt = path.join(root, "prompt.txt");
  writeFileSync(prompt, question, "utf8");
  return { root, prompt, result: path.join(root, "result.json") };
}

/** A request stub that replays scripted responses or throws scripted errors, counting calls. */
function scripted(...outcomes: unknown[]) {
  const stub = Object.assign(async () => {
    stub.calls += 1;
    const outcome = outcomes.length > 1 ? outcomes.shift() : outcomes[0];
    if (outcome instanceof Error) throw outcome;
    return outcome;
  }, { calls: 0 });
  return stub;
}

const quiet = () => undefined;
/** A Node socket error with the given code. */
const socketError = (code: string) => Object.assign(new Error(code), { code });

describe("transport", () => {
  it("HTTP refusal receipts are bounded and exclude nested request secrets", () => {
    const body = {
      error: "inference refused", code: "upstream_task_failed",
      lifecycle: { status: "failed", reason: "cleanup incomplete", request: { prompt: "private" }, authorization: "secret" },
      request_options: { token: "secret" },
    };
    const [diagnostic, details] = chatErrorDetails(new HTTPError(502, "Bad Gateway", Buffer.from(JSON.stringify(body))));
    expect(diagnostic).toBe("HTTP 502: inference refused");
    expect(details).toEqual({ status: 502, receipt: {
      error: "inference refused", code: "upstream_task_failed",
      lifecycle: { status: "failed", reason: "cleanup incomplete" },
    } });
    for (const raw of [Buffer.from("<html>private</html>"), Buffer.from('{"error": "' + "private".repeat(10_000) + '"}'),
      Buffer.from('{"error": {"prompt": "private"}}'), Buffer.from("[]")]) {
      const [refusal, refusalDetails] = chatErrorDetails(new HTTPError(422, "Unprocessable Entity", raw));
      expect(refusalDetails).toEqual({ status: 422 });
      expect(refusal).not.toContain("private");
    }
  });

  it("both adapters preserve refusals without replaying or echoing payloads", async () => {
    const receipt = {
      error: "physical placement is not verified: configured=gpu, observed=unknown",
      code: "placement_unverified",
      reason: "observed evidence required",
      resource_admission: { status: "held", required_available_bytes: 4096 },
      retry_after_seconds: 5,
      lifecycle: { requested: "immediate_unload", status: "verified" },
      placement: { processor: "unknown", status: "unverified" },
    };
    for (const [, adapter] of ADAPTERS) {
      for (const finalizing of [false, true]) {
        const { root, prompt, result } = workspace("private research question");
        const env = {
          FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: root,
          FREELLAMA_BENCH_PROMPT: prompt, FREELLAMA_AGENT_RESULT: result,
          FREELLAMA_AGENT_MANAGED_ENDPOINT: "http://managed.invalid",
          FREELLAMA_AGENT_MAX_TURNS: "1", FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS: "0",
        };
        const refused = new HTTPError(422, "Unprocessable Entity",
          Buffer.from(JSON.stringify({ ...receipt, prompt: "private research question", authorization: "Bearer private-token" })));
        const pending = { message: { content: '{"action":"page","step":999,"page":1}' } };
        const request = finalizing ? scripted(pending, refused) : scripted(refused);
        expect(await runAdapter(adapter, { env, requestJson: request, print: quiet })).toBe(1);
        expect(request.calls).toBe(finalizing ? 2 : 1);
        const text = readFileSync(result, "utf8");
        const record = JSON.parse(text);
        expect(record.model_metadata.transport_error).toEqual({ status: 422, receipt });
        expect(record.final_answer).toContain(receipt.error);
        expect(text).not.toContain("private research question");
        expect(text).not.toContain("private-token");
      }
    }
  });

  it("unverified model identity disables persistent calibration in both adapters", async () => {
    for (const [, adapter] of ADAPTERS) {
      const { root, prompt, result } = workspace("Find one fact.");
      const cachePath = path.join(root, "calibration");
      const env = { FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: root,
        FREELLAMA_BENCH_PROMPT: prompt, FREELLAMA_AGENT_RESULT: result, FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR: cachePath };
      const response = { message: { content: '{"action":"finish","answer":"done"}' }, prompt_eval_count: 6000 };
      let identityCalls = 0;
      const resolveModelIdentity = async () => {
        identityCalls += 1;
        return { identity: "", verified: false, scope: "current_process_only" };
      };
      expect(await runAdapter(adapter, { env, requestJson: scripted(response), resolveModelIdentity, print: quiet })).toBe(0);
      expect(identityCalls).toBe(1);
      expect(existsSync(cachePath)).toBe(false);
      const decoded = JSON.parse(readFileSync(result, "utf8"));
      expect(decoded.model_metadata.calibration_model_identity.verified).toBe(false);
      expect(decoded.model_metadata.context_management.calibration_samples).toBe(1);
    }
  });

  it("both adapters spool results, page, and clean up without retaining full outputs", async () => {
    const payload = "FIRST\n" + "🦙\\\"\r\n".repeat(20_000) + "LAST";
    const cases: Array<[AdapterSpec, Record<string, unknown>]> = [
      [BASH_ADAPTER, { action: "shell", command: "read src/router.py" }],
      [OCTOCODE_ADAPTER, { action: "octocode", tool: "localGetFileContent", queries: { path: "src/router.py" } }],
    ];
    for (const [adapter, action] of cases) {
      const { root, prompt, result } = workspace("Find the router.");
      const requests: Array<{ messages: Array<{ content?: string }> }> = [];
      const actions = [action, { action: "page", step: 1, page: 2 }, { action: "finish", answer: "done" }];
      // A real HTTP endpoint exercises the adapter's own client, as the reference test patched urlopen.
      const server = createServer((request: IncomingMessage, response) => {
        const chunks: Buffer[] = [];
        request.on("data", (chunk: Buffer) => chunks.push(chunk));
        request.on("end", () => {
          requests.push(JSON.parse(Buffer.concat(chunks).toString("utf8")));
          response.setHeader("content-type", "application/json");
          response.end(JSON.stringify({ message: { content: JSON.stringify(actions.shift()) }, prompt_eval_count: 100, eval_count: 10 }));
        });
      });
      await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
      const stores: ObservationStore[] = [];
      let runnerCalls = 0;
      let identityCalls = 0;
      try {
        const env = { FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: root,
          FREELLAMA_BENCH_PROMPT: prompt, FREELLAMA_AGENT_RESULT: result,
          FREELLAMA_OLLAMA_ENDPOINT: `http://127.0.0.1:${(server.address() as AddressInfo).port}` };
        const code = await runAdapter(adapter, {
          env,
          print: quiet,
          runTool: () => {
            runnerCalls += 1;
            return payload;
          },
          observationStore: (pageSize) => {
            const store = new ObservationStore(pageSize);
            stores.push(store);
            return store;
          },
          resolveModelIdentity: async () => {
            identityCalls += 1;
            return { identity: "", verified: false, scope: "current_process_only" };
          },
        });
        expect(code).toBe(0);
      } finally {
        server.close();
      }
      expect(runnerCalls).toBe(1);
      expect(identityCalls).toBe(0);
      const decoded = JSON.parse(readFileSync(result, "utf8"));
      expect(decoded.tool_calls[0].result).toBe(payload);
      expect(decoded.final_answer).toBe("done");
      expect(decoded.tool_calls[0].status).toBe("ok");
      expect(requests.at(-1)!.messages.at(-1)!.content).toContain('"page":2');
      expect(requests.every((request) => request.messages.every((message) => cpLength(message.content ?? "") < 10_000))).toBe(true);
      expect(stores.every((store) => !existsSync(store.directory))).toBe(true);
    }
  });

  it("both adapter loops do not replay timeouts and report cache reads", async () => {
    for (const [, adapter] of ADAPTERS) {
      const { root, prompt, result } = workspace("Find one fact.");
      const env = { FREELLAMA_TARGET_MODEL: "test:latest", FREELLAMA_BENCH_WORKSPACE: root,
        FREELLAMA_BENCH_PROMPT: prompt, FREELLAMA_AGENT_RESULT: result, FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS: "0" };
      const timedOut = scripted(new RequestTimeoutError());
      expect(await runAdapter(adapter, { env, requestJson: timedOut, print: quiet })).toBe(1);
      expect(timedOut.calls).toBe(1);
      const response = { message: { content: '{"action":"finish","answer":"done"}' },
        prompt_eval_count: 100, prompt_eval_cached_count: 40, eval_count: 5 };
      const finished = scripted(response);
      expect(await runAdapter(adapter, { env, requestJson: finished, print: quiet })).toBe(0);
      expect(finished.calls).toBe(1);
      const record = JSON.parse(readFileSync(result, "utf8"));
      expect(record.usage.input_tokens).toBe(100);
      expect(record.usage.cache_read_tokens).toBe(40);
      expect(record.model_metadata.cache_token_metrics.status).toBe("reported");
    }
  });

  it("retries only when inference was refused", () => {
    expect(retryableChatError(new HTTPError(503, "busy"))).toBe(true);
    expect(retryableChatError(new URLError(socketError("ECONNREFUSED")))).toBe(true);
    for (const error of [new RequestTimeoutError(), new URLError(new RequestTimeoutError()),
      new HTTPError(504, "timeout"), new HTTPError(400, "invalid"), new URLError(socketError("ECONNRESET"))]) {
      expect(retryableChatError(error)).toBe(false);
    }
  });

  it("cache usage distinguishes missing, zero and partial reporting", () => {
    const cache = new PromptCacheUsage();
    cache.observe({ prompt_eval_count: 10 });
    expect(cache.tokens).toBeNull();
    cache.observe({ prompt_eval_count: 10, prompt_eval_cached_count: 0 });
    expect(cache.tokens).toBeNull(); // A missing turn must not become an invented zero.
    expect(cache.metadata().reported_tokens).toBe(0);
    expect(cache.metadata().status).toBe("partially_reported");
  });

  it("cache usage counts the cached subset without double-counting the prompt", () => {
    const cache = new PromptCacheUsage();
    cache.observe({ prompt_eval_count: 10, prompt_eval_cached_count: 4 });
    cache.observe({ prompt_eval_count: 20, prompt_eval_cached_count: 8 });
    expect(cache.tokens).toBe(12);
    expect(cache.metadata().status).toBe("reported");
    expect(Number(String(cache.metadata().hit_ratio))).toBeCloseTo(0.4);
  });

  it("invalid cache counts remain unknown", () => {
    for (const value of [-1, 11, true, "4"]) {
      const cache = new PromptCacheUsage();
      cache.observe({ prompt_eval_count: 10, prompt_eval_cached_count: value });
      expect(cache.tokens).toBeNull();
    }
  });

  it("managed request uses the coding route and keeps num_ctx routing-owned", () => {
    const [url, body] = chatRequest("http://127.0.0.1:11435/_freellama/v1/tasks", "qwen:latest",
      [{ role: "user", content: "inspect" }], { num_ctx: 8192, num_predict: 512, temperature: 0, seed: 42 },
      false, "5m", "prefer_cpu", "observed");
    expect(url).toBe("http://127.0.0.1:11435/_freellama/v1/tasks");
    expect(body.task).toBe("coding");
    expect(body.context_tokens).toBe(8192);
    expect(body.execution_preference).toBe("prefer_cpu");
    expect(body.min_placement_evidence).toBe("observed");
    expect((body.request_options as { options: object }).options).not.toHaveProperty("num_ctx");
  });

  it("direct request remains available for benchmark comparisons", () => {
    const [url, body] = chatRequest("http://127.0.0.1:11434", "qwen:latest", [], { num_ctx: 4096 }, false, "0");
    expect(url).toBe("http://127.0.0.1:11434/api/chat");
    expect((body.options as { num_ctx: number }).num_ctx).toBe(4096);
    expect(body.truncate).toBe(false);
    expect(body.shift).toBe(false);
  });

  it("managed wrapper is unwrapped and the receipt is retained", () => {
    const receipts: Record<string, unknown>[] = [];
    const response = unwrapChatResponse({
      route: { selected_model: "qwen:latest" },
      execution: { backend: "cpu", observation: { processor: "cpu" } },
      admission: { mode: "resident_shared" },
      feedback: { accepted: true },
      response: { message: { content: "{}" } },
    }, receipts);
    expect((response.message as { content: string }).content).toBe("{}");
    expect((receipts[0]!.execution as { backend: string }).backend).toBe("cpu");
  });

  it("bearer token is read from a file", () => {
    const root = mkdtempSync(path.join(tmpdir(), "freellama-token-test-"));
    roots.push(root);
    const tokenFile = path.join(root, "token");
    writeFileSync(tokenFile, "a".repeat(32) + "\n", "utf8");
    expect(requestHeaders({ FREELLAMA_AUTH_TOKEN_FILE: tokenFile }).authorization).toBe(`Bearer ${"a".repeat(32)}`);
  });
});
