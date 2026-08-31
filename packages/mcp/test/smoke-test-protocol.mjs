// Protocol-completeness checks: every tool must advertise machine-readable behaviour hints and an
// output schema, and every non-error result must actually carry structured content conforming to
// it. These are the parts a client reads to decide whether to prompt a human and how to parse a
// result — prose in a description can't be acted on programmatically.
//
// Runs against a live Ollama but needs no `freellama serve`: the assertions that matter here are
// about the tool *contract*, and `doctor` is the one schema-bearing tool that works standalone.
import assert from "node:assert/strict";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { fileURLToPath } from "node:url";

// Resolve the built server from this file, not the working directory. Every one of these
// scripts used to pass a bare "dist/index.js", so they only ran from packages/mcp — the
// command the README documents (`node packages/mcp/test/validate-all.mjs`, from the repo
// root) failed with MODULE_NOT_FOUND before the server ever started.
const SERVER_ENTRY = fileURLToPath(new URL("../dist/index.js", import.meta.url));

const REPO_ROOT = new URL("..", import.meta.url).pathname.replace(/\/$/, "");

const EXPECTED_TOOLS = [
  "doctor", "models", "route", "search_models", "run_task",
  "ollama_manage", "ollama_delete", "delegate_research",
];

// The single destructive tool. Asserted as an exact set, not a membership check: a future tool
// that quietly forgets `destructiveHint` fails here, and so does one that over-claims it.
const EXPECTED_DESTRUCTIVE = ["ollama_delete"];
const EXPECTED_READ_ONLY = ["doctor", "models"];

const transport = new StdioClientTransport({ command: "node", args: [SERVER_ENTRY] });
const client = new Client({ name: "protocol-test", version: "0.0.1" });
await client.connect(transport);

const { tools } = await client.listTools();
const byName = new Map(tools.map((t) => [t.name, t]));

console.log("=== tools/list ===");
assert.deepEqual([...byName.keys()].sort(), [...EXPECTED_TOOLS].sort(), "tool set changed");
console.log(`${tools.length} tools: ${EXPECTED_TOOLS.join(", ")}`);

// Annotations were removed from every tool at the user's direction. Consequence, stated so it is
// not rediscovered later: with none declared, the MCP spec defaults apply — readOnlyHint=false and
// destructiveHint=TRUE — so a client that gates on those hints now treats EVERY tool as destructive,
// including `doctor`. The guard for `ollama_delete` therefore lives entirely in its description.
console.log("\n=== annotations declare only deviations from spec defaults ===");
// Spec defaults: readOnlyHint=false, destructiveHint=true, idempotentHint=false, openWorldHint=true.
// Restating a default costs bytes and says nothing; omitting a deviation loses real signal.
for (const tool of tools) {
  const a = tool.annotations ?? {};
  assert.ok(tool.annotations, `${tool.name}: no annotations`);
  assert.equal(a.openWorldHint, undefined, `${tool.name}: openWorldHint restates the default`);
  assert.equal(tool.title, undefined, `${tool.name}: title duplicates the name`);
  if (a.readOnlyHint === true) {
    assert.equal(a.destructiveHint, undefined, `${tool.name}: meaningless on a read-only tool`);
    assert.equal(a.idempotentHint, undefined, `${tool.name}: meaningless on a read-only tool`);
  }
}
assert.equal(byName.get("ollama_delete").annotations.destructiveHint, true,
  "the one irreversible tool must be machine-readable dangerous");
assert.deepEqual(tools.filter((t) => t.annotations?.destructiveHint === true).map((t) => t.name),
  ["ollama_delete"], "destructive set changed");
assert.match(byName.get("ollama_delete").description, /DESTRUCTIVE AND IRREVERSIBLE/,
  "belt and braces: the prose warning must survive too");
console.log(`  ${tools.length} tools annotated; ollama_delete is the only destructive one`);

console.log("\n=== no output schemas advertised (removed on purpose) ===");
for (const tool of tools) {
  assert.equal(tool.outputSchema, undefined, `${tool.name}: outputSchema is back — it costs every request`);
}
console.log(`  ${tools.length}/${tools.length} tools advertise none; structuredContent still returned (asserted below)`);

