import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { CASE, acquireLock, distribution, grade, health, measureRequest, requestFor, runBenchmark, summarizeVendor, validateConfig } from "./lib.mjs";

const answer = JSON.stringify(CASE.expected);
const encoder = new TextEncoder();
const ndjson = (content = answer) => [JSON.stringify({ message: { content } }), JSON.stringify({ done: true, eval_count: 20, eval_duration: 1e9, prompt_eval_count: 80, load_duration: 10e6 })].join("\n") + "\n";
const vendor = (id = "ollama") => ({ id, protocol: "ollama", base_url: `http://${id}.invalid`, model: "fixture:1b" });

function streamResponse(chunks) {
  return new Response(new ReadableStream({ start(controller) { for (const chunk of chunks) controller.enqueue(encoder.encode(chunk)); controller.close(); } }), { headers: { "content-type": "application/x-ndjson" } });
}

test("exact extraction grading rejects extra fields and prose", () => {
  assert.deepEqual(grade(answer), { json_valid: true, schema_valid: true, passed: true });
  assert.deepEqual(grade(JSON.stringify({ ...CASE.expected, invented: "x" })), { json_valid: true, schema_valid: false, passed: false });
  assert.deepEqual(grade(JSON.stringify({ ...CASE.expected, priority: "low" })), { json_valid: true, schema_valid: true, passed: false });
  assert.deepEqual(grade(JSON.stringify({ ...CASE.expected, id: 104 })), { json_valid: true, schema_valid: false, passed: false });
  assert.deepEqual(grade("```json\n" + answer + "\n```"), { json_valid: false, schema_valid: false, passed: false });
  assert.deepEqual(grade("[]"), { json_valid: true, schema_valid: false, passed: false });
});

test("normalized task KPIs include failed effort, exclude warm-ups, and use the measured client window", () => {
  const sample = (outcome, start, end, prompt, output, warmup = false) => ({ outcome, client_start_ms: start, client_end_ms: end, elapsed_ms: end - start, first_content_ms: null, metrics: { prompt_tokens: prompt, output_tokens: output }, warmup });
  const summary = summarizeVendor(managedVendor(), [
    sample("verified", 0, 10000, 9999, 9999, true),
    sample("verified", 20000, 21000, 100, 20),
    sample("quality_failed", 22000, 25000, 110, 30),
  ]);
  assert.equal(summary.attempted, 2);
  assert.equal(summary.quality_pass_rate, 0.5);
  assert.equal(summary.verified_elapsed_ms.p50, 1000);
  assert.equal(summary.verified_first_content_ms.p50, null);
  assert.equal(summary.measured_window_ms, 5000);
  assert.equal(summary.useful_tasks_per_minute, 12);
  assert.equal(summary.local_tokens.prompt_total, 210);
  assert.equal(summary.local_tokens.output_total, 50);
  assert.equal(summary.local_tokens.total_per_verified_completion, 260);
  assert.equal(summary.local_tokens.combined_coverage, 1);
  assert.equal(summary.local_tokens.paid_agent_tokens, null);
  assert.equal(summary.local_tokens.paid_agent_tokens_status, "not_measured");
});

test("missing token usage and empty/invalid measured windows remain unknown rather than cheap", () => {
  const trials = [
    { warmup: false, outcome: "verified", client_start_ms: 100, client_end_ms: 200, elapsed_ms: 100, metrics: { prompt_tokens: 80, output_tokens: 10 } },
    { warmup: false, outcome: "failed", client_start_ms: 200, client_end_ms: 500, elapsed_ms: 300, metrics: { prompt_tokens: null, output_tokens: 5 } },
  ];
  const summary = summarizeVendor(vendor(), trials);
  assert.equal(summary.local_tokens.prompt_total, null);
  assert.equal(summary.local_tokens.known_prompt_total, 80);
  assert.equal(summary.local_tokens.prompt_coverage, 0.5);
  assert.equal(summary.local_tokens.output_total, 15);
  assert.equal(summary.local_tokens.output_per_verified_completion, 15);
  assert.equal(summary.local_tokens.total_per_verified_completion, null);
  assert.equal(summarizeVendor(vendor(), []).useful_tasks_per_minute, null);
  assert.equal(summarizeVendor(vendor(), []).local_tokens.prompt_total, null);
  assert.equal(summarizeVendor(vendor(), [{ ...trials[0], outcome: "failed" }]).local_tokens.total_per_verified_completion, null);
  assert.equal(summarizeVendor(vendor(), [{ ...trials[0], client_end_ms: 50 }]).measured_window_ms, null);
  assert.equal(summarizeVendor(vendor(), [{ ...trials[0], metrics: { prompt_tokens: -1, output_tokens: 0.5 } }]).local_tokens.combined_coverage, 0);
});

