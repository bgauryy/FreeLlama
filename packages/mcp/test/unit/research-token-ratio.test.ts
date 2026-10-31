// The research agent's one token estimator: serve's learned bytes-per-token ratio counted exactly as
// the platform counts it, and the local characters-per-token estimate as the fallback.
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { runAdapter } from "../../src/research/agent-loop.js";
import { BASH_ADAPTER } from "../../src/research/bash-agent.js";
import { AgentContextManager } from "../../src/research/context-manager.js";
import { AgentRuntimeConfig, ContextPolicy, type Message } from "../../src/research/context-policy.js";
import {
  ContextBudgeter, messagesTokens, platformBytesPerTokenMilli, platformTextTokens,
} from "../../src/research/fit.js";
import { HTTPError, URLError, resolvePlatformTokenRatio } from "../../src/research/transport.js";
import type { HttpOptions } from "../../src/research/transport-http.js";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});
const tempRoot = () => {
  const root = mkdtempSync(path.join(tmpdir(), "freellama-token-ratio-"));
  roots.push(root);
  return root;
};

describe("platform token formula", () => {
  // The numbers rust-core's platform/context.rs tests assert.
  const text = "a".repeat(9000);
  it("unmeasured models use 3 bytes per token", () => expect(platformTextTokens(text, null)).toBe(3000));
  it("a learned ratio keeps a 10% margin", () => expect(platformTextTokens(text, 4000)).toBe(2500));
  it("a learned ratio is clamped to 8 bytes per token", () => expect(platformTextTokens(text, 100_000)).toBe(1125));
  it("a learned ratio is clamped to 1 byte per token", () => expect(platformBytesPerTokenMilli(500)).toBe(1000));
  it("counts UTF-8 bytes and rounds up", () => {
    // "é" is two bytes: 3 × 2 × 1000 / 3600 = 1.67 → 2.
    expect(platformTextTokens("ééé", 4000)).toBe(2);
  });
  it("counts each serialized message plus one, images at their fixed charge", () => {
    const policy = new ContextPolicy({ image_token_estimate: 77 });
    const message: Message = { role: "user", content: "x".repeat(100) };
    const expected = Math.ceil(Buffer.byteLength(JSON.stringify(message)) * 1000 / 3600) + 1;
    expect(messagesTokens([message], policy, 4000)).toBe(expected);
    expect(messagesTokens([{ ...message, images: ["bytes"] }], policy, 4000)).toBe(expected + 77);
  });
});

describe("one estimator per run", () => {
  const messages: Message[] = [{ role: "user", content: "calibrate ".repeat(100) }];
  const ratio = { bytes_per_token_milli: 4000, bytes_per_token_samples: 12 };

  it("the platform ratio is never rescaled by Ollama counts", () => {
    const budgeter = new ContextBudgeter(new ContextPolicy(), { platformRatio: ratio, initialScale: 5 });
    const before = budgeter.estimate(messages);
    expect(before).toBe(messagesTokens(messages, new ContextPolicy(), 4000));
    expect(budgeter.observe(messages, before * 10)).toBe(false);
    expect(budgeter.estimate(messages)).toBe(before);
    expect(budgeter.metadata()).toMatchObject({
      token_counting: "platform_learned_ratio", estimate_scale: 1, calibration_samples: 0,
      bytes_per_token_milli: 4000, bytes_per_token_milli_with_margin: 3600, bytes_per_token_samples: 12,
    });
  });

  it("without a ratio the local estimate calibrates as before", () => {
    const budgeter = new ContextBudgeter();
    const before = budgeter.estimate(messages);
    expect(budgeter.observe(messages, before * 2)).toBe(true);
    expect(budgeter.estimate(messages)).toBe(before * 2);
    expect(budgeter.metadata()).toMatchObject({ token_counting: "local_chars_per_token", calibration_samples: 1, bytes_per_token_milli: null });
  });

  it("the platform ratio neither applies nor writes persisted calibration", () => {
    const runtime = AgentRuntimeConfig.fromEnv({ defaultToolTimeoutSeconds: 30, env: {} });
    const calibrationDir = path.join(tempRoot(), "calibration");
    const local = new AgentContextManager(runtime, { model: "m", calibrationDir });
    local.observe(messages, local.budgeter.estimate(messages) * 3);
    expect(existsSync(calibrationDir)).toBe(true);
    const platform = new AgentContextManager(runtime, { model: "m", calibrationDir, platformRatio: ratio });
    expect(platform.budgeter.scale).toBe(1);
    expect(platform.metadata().calibration_source).toBe("not_applied");
    rmSync(calibrationDir, { recursive: true, force: true });
    platform.observe(messages, 1_000_000);
    expect(existsSync(calibrationDir)).toBe(false);
  });
});