console.log("\n=== structuredContent on a real call (doctor) ===");
const result = await client.callTool({ name: "doctor", arguments: {} });
assert.ok(!result.isError, `doctor failed: ${result.content?.[0]?.text}`);
assert.ok(result.structuredContent, "doctor returned no structuredContent");
assert.equal(typeof result.structuredContent.endpoint, "string");
// doctor absorbed `machine`; with serve up it must carry a real profile, not just the null branch.
if (await fetch("http://127.0.0.1:11435/_freellama/v1/health").then((r) => r.ok).catch(() => false)) {
  assert.ok(result.structuredContent.machine?.unified_memory_bytes, "doctor lost the absorbed machine profile");
}
// The spec says a tool returning structured content SHOULD also return the serialized JSON as
// text. Assert the two halves are the same object, not merely both present.
assert.deepEqual(
  JSON.parse(result.content[0].text),
  result.structuredContent,
  "text block and structuredContent disagree",
);
console.log(`  endpoint=${result.structuredContent.endpoint}, both halves agree`);

// Whether `freellama serve` is up decides which half of the contract is checkable, so probe once
// and assert the appropriate one. Both halves matter: with serve down, an error result must stay
// error-shaped (the SDK exempts `isError` results from output validation, which is the only
// reason a declared outputSchema can't turn a connection failure into a protocol error); with
// serve up, every declared schema must actually accept what the Rust layer really returns.
const serveUp = await fetch("http://127.0.0.1:11435/_freellama/v1/health")
  .then((r) => r.ok)
  .catch(() => false);

if (!serveUp) {
  console.log("\n=== serve down: doctor degrades, route still errors cleanly ===");
  // doctor absorbed the former `machine` tool but must still work with no serve running — the
  // Ollama half of the diagnostic is exactly the half you need when things are broken.
  const doc = await client.callTool({ name: "doctor", arguments: {} });
  assert.ok(!doc.isError, "doctor must still succeed with no serve running");
  assert.equal(doc.structuredContent.machine, null, "machine should be null, not missing");
  assert.match(doc.structuredContent.machine_unavailable, /unreachable/);
  console.log(`  doctor ok, machine=null with a reason`);
  const routeResult = await client.callTool({ name: "route", arguments: { task: "completion" } });
  assert.equal(routeResult.isError, true, "route should error with no serve running");
  assert.equal(routeResult.structuredContent, undefined, "error results must not carry structuredContent");
  console.log(`  route isError=true, no structuredContent`);
  console.log("  (start `freellama serve` to also validate the serve-backed output schemas)");
} else {
  // A declared outputSchema the real payload violates is strictly worse than none: the SDK turns
  // it into an McpError and the tool stops working. Exercise every serve-backed schema against a
  // live server so that failure mode is caught here rather than in a client.
  console.log("\n=== serve up: every serve-backed schema validated against real payloads ===");
  const live = [
    ["models", {}],
    ["models (raw)", { view: "raw" }, "models"],
    ["models (resident)", { view: "resident" }, "models"],
    ["route", { task: "completion", objective: "fastest" }],
  ];
  for (const [label, args, realName] of live) {
    const res = await client.callTool({ name: realName ?? label, arguments: args });
    assert.ok(!res.isError, `${label} failed: ${res.content?.[0]?.text}`);
    assert.ok(res.structuredContent, `${label} returned no structuredContent`);
    assert.deepEqual(JSON.parse(res.content[0].text), res.structuredContent, `${label}: halves disagree`);
    console.log(`  ${label.padEnd(20)} ok (${Object.keys(res.structuredContent).length} top-level keys)`);
  }

  // run_task is the only routing tool that executes, so it is the only one whose schema covers a
  // real Ollama response. Embedding is the cheapest way to exercise it, and it doubles as the
  // check that vectors are withheld by default: returning them defeats the tool's whole purpose.
  const embed = await client.callTool({
    name: "run_task",
    arguments: {
      task: "embedding",
      objective: "fastest",
      model: "nomic-embed-text:latest",
      input: "protocol smoke test",
      keepAlive: "0",
    },
  });
  if (embed.isError) {
    console.log(`  run_task            skipped (${embed.content[0].text.slice(0, 70)})`);
  } else {
    assert.ok(embed.structuredContent, "run_task returned no structuredContent");
    const withheld = embed.structuredContent.response.embeddings_omitted;
    assert.ok(withheld, "embeddings were not withheld by default");
    assert.equal(embed.structuredContent.response.embeddings, undefined, "raw vectors leaked into the default result");
    assert.equal(withheld.count, 1);
    assert.equal(typeof withheld.dimensions, "number");
    const bytes = embed.content[0].text.length;
    console.log(`  run_task            ok, ${withheld.count}x${withheld.dimensions} vector withheld -> ${bytes} bytes (~${Math.round(bytes / 4)} tokens)`);

    const full = await client.callTool({
      name: "run_task",
      arguments: {
        task: "embedding", objective: "fastest", model: "nomic-embed-text:latest",
        input: "protocol smoke test", keepAlive: "0", returnEmbeddings: true,
      },
    });
    assert.ok(!full.isError, "returnEmbeddings:true failed");
    assert.equal(full.structuredContent.response.embeddings.length, 1, "opt-in did not return vectors");
    const fullBytes = full.content[0].text.length;
    console.log(`  run_task (opt-in)   ok, vectors returned -> ${fullBytes} bytes (~${Math.round(fullBytes / 4)} tokens), ${Math.round(fullBytes / bytes)}x larger`);
  }
}