test("explicit artifact cohorts do not give MLX a GGUF digest or quantization", () => {
  const sha = "a".repeat(64), other = "b".repeat(64);
  const model = { artifact_sha256: sha, quantization: "Q4_K_M" };
  const gguf = summarizeVendor({ ...vendor(), cohort: "gguf" }, [], model);
  assert.equal(gguf.comparison_cohort, "gguf");
  assert.equal(gguf.declared_artifact_sha256, sha);
  const mlx = summarizeVendor({ ...vendor(), cohort: "mlx" }, [], model);
  assert.equal(mlx.declared_artifact_sha256, null);
  assert.equal(mlx.quantization, null);
  assert.equal(summarizeVendor({ ...vendor(), cohort: "mlx", artifact_sha256: `sha256:${other}` }, [], model).declared_artifact_sha256, other);
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), cohort: " " }] }), /nonempty/);
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), artifact_sha256: "not-a-sha" }] }), /SHA-256/);
});

test("protocols receive the identical messages and bounded generation settings", () => {
  const native = requestFor(vendor(), { context_tokens: 2048, output_tokens: 32 });
  const managed = requestFor({ ...vendor("freellama"), protocol: "freellama" }, { context_tokens: 2048, output_tokens: 32 });
  const compatible = requestFor({ ...vendor("openai"), protocol: "openai" }, { output_tokens: 32 });
  assert.deepEqual(native.body, managed.body);
  assert.deepEqual(native.body.messages, compatible.body.messages);
  assert.equal(native.body.options.num_predict, compatible.body.max_tokens);
  assert.equal(native.body.options.temperature, compatible.body.temperature);
  assert.equal(native.body.options.seed, compatible.body.seed);
  assert.equal(native.body.options.num_ctx, 2048);
  const task = requestFor({ ...vendor("freellama-managed"), protocol: "freellama-managed" }, { context_tokens: 2048, output_tokens: 32 });
  assert.equal(task.url, "http://freellama-managed.invalid/_freellama/v1/tasks");
  assert.equal(task.body.task, "completion");
  assert.deepEqual(task.body.messages, native.body.messages);
  assert.equal(task.body.context_tokens, native.body.options.num_ctx);
  assert.equal(task.body.request_options.think, native.body.think);
  assert.deepEqual(task.body.request_options.options, { temperature: 0, seed: 42, num_predict: 32 });
});

const managedVendor = () => ({ ...vendor("freellama-managed"), protocol: "freellama-managed" });
const receipt = (content = answer) => ({
  route: { task: "completion", selected_model: "fixture:1b" },
  admission: { mode: "managed", queue_wait_ms: 7, resource_wait_ms: 3, transition_wait_ms: 1, total_wait_ms: 11 },
  execution: { backend: "gpu", observation: { status: "verified", processor: "gpu" } },
  metrics: { prompt_tokens: 80, output_tokens: 20, load_duration_ns: 10e6, prompt_duration_ns: 40e6, output_tokens_per_second: 20 },
  timing: { elapsed_ms: 900 },
  response: { done: true, message: { role: "assistant", content, thinking: "receipt thinking" } },
});

