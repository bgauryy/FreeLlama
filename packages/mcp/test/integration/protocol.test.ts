// Protocol-completeness checks: every tool must advertise machine-readable behaviour hints, and
// every non-error result must actually carry structured content. These are the parts a client
// reads to decide whether to prompt a human and how to parse a result — prose in a description
// can't be acted on programmatically.
//
// Runs against a live Ollama but needs no `freellama serve`: whichever half of the contract is
// checkable is asserted (probed once at collection time via top-level await).
import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer } from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient, REPO_ROOT, serveAuthHeaders, serveIsUp, SERVE_ENDPOINT } from "../setup/client.js";

type Tool = { name: string; description?: string; annotations?: any; outputSchema?: any; inputSchema?: any; title?: string };
type ToolResult = { isError?: boolean; content: { text: string }[]; structuredContent?: any };

const EXPECTED_TOOLS = ["doctor", "models", "run_task", "task_jobs", "run_task_batch", "session", "ollama_manage", "ollama_delete", "delegate_research", "scope", "warm_model"];

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

  it("gives agents explicit selection, exclusion, and result guidance for every tool", () => {
    for (const tool of tools) {
      expect(tool.description, `${tool.name}: missing Use when`).toMatch(/Use when:/);
      expect(tool.description, `${tool.name}: missing Do not use when`).toMatch(/Do not use when:/);
      expect(tool.description, `${tool.name}: missing Returns`).toMatch(/Returns:/);
    }
  });

  it("teaches clients execution ownership and the model-installation approval gate", () => {
    const instructions = client.getInstructions() ?? "";
    expect(instructions).toMatch(/caller owns task decomposition/);
    expect(instructions).toMatch(/operator owns.*endpoints, exact --cpu-model assignments/s);
    expect(instructions).toMatch(/Ollama plus the.*OS\/driver.*physical CPU\/GPU/s);
    expect(instructions).toMatch(/models\{view:"installed"\}.*models\{view:"resident"\}/s);
    expect(instructions).toMatch(/ask approval.*for one exact tag and reported size before ollama_manage/s);
    expect(instructions).toMatch(/search or recommendation\s+is never download permission/i);
    expect(instructions).toMatch(/run_task preview never executes; code_review aliases coding/);
    expect(instructions).toMatch(/findings are candidates, not accepted defects/);
    expect(instructions).toMatch(/requiredCapabilities:\["tools"\].*omit preview and supply the payload/s);
    expect(instructions).toContain("Docs: freellama://docs/index");
    expect(instructions).toContain("Scoped run_task needs scopeId+scopeRevision");
    const requiredTaskFields = byName.get("run_task")!.inputSchema.required ?? [];
    expect(requiredTaskFields).not.toContain("scopeId");
    expect(requiredTaskFields).not.toContain("scopeRevision");
    expect(instructions).toContain("Check isError first; prefer structuredContent, else content[].text");
    expect(instructions).toContain("page.next_cursor→cursor unchanged");
    expect(instructions).toContain("default-endpoint autostart optional");
    expect(byName.get("delegate_research")!.description).toContain("discard verification.recommendation=escalate");
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
      "code_review",
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
    expect(schema.properties.task.description).toMatch(/caller owns prompts and output format/);
    expect(schema.properties.messages.description).toMatch(/system prompts; no injected task instructions/);
  });

  it("makes batch independence and the dispatch cap machine-readable", () => {
    const schema = byName.get("run_task_batch")!.inputSchema;
    expect(schema.properties.tasks.maxItems).toBe(64);
    expect(schema.properties.maxParallelism.maximum).toBe(64);
    expect(schema.properties.maxParallelism.exclusiveMinimum).toBe(0);
    expect(byName.get("run_task_batch")!.description).toMatch(/independent:true/);
    expect(byName.get("run_task_batch")!.description).toMatch(/maxParallelism caps dispatch/);
  });

  it("types owned output fields and exposes bounded model-list pagination", async () => {
    expect(byName.get("doctor")!.outputSchema?.properties).toMatchObject({ summary: { type: "string" } });
    expect(byName.get("run_task")!.outputSchema?.properties).toMatchObject({
      selected_model: { type: "string" }, context_window_fit: { type: "string" },
    });
    expect(byName.get("delegate_research")!.outputSchema?.properties).toMatchObject({
      answer: { type: "string" }, summary: { type: "string" },
    });
    expect(byName.get("ollama_delete")!.outputSchema?.properties).toMatchObject({ deleted: { type: "string" } });
    expect(byName.get("ollama_manage")!.outputSchema?.properties).toMatchObject({ status: { type: "string" } });
    const raw = await call("models", { view: "raw", limit: 1 });
    expect(raw.isError ?? false, raw.content[0].text).toBe(false);
    expect(raw.structuredContent.page).toMatchObject({ returned: expect.any(Number), total: expect.any(Number) });
    expect((raw.structuredContent.models ?? []).length).toBeLessThanOrEqual(1);
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

    const createWithId = await call("session", { action: "create", sessionId: "00000000-0000-4000-8000-000000000000" });
    expect(createWithId.isError).toBe(true);
    expect(createWithId.content[0].text).toMatch(/only valid for action: delete/);

    const deleteWithoutId = await call("session", { action: "delete" });
    expect(deleteWithoutId.isError).toBe(true);
    expect(deleteWithoutId.content[0].text).toMatch(/requires sessionId/);
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

  it("rejects execution-only fields in preview mode instead of silently ignoring them", async () => {
    const executionFields: Record<string, unknown>[] = [
      { prompt: "do not execute" },
      { messages: [{ role: "user", content: "do not execute" }] },
      { input: "do not embed" },
      { images: ["aW1hZ2U="] },
      { tools: [{ type: "function", function: { name: "noop" } }] },
      { keepAlive: "0" },
      { format: "json" },
      { think: false },
      { options: { temperature: 0 } },
      { logprobs: true },
      { topLogprobs: 2, logprobs: true },
      { returnEmbeddings: true },
    ];

    for (const fields of executionFields) {
      const result = await call("run_task", {
        task: "completion",
        objective: "fastest",
        preview: true,
        ...fields,
      });
      expect(result.isError, JSON.stringify(fields)).toBe(true);
      expect(result.content[0].text).toMatch(/preview:true.*routing fields only/i);
    }
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

  it("marks model deletion, session termination, and scope history deletion machine-readably destructive", () => {
    expect(byName.get("ollama_delete")!.annotations.destructiveHint).toBe(true);
    expect(tools.filter((tool) => tool.annotations?.destructiveHint === true).map((tool) => tool.name).sort()).toEqual([
      "ollama_delete",
      "scope",
      "session",
      "task_jobs",
    ]);
    // Belt and braces: the prose warning must survive too.
    expect(byName.get("ollama_delete")!.description).toMatch(/DESTRUCTIVE AND IRREVERSIBLE/);
  });

  it("advertises a permissive object output boundary for every tool", () => {
    for (const tool of tools) {
      expect(tool.outputSchema, `${tool.name}: missing outputSchema`).toMatchObject({
        type: "object",
        additionalProperties: true,
      });
    }
  });

  it("returns canonical structuredContent with a compact doctor text cue", async () => {
    const result = await call("doctor");
    expect(result.isError ?? false).toBe(false);
    expect(result.structuredContent).toBeTruthy();
    expect(typeof result.structuredContent.endpoint).toBe("string");
    if (serveUp) {
      // doctor absorbed `machine`; with serve up it must carry a real profile, not the null branch.
      expect(result.structuredContent.machine?.memory_bytes).toBeTruthy();
    }
    expect(result.content[0].text).toMatch(/Ollama .*resident model/);
    expect(result.content[0].text.length).toBeLessThan(500);
  });

  it.runIf(!serveUp)("serve down: doctor keeps local host evidence, run_task errors cleanly", async () => {
    const doctor = await call("doctor", { view: "full" });
    expect(doctor.isError ?? false).toBe(false);
    expect(doctor.structuredContent.machine?.memory_bytes).toBeGreaterThan(0);
    expect(doctor.structuredContent.machine_unavailable).toBeUndefined();

    // An explicit unavailable endpoint tests refusal without triggering default-endpoint autostart.
    const route = await call("run_task", { endpoint: "http://127.0.0.1:1", task: "completion", preview: true });
    expect(route.isError).toBe(true);
    // Error results must not carry structuredContent.
    expect(route.structuredContent).toBeUndefined();
  });

  it.runIf(serveUp)("serve up: every serve-backed result keeps canonical structured content", async () => {
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
      expect(result.content[0].text).toMatch(/Structured result available|Ollama/);
      expect(result.content[0].text.length).toBeLessThan(500);
      if (name === "run_task") {
        expect(result.structuredContent.execution?.placement).toMatch(/^(cpu|gpu)$/);
        expect(result.structuredContent.execution?.preference).toBe("auto");
        expect(typeof result.structuredContent.execution?.reason).toBe("string");
      }
    }
  });

  it.runIf(serveUp)("withholds embedding vectors by default and returns them on opt-in", async ({ skip }) => {
    let cursor: string | undefined;
    let installed = false;
    do {
      const raw = await call("models", { view: "raw", limit: 50, ...(cursor ? { cursor } : {}) });
      expect(raw.isError ?? false, raw.content[0].text).toBe(false);
      installed = raw.structuredContent.models.some((entry: { name: string }) => entry.name === "nomic-embed-text:latest");
      cursor = raw.structuredContent.page?.next_cursor ?? undefined;
    } while (!installed && cursor);
    if (!installed) skip("nomic-embed-text:latest is not installed");
    const preview = await call("run_task", { task: "embedding", model: "nomic-embed-text:latest", preview: true });
    expect(preview.isError ?? false, preview.content[0].text).toBe(false);
    if (preview.structuredContent.agent_plan?.dispatch_readiness !== "runnable_now")
      skip(`embedding needs available capacity: ${JSON.stringify(preview.structuredContent.agent_plan)}`);
    const embed = await call("run_task", {
      task: "embedding",
      objective: "fastest",
      model: "nomic-embed-text:latest",
      input: "protocol smoke test",
      keepAlive: "0",
      timeoutSeconds: 30,
      maxWaitSeconds: 5,
    });
    expect(embed.isError ?? false, embed.content[0].text).toBe(false);
    const withheld = embed.structuredContent.response.embeddings_omitted;
    expect(withheld).toBeTruthy();
    expect(embed.structuredContent.response.embeddings).toBeUndefined();
    expect(withheld.count).toBe(1);
    expect(typeof withheld.dimensions).toBe("number");
    expect(embed.structuredContent.route?.context_window_fit).toBe("fits_advertised_window");
    expect(embed.structuredContent.route?.hardware_fit).toBe("context_window_only");

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
    expect(JSON.stringify(full.structuredContent).length).toBeGreaterThan(JSON.stringify(embed.structuredContent).length);
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
    expect(JSON.stringify(verbose.structuredContent).length).toBeGreaterThan(JSON.stringify(lean.structuredContent).length);
  });

  it.runIf(serveUp)("models{view:resident} derives placement; detail without model errors with guidance", async () => {
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

  it("doctor reports core Ollama controls with effective defaults", async () => {
    const doctor = await call("doctor", { view: "full" });
    const envConfig = doctor.structuredContent.ollama_config.categories.memory_scheduler;
    const categorized = doctor.structuredContent.ollama_config;
    expect(categorized.visibility_note).toMatch(/separately launched Ollama service|remote endpoint/);
    expect(Object.keys(categorized.categories).sort()).toEqual([
      "backend_device",
      "memory_scheduler",
      "network_security",
      "operations",
      "privacy",
      "storage_lifecycle",
    ]);
    expect(doctor.structuredContent.ollama_env_config).toBeUndefined();
    expect(categorized.categories.memory_scheduler).toEqual(envConfig);
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

  it("delegate_research exposes typed budgets and keeps deployment tuning out of requests", () => {
    const delegate = byName.get("delegate_research")!;
    expect(delegate.inputSchema.properties.minPlacementEvidence.enum).toEqual(["configured", "observed"]);
    expect(delegate.inputSchema.properties.endpoint).toBeTruthy();
    expect(delegate.inputSchema.properties.ollamaEndpoint).toBeUndefined();
    const agent = delegate.inputSchema.properties.agent;
    const properties = agent.anyOf?.[0]?.properties ?? agent.properties;
    expect(properties.contextTokens.type).toBe("integer");
    expect(properties.maxTurns.exclusiveMinimum).toBe(0);
    expect(properties.outputTokens.exclusiveMinimum).toBe(0);
    expect(properties.requestTimeoutSeconds.exclusiveMinimum).toBe(0);
    expect(properties.toolTimeoutSeconds.exclusiveMinimum).toBe(0);
    expect(properties.retryAttempts).toBeUndefined();
    expect(properties.context).toBeUndefined();
    expect((agent.anyOf?.[0] ?? agent).additionalProperties).toBe(false);
  });

  it("refuses unsupported per-call compaction tuning before research starts", async () => {
    const result = await call("delegate_research", {
      question: "Find the task router.",
      workspacePath: REPO_ROOT,
      agent: { context: { pinnedOverflow: "clip" } },
    });
    expect(result.isError).toBe(true);
    expect(result.content[0].text).toMatch(/context|unrecognized/i);
  });

  it("keeps the schema surface within the token budget", () => {
    // Paid on EVERY request, so it is a real running cost. Measured 7,431 tokens across 13 tools
    // before the read-only merge and the de-duplication of RouteDecision.
    const surfaceTokens = Math.round(
      (JSON.stringify(tools).length + (client.getInstructions() ?? "").length) / 4,
    );
    expect(surfaceTokens, `schema surface grew to ~${surfaceTokens} tokens`).toBeLessThan(4500);
  });

  it("models{view:library} defaults to popular, flags cloud, cross-references installed", async () => {
    const local = createServer((request, response) => {
      response.setHeader("Content-Type", "application/json");
      if (request.url === "/api/tags") response.end(JSON.stringify({ models: [{ name: "qwen3-vl:4b" }] }));
      else if (request.url === "/_freellama/v1/machine") response.end(JSON.stringify({ memory_bytes: 16e9 }));
      else { response.statusCode = 503; response.end("{}"); }
    });
    await new Promise<void>((resolve) => local.listen(0, "127.0.0.1", resolve));
    const address = local.address();
    if (!address || typeof address === "string") throw new Error("No fixture port");
    const endpoint = `http://127.0.0.1:${address.port}`;
    let libraryClient: Client | undefined;
    try {
      const preload = new URL("../fixtures/library-fetch.mjs", import.meta.url).href;
      libraryClient = await connectClient({
        NODE_OPTIONS: `${process.env.NODE_OPTIONS ?? ""} --import=${preload}`.trim(),
        FREELLAMA_MCP_AUTOSTART_SERVE: "0",
      });
      const libraryCall = (args: Record<string, unknown>) => libraryClient!.callTool({ name: "models", arguments: args }) as Promise<ToolResult>;
      const search = await libraryCall({ view: "library", capabilities: ["vision"], limit: 6, endpoint, ollamaEndpoint: endpoint });
      expect(search.isError ?? false, search.content[0]?.text).toBe(false);
      const data = search.structuredContent;
      expect(data.order).toBe("popular");
      expect(data.query).not.toMatch(/o=newest/);
      expect(data.models).toHaveLength(1);
      expect(data.models[0]).toMatchObject({ name: "qwen3-vl", cloudAvailable: true, cloudOnly: null, installed: true });
      expect(data.nextStep).toMatch(/model:/);

      const detail = await libraryCall({ view: "library", model: "qwen3-vl", endpoint, ollamaEndpoint: endpoint });
      expect(detail.isError ?? false, detail.content[0]?.text).toBe(false);
      const tags = detail.structuredContent;
      expect(tags.library.status).toBe("available");
      expect(tags.tags).toHaveLength(2);
      expect(tags.tags.every((entry: any) => entry.tag.includes(":"))).toBe(true);
      expect(tags.tags.every((entry: any) => entry.fitScope === "host_memory_budget_only")).toBe(true);
      expect(tags.tags.find((entry: any) => entry.tag === "qwen3-vl:4b")).toMatchObject({ installed: true, fitsInMemory: true });
      expect(tags.tags.find((entry: any) => entry.tag === "qwen3-vl:235b")?.fitsInMemory).toBe(false);
      expect(tags.recommendation.tag).toBe("qwen3-vl:4b");

      const noLocalState = await libraryCall({
        view: "library", model: "qwen3-vl", endpoint: `${endpoint}/unavailable`, ollamaEndpoint: `${endpoint}/unavailable`,
      });
      expect(noLocalState.isError ?? false, noLocalState.content[0]?.text).toBe(false);
      expect(noLocalState.structuredContent.machineMemoryBytes).toBeNull();
      expect(noLocalState.structuredContent.fitBudgetBytes).toBeNull();
      expect(noLocalState.structuredContent.recommendation).toBeNull();
      expect(noLocalState.structuredContent.recommendationUnavailable).toMatch(/could not be checked/);
    } finally {
      local.closeAllConnections();
      await Promise.all([
        libraryClient?.close(),
        new Promise<void>((resolve) => local.close(() => resolve())),
      ]);
    }
  });

  it.skipIf(process.platform === "win32")("keeps the MCP connection usable after a serve spawn failure", async () => {
    const directory = await mkdtemp(join(tmpdir(), "freellama-mcp-startup-"));
    const binary = join(directory, "freellama");
    await writeFile(binary, "Not executable\n", { mode: 0o600 });
    const reserve = createServer();
    await new Promise<void>((resolve) => reserve.listen(0, "127.0.0.1", resolve));
    const address = reserve.address();
    if (!address || typeof address === "string") throw new Error("No fixture port");
    await new Promise<void>((resolve) => reserve.close(() => resolve()));
    let startupClient: Client | undefined;
    try {
      startupClient = await connectClient({
        FREELLAMA_SERVE_BINARY: binary,
        FREELLAMA_SERVE_ENDPOINT: `http://127.0.0.1:${address.port}`,
        FREELLAMA_MCP_AUTOSTART_SERVE: "1",
      });
      const failure = await startupClient.callTool({ name: "models", arguments: { view: "installed" } }) as ToolResult;
      expect(failure.isError).toBe(true);
      expect(failure.content[0]?.text).toMatch(/Could not start freellama serve.*EACCES/);
      expect((await startupClient.listTools()).tools.map((tool) => tool.name).sort()).toEqual([...EXPECTED_TOOLS].sort());
      const retry = await startupClient.callTool({ name: "models", arguments: { view: "installed" } }) as ToolResult;
      expect(retry.isError).toBe(true);
      expect(retry.content[0]?.text).toMatch(/Could not start freellama serve.*EACCES/);
    } finally {
      await Promise.all([
        startupClient?.close(),
        rm(directory, { recursive: true, force: true }),
      ]);
    }
  });
});
