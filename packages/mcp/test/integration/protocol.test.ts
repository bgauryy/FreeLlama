// Protocol-completeness checks: every tool must advertise machine-readable behaviour hints, and
// every non-error result must actually carry structured content. These are the parts a client
// reads to decide whether to prompt a human and how to parse a result — prose in a description
// can't be acted on programmatically.
//
// Runs against a live Ollama but needs no `freellama serve`: whichever half of the contract is
// checkable is asserted (probed once at collection time via top-level await).
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient, serveAuthHeaders, serveIsUp, SERVE_ENDPOINT } from "../setup/client.js";

type Tool = { name: string; description?: string; annotations?: any; outputSchema?: unknown; inputSchema?: any; title?: string };
type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

const EXPECTED_TOOLS = ["doctor", "models", "run_task", "ollama_manage", "ollama_delete", "delegate_research"];

// Whether `freellama serve` is up decides which half of the contract is checkable.
const serveUp = await serveIsUp();

describe("tool contract", () => {
  let client: Client;
  let tools: Tool[];
  let byName: Map<string, Tool>;

  beforeAll(async () => {
    client = await connectClient();
    tools = (await client.listTools()).tools as Tool[];
    byName = new Map(tools.map((tool) => [tool.name, tool]));
  });

  afterAll(async () => {
    await client.close();
  });

  const call = (name: string, args: Record<string, unknown> = {}, timeout = 60_000) =>
    client.callTool({ name, arguments: args }, undefined, { timeout }) as Promise<ToolResult>;

  it("advertises exactly the expected tool set", () => {
    expect([...byName.keys()].sort()).toEqual([...EXPECTED_TOOLS].sort());
  });

  it("teaches clients execution ownership and the model-installation approval gate", () => {
    const instructions = client.getInstructions() ?? "";
    expect(instructions).toMatch(/caller owns task decomposition/);
    expect(instructions).toMatch(/operator owns.*endpoints, exact --cpu-model assignments/s);
    expect(instructions).toMatch(/Ollama plus the.*OS\/driver.*physical CPU\/GPU/s);
    expect(instructions).toMatch(/models\{view:"installed"\}.*models\{view:"resident"\}/s);
    expect(instructions).toMatch(/ask approval.*for one exact tag and reported size before ollama_manage/s);
    expect(instructions).toMatch(/search or recommendation\s+is never download permission/i);
  });

  it("exposes a bounded placement preference instead of an unsafe backend override", () => {
    const schema = byName.get("run_task")!.inputSchema;
    expect(schema.properties.task.enum).toEqual([
      "completion",
      "coding",
      "code_repair",
      "tools",
      "browser",
      "vision",
      "embedding",
      "long_context",
    ]);
    expect(schema.properties.requiredCapabilities.items.enum).toEqual([
      "completion",
      "tools",
      "vision",
      "audio",
      "thinking",
      "embedding",
    ]);
    expect(schema.properties.executionPreference.enum).toEqual(["auto", "prefer_cpu", "prefer_gpu"]);
    expect(schema.properties.minPlacementEvidence.enum).toEqual(["configured", "observed"]);
    expect(schema.properties).not.toHaveProperty("upstream");
    expect(schema.properties).not.toHaveProperty("numGpu");
  });

  it("rejects view- and action-specific fields before silently ignoring them", async () => {
    const wrongModelField = await call("models", { view: "installed", query: "ignored" });
    expect(wrongModelField.isError).toBe(true);
    expect(wrongModelField.content[0].text).toMatch(/does not accept library search fields/);

    const mixedLibrarySteps = await call("models", {
      view: "library",
      model: "qwen3-vl",
      query: "ignored",
    });
    expect(mixedLibrarySteps.isError).toBe(true);
    expect(mixedLibrarySteps.content[0].text).toMatch(/step 2 accepts only/);

    const stopTimeout = await call("ollama_manage", {
      action: "stop",
      model: "not-loaded:latest",
      timeoutSeconds: 1,
    });
    expect(stopTimeout.isError).toBe(true);
    expect(stopTimeout.content[0].text).toMatch(/valid only for action "pull"/);
  });

  it("rejects incompatible run_task payloads before calling serve", async () => {
    const missingInput = await call("run_task", { task: "embedding", objective: "fastest" });
    expect(missingInput.isError).toBe(true);
    expect(missingInput.content[0].text).toMatch(/requires `input`/);

    const wrongPayload = await call("run_task", {
      task: "completion",
      objective: "fastest",
      input: "ignored",
    });
    expect(wrongPayload.isError).toBe(true);
    expect(wrongPayload.content[0].text).toMatch(/valid only for task "embedding"/);

    const invalidLogprobs = await call("run_task", {
      task: "completion",
      preview: true,
      topLogprobs: 2,
    });
    expect(invalidLogprobs.isError).toBe(true);
    expect(invalidLogprobs.content[0].text).toMatch(/requires `logprobs:true`/);

    const implicitVisionModel = await call("run_task", {
      task: "vision",
      prompt: "describe",
      images: ["aW1hZ2U="],
    });
    expect(implicitVisionModel.isError).toBe(true);
    expect(implicitVisionModel.content[0].text).toMatch(/explicit tested vision-capable `model`/);
  });

  it("exposes lossless agent history and advanced non-routing Ollama controls", () => {
    const schema = byName.get("run_task")!.inputSchema;
    for (const field of ["format", "think", "options", "logprobs", "topLogprobs"]) {
      expect(schema.properties, `run_task is missing ${field}`).toHaveProperty(field);
    }
    expect(schema.properties.messages.items.additionalProperties).toBe(true);
    expect(schema.properties.options.description).toMatch(/num_ctx.*num_gpu/);
  });

  it("annotations declare only deviations from spec defaults", () => {
    // Spec defaults: readOnlyHint=false, destructiveHint=true, idempotentHint=false,
    // openWorldHint=true. Restating a default costs bytes and says nothing; omitting a
    // deviation loses real signal.
    for (const tool of tools) {
      expect(tool.annotations, `${tool.name}: no annotations`).toBeTruthy();
      expect(tool.annotations.openWorldHint, `${tool.name}: openWorldHint restates the default`).toBeUndefined();
      expect(tool.title, `${tool.name}: title duplicates the name`).toBeUndefined();
      if (tool.annotations.readOnlyHint === true) {
        expect(tool.annotations.destructiveHint, `${tool.name}: meaningless on a read-only tool`).toBeUndefined();
        expect(tool.annotations.idempotentHint, `${tool.name}: meaningless on a read-only tool`).toBeUndefined();
      }
    }
  });

  it("marks ollama_delete — and only ollama_delete — machine-readably destructive", () => {
    expect(byName.get("ollama_delete")!.annotations.destructiveHint).toBe(true);
    expect(tools.filter((tool) => tool.annotations?.destructiveHint === true).map((tool) => tool.name)).toEqual([
      "ollama_delete",
    ]);
    // Belt and braces: the prose warning must survive too.
    expect(byName.get("ollama_delete")!.description).toMatch(/DESTRUCTIVE AND IRREVERSIBLE/);
  });

  it("advertises no output schemas (removed on purpose — they cost every request)", () => {
    for (const tool of tools) {
      expect(tool.outputSchema, `${tool.name}: outputSchema is back`).toBeUndefined();
    }
  });

  it("returns structuredContent whose text half is the same object (doctor)", async () => {
    const result = await call("doctor");
    expect(result.isError ?? false).toBe(false);
    expect(result.structuredContent).toBeTruthy();
    expect(typeof result.structuredContent.endpoint).toBe("string");
    if (serveUp) {
      // doctor absorbed `machine`; with serve up it must carry a real profile, not the null branch.
      expect(result.structuredContent.machine?.memory_bytes).toBeTruthy();
    }
    expect(JSON.parse(result.content[0].text)).toEqual(result.structuredContent);
  });

  it.runIf(!serveUp)("serve down: doctor keeps local host evidence, run_task errors cleanly", async () => {
    const doctor = await call("doctor");
    expect(doctor.isError ?? false).toBe(false);
    expect(doctor.structuredContent.machine?.memory_bytes).toBeGreaterThan(0);
    expect(doctor.structuredContent.machine_unavailable).toBeUndefined();

    const route = await call("run_task", { task: "completion", preview: true });
    expect(route.isError).toBe(true);
    // Error results must not carry structuredContent.
    expect(route.structuredContent).toBeUndefined();
  });

  it.runIf(serveUp)("serve up: every serve-backed result keeps both halves in agreement", async () => {
    const health = await fetch(`${SERVE_ENDPOINT}/_freellama/v1/health`, {
      headers: serveAuthHeaders(),
    }).then((r) => r.json());
    // Stale serve builds can grade hardware fit wrong or omit explicit backend assignment;
    // rebuild and restart rather than weakening either contract.
    expect(health?.contracts?.hardware_fit).toBe("sent_num_ctx");
    expect(health?.contracts?.machine_profile).toBe("portable_host_memory_v2");
    expect(health?.contracts?.model_backends).toBe("explicit_cpu_assignment");
    expect(health?.contracts?.placement_observation).toBe("ollama_api_ps_after_execution");
    expect(health?.contracts?.placement_evidence_gate).toBe("configured_or_observed");
    expect(health?.contracts?.placement_feedback_metric).toBe("normalized_work_unit_10_percent");
    expect(health?.backends?.gpu?.upstream).toBeTruthy();

    const liveCalls: [string, Record<string, unknown>][] = [
      ["models", {}],
      ["models", { view: "raw" }],
      ["models", { view: "resident" }],
      ["run_task", { task: "completion", objective: "fastest", preview: true }],
    ];
    for (const [name, args] of liveCalls) {
      const result = await call(name, args);
      expect(result.isError ?? false, `${name} ${JSON.stringify(args)}: ${result.content?.[0]?.text}`).toBe(false);
      expect(result.structuredContent).toBeTruthy();
      expect(JSON.parse(result.content[0].text)).toEqual(result.structuredContent);
      if (name === "run_task") {
        expect(result.structuredContent.execution?.placement).toMatch(/^(cpu|gpu)$/);
        expect(result.structuredContent.execution?.preference).toBe("auto");
        expect(typeof result.structuredContent.execution?.reason).toBe("string");
      }
    }
  });

  it.runIf(serveUp)("withholds embedding vectors by default and returns them on opt-in", async () => {
    const embed = await call("run_task", {
      task: "embedding",
      objective: "fastest",
      model: "nomic-embed-text:latest",
      input: "protocol smoke test",
      keepAlive: "0",
    });
    if (embed.isError) {
      // nomic-embed-text not installed on this machine — the e2e tier covers execution.
      console.warn(`embedding check skipped: ${embed.content[0].text.slice(0, 80)}`);
      return;
    }
    const withheld = embed.structuredContent.response.embeddings_omitted;
    expect(withheld).toBeTruthy();
    expect(embed.structuredContent.response.embeddings).toBeUndefined();
    expect(withheld.count).toBe(1);
    expect(typeof withheld.dimensions).toBe("number");
    expect(embed.structuredContent.route?.hardware_fit).toBe("strong");

    const full = await call("run_task", {
      task: "embedding",
      objective: "fastest",
      model: "nomic-embed-text:latest",
      input: "protocol smoke test",
      keepAlive: "0",
      returnEmbeddings: true,
    });
    expect(full.isError ?? false).toBe(false);
    expect(full.structuredContent.response.embeddings).toHaveLength(1);
    expect(full.content[0].text.length).toBeGreaterThan(embed.content[0].text.length);
  });

  it("models{view:detail} withholds license/modelfile blobs unless includeVerbose", async () => {
    const tags = await call("models", { view: "raw" });
    const someModel = tags.structuredContent?.models?.[0]?.name;
    if (!someModel) return console.warn("skipped: no models installed");

    const lean = await call("models", { view: "detail", model: someModel });
    expect(lean.isError ?? false, lean.content?.[0]?.text).toBe(false);
    expect(lean.structuredContent.license).toBeUndefined();
    expect(lean.structuredContent.modelfile).toBeUndefined();
    expect(Array.isArray(lean.structuredContent.capabilities)).toBe(true);

    const verbose = await call("models", { view: "detail", model: someModel, includeVerbose: true });
    expect(verbose.isError ?? false).toBe(false);
    expect(verbose.content[0].text.length).toBeGreaterThan(lean.content[0].text.length);
  });

  it("models{view:resident} derives placement; detail without model errors with guidance", async () => {
    const resident = await call("models", { view: "resident" });
    expect(resident.isError ?? false).toBe(false);
    for (const model of resident.structuredContent.models ?? []) {
      expect(model.placement, `${model.name}: no derived placement`).toBeTruthy();
      expect(model.execution?.placement, `${model.name}: no managed backend receipt`).toMatch(/^(cpu|gpu)$/);
      if (model.execution?.placement === "cpu") {
        expect(model.placement.assigned).toBe(true);
        expect(model.placement.processor).toMatch(/^100% (CPU|GPU)$/);
        if (model.execution?.observation?.status === "mismatch") {
          expect(model.placement.warning).toMatch(/disagrees with Ollama \/api\/ps/);
        }
      }
    }
    const noModel = await call("models", { view: "detail" });
    expect(noModel.isError).toBe(true);
    expect(noModel.content[0].text).toMatch(/needs `model`/);
  });

  it("doctor reports the memory-governing env vars with effective defaults", async () => {
    const doctor = await call("doctor");
    const envConfig = doctor.structuredContent.ollama_env_config;
    for (const key of [
      "OLLAMA_MAX_LOADED_MODELS",
      "OLLAMA_CONTEXT_LENGTH",
      "OLLAMA_KV_CACHE_TYPE",
      "OLLAMA_NUM_PARALLEL",
      "LLAMA_ARG_FIT",
      "LLAMA_ARG_FIT_TARGET",
    ]) {
      expect(envConfig[key], `doctor does not report ${key}`).toBeTruthy();
      expect(envConfig[key].effective_default, `${key} reported without an effective_default`).toBeTruthy();
    }
    // MAX_LOADED_MODELS resolves to 3 x GPU count, not "unlimited".
    expect(JSON.stringify(doctor.structuredContent)).not.toMatch(/unlimited/);
  });

  it.runIf(serveUp)("minConfidence fails closed, before generating", async () => {
    const open = await call("run_task", { task: "completion", objective: "fastest", preview: true });
    expect(open.isError ?? false).toBe(false);
    // The server grades a no-policy/no-benchmark pick "low" (route_evidence in rust-core). If
    // that ever becomes "medium" this assertion should be revisited, not deleted.
    expect(open.structuredContent.confidence).toBe("low");

    const gated = await call("run_task", {
      task: "completion",
      objective: "fastest",
      minConfidence: "medium",
      preview: true,
    });
    expect(gated.isError).toBe(true);
    expect(gated.content[0].text).toMatch(/fail-closed refusal/);
    // The refusal must name the rejected model and the missing evidence.
    expect(gated.content[0].text).toContain(open.structuredContent.selected_model);
    expect(gated.content[0].text).toMatch(/capability_metadata_only/);

    // The gate has to fire BEFORE generation, or it saves nothing.
    const started = Date.now();
    const blocked = await call("run_task", {
      task: "completion",
      objective: "fastest",
      prompt: "hi",
      minConfidence: "medium",
    });
    expect(blocked.isError).toBe(true);
    expect(Date.now() - started).toBeLessThan(5000);
  });

  it("delegate_research offers exactly the two adapters", () => {
    const adapterTool = byName.get("delegate_research")!;
    expect([...adapterTool.inputSchema.properties.adapter.enum].sort()).toEqual(["bash", "octocode"]);
  });

  it("delegate_research exposes typed runtime and fail-closed context policy", () => {
    const delegate = byName.get("delegate_research")!;
    expect(delegate.inputSchema.properties.minPlacementEvidence.enum).toEqual(["configured", "observed"]);
    expect(delegate.inputSchema.properties.endpoint).toBeTruthy();
    expect(delegate.inputSchema.properties.ollamaEndpoint).toBeUndefined();
    const agent = delegate.inputSchema.properties.agent;
    const properties = agent.anyOf?.[0]?.properties ?? agent.properties;
    expect(properties.contextTokens.type).toBe("integer");
    expect(properties.retryAttempts.exclusiveMinimum).toBe(0);
    const context = properties.context.anyOf?.[0] ?? properties.context;
    expect(context.properties.pinnedOverflow.enum).toEqual(["error", "clip"]);
    expect(context.properties.compactRetainRatio.exclusiveMaximum).toBe(1);
  });

  it("keeps the schema surface within the token budget", () => {
    // Paid on EVERY request, so it is a real running cost. Measured 7,431 tokens across 13 tools
    // before the read-only merge and the de-duplication of RouteDecision.
    const surfaceTokens = Math.round(
      (JSON.stringify(tools).length + (client.getInstructions() ?? "").length) / 4,
    );
    expect(surfaceTokens, `schema surface grew to ~${surfaceTokens} tokens`).toBeLessThan(3200);
  });

  it("models{view:library} defaults to popular, flags cloud, cross-references installed", async () => {
    const search = await call("models", { view: "library", capabilities: ["vision"], limit: 6 }).catch(() => null);
    if (!search || search.isError) return console.warn("skipped: ollama.com unreachable");

    const data = search.structuredContent;
    expect(data.order).toBe("popular");
    expect(data.query).not.toMatch(/o=newest/);
    expect(data.models.length).toBeGreaterThan(0);
    for (const model of data.models) {
      expect(typeof model.name).toBe("string");
      expect(typeof model.cloudOnly).toBe("boolean");
      expect(typeof model.installed).toBe("boolean");
    }
    expect(data.nextStep).toMatch(/model:/);

    // Step 2 is what makes the result actionable: only a tag is pullable, and only the tag
    // carries the size that decides whether it fits.
    const detail = await call("models", { view: "library", model: "qwen3-vl" }).catch(() => null);
    if (!detail || detail.isError) return console.warn("step 2 skipped: ollama.com unreachable");
    const tags = detail.structuredContent;
    expect(tags.tags.length).toBeGreaterThan(0);
    expect(tags.tags.every((entry: any) => entry.tag.includes(":"))).toBe(true);
    expect(tags.tags.every((entry: any) => entry.fitScope === "host_memory_budget_only")).toBe(true);
    const huge = tags.tags.find((entry: any) => (entry.sizeBytes ?? 0) > 100e9);
    if (huge && tags.fitBudgetBytes) expect(huge.fitsInMemory).toBe(false);
    if (!tags.fitBudgetBytes) {
      // Fail CLOSED: with no machine profile the fit is unknowable — no budget, no recommendation.
      expect(tags.recommendation).toBeNull();
      expect(tags.recommendationUnavailable ?? "").toMatch(/could not be checked/);
    } else {
      expect(tags.recommendation.tag).toContain(":");
      expect(tags.tags.find((entry: any) => entry.tag === tags.recommendation.tag)?.fitsInMemory).toBe(true);
    }

    const noLocalState = await call("models", {
      view: "library",
      model: "qwen3-vl",
      endpoint: "http://127.0.0.1:1",
      ollamaEndpoint: "http://127.0.0.1:1",
    }).catch(() => null);
    if (!noLocalState || noLocalState.isError) return console.warn("endpoint override check skipped");
    expect(noLocalState.structuredContent.machineMemoryBytes).toBeNull();
    expect(noLocalState.structuredContent.fitBudgetBytes).toBeNull();
    expect(noLocalState.structuredContent.recommendation).toBeNull();
  });
});