test("managed completion retains its receipt and client wall time without inventing streamed TTFT", async () => {
  const value = receipt();
  const wire = JSON.stringify(value);
  let clock = 0;
  let calls = 0;
  const result = await measureRequest(managedVendor(), {}, { fetchImpl: async (url, options) => {
    calls++; assert.match(url, /\/_freellama\/v1\/tasks$/);
    assert.deepEqual(JSON.parse(options.body).messages, CASE.messages);
    return new Response(wire);
  }, now: () => clock++ * 1000 });
  assert.equal(calls, 1);
  assert.equal(result.outcome, "verified");
  assert.equal(result.first_content_ms, null);
  assert.equal(result.elapsed_ms, 1000);
  assert.equal(result.completed_stream, null);
  assert.equal(result.response_complete, true);
  assert.equal(result.server_timing.elapsed_ms, 900);
  assert.equal(result.raw_response, wire);
  assert.deepEqual(result.receipt, value);
  assert.deepEqual(result.route, value.route);
  assert.deepEqual(result.execution, value.execution);
  assert.equal(result.admission.queue_wait_ms, 7);
  assert.equal(result.thinking, "receipt thinking");
  assert.deepEqual(result.metrics, { prompt_tokens: 80, output_tokens: 20, load_ms: 10, prompt_eval_ms: 40, output_tokens_per_second: 20 });
});

test("managed completion uses exact grading and leaves unavailable engine metrics unknown", async () => {
  const value = receipt(JSON.stringify({ ...CASE.expected, priority: "low" }));
  value.metrics = { output_tokens: 20, load_duration_ns: -1 };
  const result = await measureRequest(managedVendor(), {}, { fetchImpl: async () => Response.json(value) });
  assert.equal(result.outcome, "quality_failed");
  assert.equal(result.grade.schema_valid, true);
  assert.equal(result.metrics.prompt_tokens, null);
  assert.equal(result.metrics.load_ms, null);
  assert.equal(result.metrics.output_tokens_per_second, null);
});

test("managed errors preserve safe refusal versus unknown/post-start execution and never retry", async () => {
  for (const [execution_outcome, expected] of [["not_started", "refused"], ["unknown", "failed"], ["started", "failed"]]) {
    let calls = 0;
    const value = { code: "resource_admission_unavailable", error: "capacity unavailable", execution_outcome, retry_safe: execution_outcome === "not_started" };
    const result = await measureRequest(managedVendor(), {}, { fetchImpl: async () => { calls++; return Response.json(value, { status: 503 }); } });
    assert.equal(calls, 1);
    assert.equal(result.outcome, expected);
    assert.deepEqual(result.receipt, value);
    assert.equal(result.first_content_ms, null);
    assert.equal(result.response_complete, false);
  }
});

test("managed completion rejects missing terminal evidence, wrong pinned model, and malformed bodies", async () => {
  const incomplete = receipt(); delete incomplete.response.done;
  const wrongModel = receipt(); wrongModel.route.selected_model = "other:1b";
  const errored = { ...receipt(), error: { code: "execution_outcome_unknown", execution_outcome: "unknown" } };
  for (const value of [incomplete, wrongModel, errored, null]) {
    const result = await measureRequest(managedVendor(), {}, { fetchImpl: async () => Response.json(value) });
    assert.equal(result.outcome, "failed");
    assert.equal(result.response_complete, false);
  }
  const malformed = await measureRequest(managedVendor(), {}, { fetchImpl: async () => new Response("not json") });
  assert.equal(malformed.raw_response, "not json");
  assert.equal(malformed.outcome, "failed");
});

test("managed health checks readiness then the pinned upstream inventory", async () => {
  const calls = [];
  const value = await health(managedVendor(), {}, { fetchImpl: async (url) => {
    calls.push(new URL(url).pathname);
    if (url.endsWith("/ready")) return Response.json({ ready: true });
    if (url.endsWith("/version")) return Response.json({ version: "upstream-fixture" });
    return Response.json({ models: [{ name: "fixture:1b" }] });
  } });
  assert.deepEqual(calls, ["/_freellama/v1/ready", "/api/version", "/api/tags"]);
  assert.equal(value.version, null);
  assert.equal(value.upstream_version, "upstream-fixture");
});

