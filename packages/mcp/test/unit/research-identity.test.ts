// Model identity contracts and frozen byte-identity vectors for the research adapters.
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { AgentContextManager } from "../../src/research/context-manager.js";
import { AgentRuntimeConfig } from "../../src/research/context-policy.js";
import { BASH_SYSTEM_PROMPT, octocodeSystemPrompt } from "../../src/research/prompts.js";
import { RequestTimeoutError, resolveModelIdentity } from "../../src/research/transport.js";

const resolve = (endpoint: string, model: string, prompt: string, readJson: (url: string) => unknown) =>
  resolveModelIdentity(endpoint, model, prompt, { readJson: async (url) => readJson(url) });

describe("model identity", () => {
  it("digest and prompt changes invalidate identity", async () => {
    const identity = (digest: string, prompt = "system") => resolve("http://local", "m:latest", prompt, () => ({ models: [{ name: "m:latest", digest }] }));
    const first = await identity("a".repeat(64));
    expect(first.verified).toBe(true);
    expect(first.identity).toBe((await identity("a".repeat(64))).identity);
    expect(first.identity).not.toBe((await identity("b".repeat(64))).identity);
    expect(first.identity).not.toBe((await identity("a".repeat(64), "new system")).identity);
  });

  it("managed lookup uses the operator-assigned catalog and a missing digest is unknown", async () => {
    const seen: string[] = [];
    const result = await resolve("http://local/_freellama/v1/tasks", "m", "system", (url) => {
      seen.push(url);
      return { models: [{ name: "m:latest" }] };
    });
    expect(seen).toEqual(["http://local/_freellama/v1/models"]);
    expect(result.verified).toBe(false);
    expect(result.identity).toBe("");
  });

  it("unavailable or malformed identity never enables persistence", async () => {
    for (const payload of [[], { models: null }, { models: [{ name: "m", digest: "wrong" }] }]) {
      expect((await resolve("http://local", "m", "secret system", () => payload)).verified).toBe(false);
    }
    const result = await resolve("http://local", "m", "secret system", () => {
      throw new RequestTimeoutError("unavailable");
    });
    expect(result.verified).toBe(false);
    expect(JSON.stringify(result)).not.toContain("secret");
  });
});

// Vectors computed by the reference implementation (`json.dumps(..., sort_keys=True)` with the
// default ", " and ": " separators, then sha256) and frozen before it was removed.
interface Vectors {
  identity: Array<{ endpoint: string; model: string; prompt_kind: string; catalog: unknown; url: string; result: Record<string, unknown> }>;
  model_key: Array<{ env: Record<string, string>; model: string; identity: string; contract: string; key: string }>;
  record: string[];
}
const vectors = JSON.parse(readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "fixtures", "research-identity-vectors.json"), "utf8")) as Vectors;

describe("frozen identity vectors", () => {
  const prompts: Record<string, string> = { bash: BASH_SYSTEM_PROMPT, octocode: octocodeSystemPrompt("/w\u00e9"), custom: "sys \u2014 \u{1f999}" };
  it.each(vectors.identity.map((vector, index) => [index, vector] as const))("model identity %i is byte-identical", async (_index, vector) => {
    const urls: string[] = [];
    const result = await resolve(vector.endpoint, vector.model, prompts[vector.prompt_kind]!, (url) => {
      urls.push(url);
      return vector.catalog;
    });
    expect(urls).toEqual([vector.url]);
    expect(result).toEqual(vector.result);
    expect(Object.keys(result)).toEqual(Object.keys(vector.result));
  });

  it.each(vectors.model_key.map((vector, index) => [index, vector] as const))("calibration key %i is byte-identical", (index, vector) => {
    const runtime = AgentRuntimeConfig.fromEnv({ defaultToolTimeoutSeconds: 30, env: vector.env });
    const manager = new AgentContextManager(runtime, { model: vector.model, calibrationIdentity: vector.identity });
    expect(manager.calibrationContractJson()).toBe(vector.contract);
    expect(manager.modelKey()).toBe(vector.key);
    expect(manager.calibrationRecordJson(2.5, 3)).toBe(vectors.record[index]);
  });

  it("system prompts hash to the reference digests", async () => {
    const { createHash } = await import("node:crypto");
    const sha = (text: string) => createHash("sha256").update(text, "utf8").digest("hex");
    expect(sha(BASH_SYSTEM_PROMPT)).toBe("412f3f9f64dd210cb1d2b7cd5d61ede25417934e3ebfe880c337ce7e1f8a0049");
    expect(sha(octocodeSystemPrompt("/w"))).toBe("2b9c25f86aa987af6c50d4e88e4c876ff3a508dacb0036ce0569d0aec02323a7");
  });
});