describe("reading serve's ratio", () => {
  const catalog = (entry: Record<string, unknown>) => ({ models: [{ name: "other:1b", bytes_per_token_milli: 9999 }, entry] });

  it("reads the chosen model's learned ratio from the managed catalog", async () => {
    const seen: string[] = [];
    const ratio = await resolvePlatformTokenRatio("http://serve/", "m", {
      readJson: (url) => {
        seen.push(url);
        return catalog({ name: "m:latest", bytes_per_token_milli: 3712, bytes_per_token_samples: 4 });
      },
    });
    expect(seen).toEqual(["http://serve/_freellama/v1/models"]);
    expect(ratio).toEqual({ bytes_per_token_milli: 3712, bytes_per_token_samples: 4 });
  });

  it("unmeasured, absent, older or unreachable serve gives no ratio", async () => {
    const payloads = [
      catalog({ name: "m", bytes_per_token_milli: null, bytes_per_token_samples: 0 }),
      catalog({ name: "m" }),
      { models: [] },
      { error: "not found" },
    ];
    for (const payload of payloads) expect(await resolvePlatformTokenRatio("http://serve", "m", { readJson: () => payload })).toBeNull();
    for (const failure of [new HTTPError(404, "Not Found"), new URLError(new Error("ECONNREFUSED")), new SyntaxError("bad json")]) {
      expect(await resolvePlatformTokenRatio("http://serve", "m", { readJson: () => { throw failure; } })).toBeNull();
    }
  });
});

describe("research run token counting", () => {
  const SERVE = "http://serve.test";
  const digest = "a".repeat(64);

  async function managedRun(catalogReply: unknown) {
    const root = tempRoot();
    const prompt = path.join(root, "prompt.txt");
    writeFileSync(prompt, "Find one fact.", "utf8");
    const result = path.join(root, "result.json");
    const calibrationDir = path.join(root, "calibration");
    const gets: string[] = [];
    const httpRequest = async (url: string, options: HttpOptions): Promise<Buffer> => {
      gets.push(`${options.method} ${url}`);
      if (catalogReply instanceof Error) throw catalogReply;
      // Calibration identity of a managed run is read from serve's Ollama-compatible tag list.
      if (url.endsWith("/api/tags")) return Buffer.from(JSON.stringify({ models: [{ name: "m:latest", digest }] }), "utf8");
      return Buffer.from(JSON.stringify(catalogReply), "utf8");
    };
    const response = { message: { content: '{"action":"finish","answer":"done"}' }, prompt_eval_count: 6000 };
    const env = {
      FREELLAMA_TARGET_MODEL: "m:latest", FREELLAMA_BENCH_WORKSPACE: root, FREELLAMA_BENCH_PROMPT: prompt,
      FREELLAMA_AGENT_RESULT: result, FREELLAMA_AGENT_MANAGED_ENDPOINT: SERVE, FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR: calibrationDir,
    };
    const code = await runAdapter(BASH_ADAPTER, { env, httpRequest, requestJson: async () => response, print: () => undefined });
    expect(code).toBe(0);
    const decoded = JSON.parse(readFileSync(result, "utf8"));
    return { gets, calibrationWritten: existsSync(calibrationDir), metadata: decoded.model_metadata };
  }

  it("uses serve's ratio, read once per run, and skips local calibration", async () => {
    const run = await managedRun({ models: [{ name: "m:latest", digest, bytes_per_token_milli: 4000, bytes_per_token_samples: 7 }] });
    expect(run.gets.filter((get) => get.endsWith("/_freellama/v1/models"))).toEqual([`GET ${SERVE}/_freellama/v1/models`]);
    expect(run.metadata.calibration_model_identity.verified).toBe(true);
    expect(run.metadata.context_management).toMatchObject({
      token_counting: "platform_learned_ratio", bytes_per_token_milli: 4000, bytes_per_token_milli_with_margin: 3600,
      bytes_per_token_samples: 7, calibration_samples: 0, estimate_scale: 1,
    });
    expect(run.calibrationWritten).toBe(false);
  });

  it("falls back to the calibrated local estimate while the ratio is unmeasured", async () => {
    const run = await managedRun({ models: [{ name: "m:latest", digest, bytes_per_token_milli: null, bytes_per_token_samples: 0 }] });
    expect(run.metadata.context_management).toMatchObject({ token_counting: "local_chars_per_token", bytes_per_token_milli: null, calibration_samples: 1 });
    expect(run.calibrationWritten).toBe(true);
  });

  it("falls back when serve is unreachable", async () => {
    const run = await managedRun(new URLError(new Error("ECONNREFUSED")));
    expect(run.metadata.context_management).toMatchObject({ token_counting: "local_chars_per_token", calibration_samples: 1 });
  });
});