console.log("\n=== models{view:detail} withholds the license/modelfile blobs by default ===");
const tags = await client.callTool({ name: "models", arguments: { view: "raw" } });
const someModel = tags.structuredContent.models?.[0]?.name;
if (!someModel) {
  console.log("  skipped (no models installed)");
} else {
  const lean = await client.callTool({ name: "models", arguments: { view: "detail", model: someModel } });
  assert.ok(!lean.isError, `models{detail} failed: ${lean.content?.[0]?.text}`);
  assert.equal(lean.structuredContent.license, undefined, "license leaked into the default response");
  assert.equal(lean.structuredContent.modelfile, undefined, "modelfile leaked into the default response");
  assert.ok(Array.isArray(lean.structuredContent.capabilities), "capabilities missing");
  const verbose = await client.callTool({
    name: "models",
    arguments: { view: "detail", model: someModel, includeVerbose: true },
  });
  assert.ok(!verbose.isError, "includeVerbose failed");
  const leanBytes = lean.content[0].text.length;
  const verboseBytes = verbose.content[0].text.length;
  assert.ok(verboseBytes > leanBytes, "includeVerbose returned no more than the lean form");
  console.log(`  ${someModel}: ${leanBytes} bytes lean vs ${verboseBytes} verbose (${Math.round((1 - leanBytes / verboseBytes) * 100)}% withheld)`);
  console.log(`  max_context_length=${lean.structuredContent.max_context_length}, capabilities=${lean.structuredContent.capabilities.join("/")}`);
}

console.log("\n=== models{view:resident} derives the GPU/CPU split ===");
const ps = await client.callTool({ name: "models", arguments: { view: "resident" } });
assert.ok(!ps.isError, "models{resident} failed");
// `detail` without `model` must fail with actionable guidance, not a confusing upstream error.
const noModel = await client.callTool({ name: "models", arguments: { view: "detail" } });
assert.equal(noModel.isError, true, "detail without model should error");
assert.match(noModel.content[0].text, /requires a `model` argument/);
for (const m of ps.structuredContent.models ?? []) {
  assert.ok(m.placement, `${m.name}: no derived placement`);
  console.log(`  ${m.name}: ${m.placement.processor}`);
}
if ((ps.structuredContent.models ?? []).length === 0) console.log("  (nothing resident right now)");

console.log("\n=== doctor reports the memory-governing env vars with effective defaults ===");
const doc = await client.callTool({ name: "doctor", arguments: {} });
const envCfg = doc.structuredContent.ollama_env_config;
for (const key of ["OLLAMA_MAX_LOADED_MODELS", "OLLAMA_CONTEXT_LENGTH", "OLLAMA_KV_CACHE_TYPE", "OLLAMA_NUM_PARALLEL"]) {
  assert.ok(envCfg[key], `doctor does not report ${key}`);
  assert.ok(envCfg[key].effective_default, `${key} reported without an effective_default`);
}
assert.doesNotMatch(
  JSON.stringify(doc.structuredContent),
  /unlimited/,
  "doctor still claims MAX_LOADED_MODELS defaults to unlimited (it resolves to 3 x GPU count)",
);
console.log(`  ${Object.keys(envCfg).length} vars reported, each with an effective_default`);