test("managed run reports receipt queue metrics and no TTFT separately from raw admission", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "freellama-bench-managed-"));
  const calls = [];
  const raw = { ...vendor("freellama-raw"), protocol: "freellama" };
  const fetchImpl = async (url) => {
    const route = new URL(url).pathname;
    if (route === "/api/version") return Response.json({ version: "upstream-fixture" });
    if (route === "/api/tags") return Response.json({ models: [{ name: "fixture:1b" }] });
    if (route === "/_freellama/v1/ready") return Response.json({ ready: true });
    calls.push(route);
    return route === "/_freellama/v1/tasks" ? Response.json(receipt()) : streamResponse([ndjson()]);
  };
  try {
    const out = path.join(directory, "out");
    const report = await runBenchmark({ config: { model: {}, vendors: [raw, managedVendor()] }, out, samples: 1, warmups: 1, allowSmoke: true, lockPath: path.join(directory, "lock"), fetchImpl, onProgress: () => {} });
    assert.deepEqual(calls, ["/api/chat", "/api/chat", "/_freellama/v1/tasks", "/_freellama/v1/tasks"]);
    assert.match(report.vendors[0].mode, /raw admission/);
    assert.match(report.vendors[1].mode, /managed completion/);
    assert.equal(report.vendors[0].all_attempt_queue_wait_ms.n, 0);
    assert.equal(report.vendors[1].all_attempt_queue_wait_ms.p50, 7);
    assert.equal(report.vendors[1].all_attempt_total_wait_ms.p50, 11);
    assert.equal(report.vendors[1].verified_first_content_ms.n, 0);
    assert.equal(report.vendors[1].verified_first_content_ms.p50, null);
    assert.equal(report.vendors[1].verified_elapsed_ms.n, 1);
    const trials = (await readFile(path.join(out, "freellama-managed", "trials.jsonl"), "utf8")).trim().split("\n").map(JSON.parse);
    assert.deepEqual(trials[1].receipt, receipt());
    assert.match(await readFile(path.join(out, "summary.csv"), "utf8"), /queue_wait_p50_ms/);
    assert.match(await readFile(path.join(out, "SUMMARY.md"), "utf8"), /Managed completion is nonstreaming/);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("NDJSON TTFT excludes thinking and tolerates frames split across reads", async () => {
  const thinking = JSON.stringify({ message: { thinking: "reason", content: "" } }) + "\n";
  const wire = thinking + ndjson();
  let clock = 0;
  const result = await measureRequest(vendor(), {}, { fetchImpl: async () => streamResponse([wire.slice(0, 9), wire.slice(9, 80), wire.slice(80)]), now: () => clock++ * 10 });
  assert.equal(result.outcome, "verified");
  assert.equal(result.thinking, "reason");
  assert.equal(result.first_content_ms, 10);
  assert.equal(result.elapsed_ms, 20);
  assert.equal(result.raw_response, wire);
  assert.equal(result.metrics.output_tokens_per_second, 20);
});

test("SSE records content, final usage and DONE, without inventing engine speed", async () => {
  const frames = [
    { choices: [{ delta: { reasoning_content: "thinking" } }] },
    { choices: [{ delta: { content: answer.slice(0, 12) } }] },
    { choices: [{ delta: { content: answer.slice(12) }, finish_reason: "stop" }] },
    { choices: [], usage: { completion_tokens: 20, prompt_tokens: 80 } },
  ];
  const wire = frames.map((frame) => `data: ${JSON.stringify(frame)}\r\n\r\n`).join("") + "data: [DONE]\r\n\r\n";
  const result = await measureRequest({ ...vendor(), protocol: "openai" }, {}, { fetchImpl: async () => streamResponse([wire.slice(0, 23), wire.slice(23)]) });
  assert.equal(result.outcome, "verified");
  assert.equal(result.content, answer);
  assert.equal(result.metrics.output_tokens, 20);
  assert.equal(result.metrics.output_tokens_per_second, null);
  assert.equal(result.raw_response, wire);
});

test("a valid answer with a truncated stream is failed, not verified", async () => {
  const wire = JSON.stringify({ message: { content: answer } }) + "\n";
  const result = await measureRequest(vendor(), {}, { fetchImpl: async () => streamResponse([wire]) });
  assert.equal(result.outcome, "failed");
  assert.equal(result.completed_stream, false);
  assert.match(result.error, /Truncated stream/);
  assert.equal(result.raw_response, wire);
});

test("malformed frames retain the whole raw response", async () => {
  const wire = "{bad frame}\n" + ndjson();
  const result = await measureRequest(vendor(), {}, { fetchImpl: async () => streamResponse(["{bad frame}\n", ndjson()]) });
  assert.equal(result.outcome, "failed");
  assert.match(result.error, /Malformed stream frame/);
  assert.equal(result.raw_response, wire);
});

test("generic 503 is not mislabeled a pre-execution refusal", async () => {
  const failed = await measureRequest(vendor(), {}, { fetchImpl: async () => new Response("engine crashed", { status: 503 }) });
  const refused = await measureRequest(vendor(), {}, { fetchImpl: async () => Response.json({ code: "admission_timeout" }, { status: 503 }) });
  assert.equal(failed.outcome, "failed");
  assert.equal(refused.outcome, "refused");
  assert.equal(failed.raw_response, "engine crashed");
});

test("config rejects duplicates, shell stop commands, and embedded credentials", () => {
  assert.throws(() => validateConfig({ model: {}, vendors: [vendor(), vendor()] }), /unique/);
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), stop: "pkill ollama" }] }), /only a process started/);
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), base_url: "http://user:secret@localhost" }] }), /credentials/);
  assert.throws(() => validateConfig({ model: {}, vendors: [{ ...vendor(), start: { command: "bash", args: "-c anything" } }] }), /args array/);
});

