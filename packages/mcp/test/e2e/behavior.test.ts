// Every tool exercised against the live system — the behavior suite (formerly validate-all.mjs).
// Not a schema check: asserts what each tool *does*, including a real delegated research run on
// the default ~27B model. Needs serve + Ollama + installed models; the slowest, truest tier.
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  connectClient,
  type IsolatedServe,
  releaseServeAvailable,
  REPO_ROOT,
  startIsolatedServe,
} from "../setup/client.js";

type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

describe.runIf(releaseServeAvailable)("behavior: every tool against the live system", () => {
  let client: Client;
  let isolated: IsolatedServe | null = null;

  beforeAll(async () => {
    isolated = await startIsolatedServe();
    client = await connectClient({ FREELLAMA_SERVE_ENDPOINT: isolated.endpoint });
  });

  afterAll(async () => {
    await client?.close();
    isolated?.child.kill();
  });

  const call = (name: string, args: Record<string, unknown> = {}, timeout = 180_000) =>
    client.callTool({ name, arguments: args }, undefined, { timeout }) as Promise<ToolResult>;

  it("doctor: Ollama settings have effective defaults and include memory controls", async () => {
    const doctor = (await call("doctor", { view: "full" })).structuredContent;
    const categorized = doctor.ollama_config;
    const envConfig = categorized.categories.memory_scheduler;
    expect(doctor.ollama_env_config).toBeUndefined();
    expect(categorized.categories.network_security.OLLAMA_HOST).toBeTruthy();
    expect(categorized.categories.privacy.OLLAMA_NO_CLOUD).toBeTruthy();
    expect(categorized.categories.backend_device.OLLAMA_LLM_LIBRARY).toBeTruthy();
    expect(categorized.categories.storage_lifecycle.OLLAMA_MODELS).toBeTruthy();
    const allSettings = Object.values(categorized.categories).flatMap((category) =>
      Object.entries(category as Record<string, any>),
    ) as [string, any][];
    for (const key of [
      "OLLAMA_MAX_LOADED_MODELS",
      "OLLAMA_NUM_PARALLEL",
      "OLLAMA_CONTEXT_LENGTH",
      "OLLAMA_KV_CACHE_TYPE",
      "LLAMA_ARG_FIT",
      "LLAMA_ARG_FIT_TARGET",
    ]) {
      expect(envConfig[key], `doctor does not report ${key}`).toBeTruthy();
    }
    for (const [key, value] of allSettings) {
      expect(typeof value.effective_default, `${key} lacks an effective_default`).toBe("string");
      expect(value.effective_default).toBeTruthy();
    }
    expect(doctor.machine?.memory_bytes).toBeTruthy();
    const loadedModelCap = envConfig.OLLAMA_MAX_LOADED_MODELS;
    if (loadedModelCap.value === null) {
      expect(doctor.ollama_env_config_warning ?? "").toMatch(/3 x GPU count/);
    } else {
      expect(Number(loadedModelCap.value)).toBeGreaterThan(0);
      expect(doctor.ollama_env_config_warning).toBeNull();
    }
  });

  it("models: installed/resident/raw views succeed; detail withholds blobs and needs a model", async () => {
    let raw: ToolResult | undefined;
    for (const view of ["installed", "resident", "raw"]) {
      const result = await call("models", { view });
      expect(result.isError ?? false, `models[${view}]: ${result.content?.[0]?.text}`).toBe(false);
      expect(result.structuredContent).toBeTruthy();
      if (view === "raw") raw = result;
    }
    const installedModel = raw?.structuredContent?.models?.[0]?.name;
    expect(installedModel, "the live E2E tier requires at least one installed model").toBeTruthy();
    const detailResult = await call("models", { view: "detail", model: installedModel });
    expect(detailResult.isError ?? false, detailResult.content?.[0]?.text).toBe(false);
    const detail = detailResult.structuredContent;
    expect(detail.license).toBeUndefined();
    expect(detail.modelfile).toBeUndefined();
    expect(typeof detail.max_context_length).toBe("number");
    expect((await call("models", { view: "detail" })).isError).toBe(true);
  });

  it("run_task: grades a policy-less route low and refuses before generating", async () => {
    const preview = (await call("run_task", { task: "completion", objective: "fastest", preview: true }))
      .structuredContent;
    expect(preview.confidence).toBe("low");

    const gatedPreview = await call("run_task", {
      task: "completion", objective: "fastest", minConfidence: "medium", preview: true,
    });
    expect(gatedPreview.isError).toBe(true);

    const started = Date.now();
    const blocked = await call("run_task", {
      task: "completion", objective: "fastest", prompt: "hi", minConfidence: "medium",
    });
    expect(blocked.isError).toBe(true);
    expect(Date.now() - started).toBeLessThan(5000);
  });

  it("run_task: withholds embedding vectors by default, returns them on opt-in", async ({ skip }) => {
    const raw = await call("models", { view: "raw" });
    const hasEmbeddingModel = raw.structuredContent?.models?.some(
      (model: { name?: string }) => model.name === "nomic-embed-text:latest",
    );
    if (!hasEmbeddingModel) skip("nomic-embed-text:latest is not installed");

    const leanResult = await call("run_task", {
      task: "embedding", model: "nomic-embed-text:latest", input: "x", keepAlive: "0",
    });
    expect(leanResult.isError ?? false, leanResult.content?.[0]?.text).toBe(false);
    const lean = leanResult.structuredContent;
    expect(lean.response.embeddings_omitted).toBeTruthy();
    expect(lean.response.embeddings).toBeUndefined();

    const fullResult = await call("run_task", {
      task: "embedding", model: "nomic-embed-text:latest", input: "x", keepAlive: "0", returnEmbeddings: true,
    });
    expect(fullResult.isError ?? false, fullResult.content?.[0]?.text).toBe(false);
    const full = fullResult.structuredContent;
    expect(Array.isArray(full.response.embeddings)).toBe(true);
  });

  it("models[library]: popular by default, recommends only what fits", async () => {
    const search = (await call("models", { view: "library", capabilities: ["vision"], limit: 5 }, 60_000))
      .structuredContent;
    expect(search.order).toBe("popular");
    expect(search.query).not.toMatch(/o=newest/);
    expect(search.nextStep).toMatch(/model:/);

    const tags = (await call("models", { view: "library", model: "qwen3-vl" }, 60_000)).structuredContent;
    expect(tags.recommendation).toBeTruthy();
    expect(tags.tags.every((entry: any) => entry.fitScope === "host_memory_budget_only")).toBe(true);
    expect(tags.tags.find((entry: any) => entry.tag === tags.recommendation.tag)?.fitsInMemory).toBe(true);
    // A 235B model must not be reported as fitting this machine.
    expect(tags.tags.some((entry: any) => (entry.sizeBytes ?? 0) > 100e9 && entry.fitsInMemory === false)).toBe(true);
  });

  it("delegate_research: pre-flight refuses an unusable model with zero tool calls", async () => {
    const escalated = (await call("delegate_research", {
      question: "unanswerable from files", workspacePath: REPO_ROOT, model: "qwen2.5:0.5b",
    })).structuredContent;
    expect(escalated.verification.recommendation).toBe("escalate");
    expect(escalated.toolCallCount).toBe(0);
    // Model evidence is loaded from disk, not compiled in.
    expect(escalated.verification.measuredBaseRate).not.toContain("undefined");
  });

  it("delegate_research: grounds a real lookup and accepts it (bash adapter default)", async ({ skip }) => {
    const raw = await call("models", { view: "raw" });
    const hasDefaultModel = raw.structuredContent?.models?.some(
      (model: { name?: string }) => model.name === "qwen3.8:27b-mlx",
    );
    if (!hasDefaultModel) skip("qwen3.8:27b-mlx is not installed");

    const groundedResult = await call("delegate_research", {
      question: "In packages/rust-core/Cargo.toml, what optional feature enables the Node addon?",
      workspacePath: REPO_ROOT,
      agent: {
        contextTokens: 8192,
        context: { charsPerToken: 3.5, observationPageChars: 2048, pinnedOverflow: "error" },
      },
    }, 300_000);
    expect(groundedResult.isError ?? false, groundedResult.content?.[0]?.text).toBe(false);
    const grounded = groundedResult.structuredContent;
    expect(grounded.answer).toMatch(/napi/i);
    expect(grounded.verification.recommendation).toBe("accept");
    expect(grounded.adapter).toBe("bash");
    expect(grounded.contextManagement.chars_per_token).toBe(3.5);
    expect(grounded.contextManagement.observation_page_chars).toBe(2048);
    expect(grounded.contextManagement.pinned_overflow).toBe("error");
    expect(grounded.contextManagement.calibration_samples).toBeGreaterThan(0);
  });
});