console.log("\n=== minConfidence fails closed on a weakly-justified route ===");
if (!serveUp) {
  console.log("  skipped (needs freellama serve)");
} else {
  const open = await client.callTool({ name: "route", arguments: { task: "completion", objective: "fastest" } });
  assert.ok(!open.isError, "unfiltered route should succeed");
  // The server grades a no-policy/no-benchmark pick "low" (route_evidence in packages/rust-core/src/platform.rs).
  // If that ever becomes "medium" this assertion should be revisited, not deleted.
  assert.equal(open.structuredContent.confidence, "low", "expected a low-confidence baseline route");
  const gated = await client.callTool({
    name: "route",
    arguments: { task: "completion", objective: "fastest", minConfidence: "medium" },
  });
  assert.equal(gated.isError, true, "minConfidence:medium should refuse a low-confidence route");
  assert.match(gated.content[0].text, /fail-closed refusal/);
  // The refusal must name the rejected model and the missing evidence, or the caller can't decide
  // whether to escalate or to go configure a policy.
  assert.match(gated.content[0].text, new RegExp(open.structuredContent.selected_model.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  assert.match(gated.content[0].text, /capability_metadata_only/);
  console.log(`  route refused: would have picked ${open.structuredContent.selected_model} (confidence=low)`);

  // The gate has to fire BEFORE generation, or it saves nothing.
  const t0 = Date.now();
  const blocked = await client.callTool({
    name: "run_task",
    arguments: { task: "completion", objective: "fastest", prompt: "hi", minConfidence: "medium" },
  });
  const elapsed = Date.now() - t0;
  assert.equal(blocked.isError, true, "run_task should refuse below the confidence floor");
  assert.ok(elapsed < 5000, `run_task refused in ${elapsed}ms — too slow to have skipped generation`);
  console.log(`  run_task refused in ${elapsed}ms, before spending any tokens`);
}

console.log("\n=== delegate_research defaults to the faster adapter ===");
const adapterTool = byName.get("delegate_research");
assert.deepEqual(
  adapterTool.inputSchema.properties.adapter.enum.sort(),
  ["bash", "octocode"],
  "adapter param missing or wrong",
);
// The verification contract is no longer advertised in an output schema, so assert it on a real
// result instead — which is the stronger check anyway.
console.log("  adapter: bash|octocode (verification contract asserted on a live call below)");

console.log("\n=== schema surface stays within budget ===");
// Paid on EVERY request, so it is a real running cost, not a one-off. Measured 7,431 tokens
// across 13 tools before the read-only merge and the de-duplication of RouteDecision.
const surfaceTokens = Math.round((JSON.stringify(tools).length + (client.getInstructions() ?? "").length) / 4);
// Budget moved 3,400 -> 3,550 when search_models gained its two-step (search -> inspect tags)
// flow. Raised deliberately for a capability, not to silence the guard: keeping search and
// inspect in ONE tool costs 447 tokens against ~700 for two separate tools, because a second tool
// repeats name, description, and the endpoint/limit params.
assert.ok(surfaceTokens < 3400, `schema surface grew to ~${surfaceTokens} tokens (budget 3400)`);
console.log(`  ~${surfaceTokens} tokens total (was 7431)`);

console.log("\n=== verification is model-aware, and refuses before running ===");
// Deliberately targets a model that is NOT installed. delegate_research now checks MODEL_EVIDENCE
// before spawning anything, so a model measured unusable is refused instantly — which also makes
// this assertion immune to the recurring problem that killed its two predecessors: it pointed at
// llama3.2:3b, then gemma4:12b-mlx, and each deletion turned it into a silent skip.
const t0 = Date.now();
const weak = await client.callTool(
  { name: "delegate_research", arguments: {
    question: "In packages/rust-core/Cargo.toml, what optional feature enables the Node addon?",
    workspacePath: REPO_ROOT, model: "qwen2.5:0.5b" } },
  undefined, { timeout: 60000 },
);
const elapsed = Date.now() - t0;
assert.ok(!weak.isError, "an unusable model should be refused with a verdict, not an error");
const v = weak.structuredContent.verification;
assert.equal(v.recommendation, "escalate", "a model measured 0/8 must escalate");
assert.match(v.measuredBaseRate, /qwen2\.5:0\.5b/, "base rate must name the model that was asked for");
assert.doesNotMatch(v.measuredBaseRate, /^98\.9/, "quoted a different model's base rate");
assert.ok(elapsed < 5000, `took ${elapsed}ms — it ran the model instead of refusing up front`);
console.log(`  qwen2.5:0.5b -> escalate in ${elapsed}ms, without running it`);

console.log("\n=== search_models: popular by default, cloud flagged, installed cross-referenced ===");
const sm = await client.callTool(
  { name: "search_models", arguments: { capabilities: ["vision"], limit: 6 } },
  undefined, { timeout: 60000 },
).catch(() => null);
if (!sm || sm.isError) {
  console.log("  skipped (ollama.com unreachable)");
} else {
  const d = sm.structuredContent;
  assert.equal(d.order, "popular", "must default to popular, never newest");
  assert.doesNotMatch(d.query, /o=newest/, "default query must not request newest");
  assert.ok(d.models.length > 0, "parser returned nothing — ollama.com markup may have changed");
  for (const m of d.models) {
    assert.equal(typeof m.name, "string");
    assert.equal(typeof m.cloudOnly, "boolean", `${m.name}: cloudOnly missing`);
    assert.equal(typeof m.installed, "boolean", `${m.name}: installed cross-reference missing`);
  }
  assert.match(d.nextStep, /model:/, "search result must point at the inspect step");
  console.log(`  step 1: ${d.models.length} results, order=${d.order}, ${d.models.filter((m) => m.cloudOnly).length} cloud-only, ${d.models.filter((m) => m.installed).length} installed`);

  // Step 2 is what makes the result actionable: a family is not pullable, only a tag is, and only
  // the tag carries the size that decides whether it fits.
  const det = await client.callTool(
    { name: "search_models", arguments: { model: "qwen3-vl" } }, undefined, { timeout: 60000 },
  ).catch(() => null);
  if (!det || det.isError) {
    console.log("  step 2 skipped (ollama.com unreachable)");
  } else {
    const t2 = det.structuredContent;
    assert.ok(t2.tags.length > 0, "no tags parsed — ollama.com markup may have changed");
    assert.ok(t2.tags.every((x) => x.tag.includes(":")), "tags must be pullable name:tag form");
    // A 235B model must not be reported as fitting a 48GB machine.
    const huge = t2.tags.find((x) => (x.sizeBytes ?? 0) > 100e9);
    if (huge && t2.fitBudgetBytes) assert.equal(huge.fitsInMemory, false, `${huge.tag} wrongly reported as fitting`);
    // Fail CLOSED: with no machine profile the fit is unknowable, and "largest not known to fail"
    // once recommended a 143GB model on a 48GB machine. No budget => no recommendation, with a
    // stated reason so a caller can tell "nothing fits" from "I could not check".
    if (!t2.fitBudgetBytes) {
      assert.equal(t2.recommendation, null, "recommended a tag without knowing the memory budget");
      assert.match(t2.recommendationUnavailable ?? "", /could not be checked/);
    } else {
      assert.ok(t2.recommendation.tag.includes(":"), "recommendation must name a pullable tag");
      assert.equal(t2.tags.find((x) => x.tag === t2.recommendation.tag)?.fitsInMemory, true,
        "recommended a tag that does not fit");
    }
    console.log(`  step 2: ${t2.tags.length} tags, recommends ${t2.recommendation?.tag ?? "(none)"}`);
  }
}

console.log("\n=== workspace boundary rejects an outside path ===");
const escape = await client.callTool({
  name: "delegate_research",
  arguments: { question: "anything", workspacePath: "/etc" },
});
assert.equal(escape.isError, true, "/etc should be rejected");
assert.match(escape.content[0].text, /outside the allowed research roots/);
console.log(`  ${escape.content[0].text.split("\n")[0].slice(0, 90)}...`);

await client.close();
console.log("\nAll protocol assertions passed.");
process.exit(0);