test("quantiles retain null for unavailable metrics", () => {
  assert.equal(distribution([null, undefined]).p50, null);
  assert.deepEqual(distribution([30, 10, 20]), { n: 3, p50: 20, p95: 30, p99: 30, min: 10, max: 30, spread: 20 });
});

test("exclusive run lock is never silently stolen", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "freellama-bench-lock-"));
  try {
    const lock = path.join(directory, "lock");
    const unlock = await acquireLock(lock);
    await assert.rejects(acquireLock(lock), /Another benchmark/);
    await unlock();
    const releaseAgain = await acquireLock(lock);
    await releaseAgain();
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("vendors and requests stay sequential, warmups are excluded, and raw outcomes persist", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "freellama-bench-run-"));
  const out = path.join(directory, "results");
  let active = 0;
  let maxActive = 0;
  const calls = [];
  const counts = new Map();
  const fetchImpl = async (url, options = {}) => {
    const parsed = new URL(url);
    if (parsed.pathname === "/api/version") return Response.json({ version: "fixture-1" });
    if (parsed.pathname === "/api/tags") return Response.json({ models: [{ name: "fixture:1b", digest: "abc" }] });
    assert.equal(parsed.pathname, "/api/chat");
    const id = parsed.hostname;
    active++; maxActive = Math.max(maxActive, active); calls.push(id);
    const count = (counts.get(id) ?? 0) + 1; counts.set(id, count);
    assert.equal(JSON.parse(options.body).model, "fixture:1b");
    return new Response(new ReadableStream({ start(controller) {
      setTimeout(() => {
        controller.enqueue(encoder.encode(ndjson(id === "b.invalid" && count === 2 ? '{"wrong":true}' : answer)));
        active--; controller.close();
      }, 2);
    } }));
  };
  try {
    const report = await runBenchmark({ config: { model: { ollama_tag: "fixture:1b" }, vendors: [vendor("a"), vendor("b")] }, out, samples: 2, warmups: 1, allowSmoke: true, lockPath: path.join(directory, "lock"), fetchImpl, onProgress: () => {} });
    assert.equal(maxActive, 1);
    assert.deepEqual(calls, ["a.invalid", "a.invalid", "a.invalid", "b.invalid", "b.invalid", "b.invalid"]);
    assert.equal(report.publication_status, "smoke_only");
    assert.equal(report.vendors[0].attempted, 2);
    assert.equal(report.vendors[0].warmup_discarded, 1);
    assert.equal(report.vendors[1].quality_failed, 1);
    assert.equal(report.vendors[1].success, 1);
    assert.equal(report.vendors[1].status, "failed");
    assert.ok(report.vendors[0].host_observations.sample_count >= 1);
    assert.ok(Object.hasOwn(report.vendors[0].host_observations, "peak_swap_growth_bytes"));
    assert.ok(Object.hasOwn(report.vendors[0].host_observations.sampled_pressure, "critical_ms"));
    const raw = (await readFile(path.join(out, "b", "trials.jsonl"), "utf8")).trim().split("\n").map(JSON.parse);
    assert.equal(raw.length, 3);
    assert.equal(raw[1].outcome, "quality_failed");
    assert.equal(JSON.parse(await readFile(path.join(out, "summary.json"), "utf8")).vendors.length, 2);
    assert.match(await readFile(path.join(out, "SUMMARY.md"), "utf8"), /raw admission|managed-task behavior/);
    assert.match(await readFile(path.join(out, "SUMMARY.md"), "utf8"), /Peak swap growth/);
    assert.match(await readFile(path.join(out, "summary.csv"), "utf8"), /sampled_critical_ms/);
    await assert.rejects(runBenchmark({ config: { model: {}, vendors: [vendor()] }, out, samples: 1, allowSmoke: true }), /Output already exists/);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("unavailable health retains its raw error and does not send inference", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "freellama-bench-health-"));
  const body = "{upstream unavailable}\n";
  let calls = 0;
  try {
    const out = path.join(directory, "out");
    const report = await runBenchmark({ config: { model: {}, vendors: [vendor()] }, out, samples: 1, warmups: 1, allowSmoke: true, lockPath: path.join(directory, "lock"), fetchImpl: async () => { calls++; return new Response(body, { status: 503 }); }, onProgress: () => {} });
    assert.equal(calls, 1);
    assert.equal(report.vendors[0].status, "unavailable");
    assert.equal(report.vendors[0].host_observations, null);
    const error = JSON.parse(await readFile(path.join(out, "ollama", "health-error.json"), "utf8"));
    assert.equal(error.observation.raw_response, body);
    assert.equal(error.observation.http_status, 503);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("failed warmup is visible and the next vendor can still run", async () => {
  const directory = await mkdtemp(path.join(os.tmpdir(), "freellama-bench-unavailable-"));
  const fetchImpl = async (url) => {
    const parsed = new URL(url);
    if (parsed.pathname === "/api/version") return Response.json({ version: "fixture-1" });
    if (parsed.pathname === "/api/tags") return Response.json({ models: [{ name: "fixture:1b" }] });
    return streamResponse([ndjson(parsed.hostname === "a.invalid" ? "wrong" : answer)]);
  };
  try {
    const report = await runBenchmark({ config: { model: {}, vendors: [vendor("a"), vendor("b")] }, out: path.join(directory, "out"), samples: 1, warmups: 1, allowSmoke: true, lockPath: path.join(directory, "lock"), fetchImpl, onProgress: () => {} });
    assert.equal(report.vendors[0].status, "failed");
    assert.match(report.vendors[0].error, /Warm-up/);
    assert.equal(report.vendors[0].attempted, 0);
    assert.equal(report.vendors[1].status, "complete");
  } finally { await rm(directory, { recursive: true, force: true }); }
});
