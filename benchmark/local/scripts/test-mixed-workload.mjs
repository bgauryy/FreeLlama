// Fixture and mock-server contracts for mixed-workload.mjs; no Ollama or live inference required.
import assert from "node:assert/strict";
import http from "node:http";
import { readFileSync } from "node:fs";
import { performance } from "node:perf_hooks";
import { afterEach, beforeEach, test } from "node:test";
import * as benchmark from "./mixed-workload.mjs";

const clone = (value) => structuredClone(value);

function health(holding = false) {
  return { status: "ok", admission: { resources: { holding, observation: {
    available_memory_bytes: 16 * 1024 ** 3, swap_out_pages: 10, load_average_one_minute: 0.5,
    logical_cpus: 8, thermal_throttled: false, memory_pressure: "normal" } } } };
}

const MODEL = { name: "fixture:exact", digest: "sha256:fixture", resident: false, execution: { placement: "gpu" } };

let server;
let state;

function sendJson(response, status, body) {
  const data = Buffer.from(JSON.stringify(body));
  response.writeHead(status, { "Content-Type": "application/json", "Content-Length": String(data.length) });
  response.end(data);
}

beforeEach(async () => {
  state = {
    requests: [], healthCalls: 0, holdAfter: null, delay: 0.01, refuse: false,
    refusalBody: { error: "queue admission refused" }, done: true, observation: {}, model: clone(MODEL),
  };
  server = http.createServer((request, response) => {
    response.on("error", () => {});
    if (request.method === "GET") {
      if (request.url.endsWith("/health")) {
        state.healthCalls += 1;
        const sample = health(state.holdAfter !== null && state.healthCalls >= state.holdAfter);
        Object.assign(sample.admission.resources.observation, state.observation);
        sendJson(response, 200, sample);
      } else {
        sendJson(response, 200, { models: [state.model] });
      }
      return;
    }
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => {
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      state.requests.push(body);
      setTimeout(() => {
        if (state.refuse) {
          sendJson(response, 503, state.refusalBody);
          return;
        }
        const text = body.messages.map((message) => message.content).join(" ");
        const answer = text.includes("CURRENT") ? "FL-7319" : text.includes("FL-2086") ? "FL-2086" : text.includes("owner=Rina") ? '{"count":7,"owner":"Rina"}' : "review";
        sendJson(response, 200, {
          route: { selected_model: MODEL.name },
          execution: { model_digest: MODEL.digest, lifecycle: { unload_requested: true }, observation: { status: "verified" } },
          response: { done: state.done, message: { content: answer } },
        });
      }, state.delay * 1000);
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
});

afterEach(async () => {
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
});

function runProbe(overrides = {}) {
  return benchmark.run({
    endpoint: `http://127.0.0.1:${server.address().port}`, model: MODEL.name, cpu_model: null,
    trials: 3, max_wall_seconds: 5, request_timeout_seconds: 1, max_output_tokens: 96,
    max_parallel: 2, include_compacted: false, smoke: false, context_tokens: 4096, ...overrides,
  });
}

const counts = (report) => JSON.parse(JSON.stringify(report.summary.status_counts));
const value = (item) => (item !== null && typeof item === "object" && "value" in item ? item.value : item);

test("complete mock run and fixed payload", async () => {
  const report = await runProbe({ include_compacted: true });
  assert.equal(report.complete, true);
  assert.equal(report.summary.correct, 15);
  assert.equal(state.requests.length, 15);
  for (const request of state.requests) {
    assert.equal(request.keep_alive, "0");
    assert.equal(request.model, MODEL.name);
    assert.equal(request.objective, "fastest");
    assert.equal(request.context_tokens, 4096);
    assert.equal(request.request_options.options.num_predict, 96);
  }
  assert.ok(report.tasks.some((task) => task.context.precompacted));
  assert.equal(benchmark.compare(report, report).verdict, "NO_MEASURED_IMPROVEMENT");
});

test("refusals reduce primary and correct fraction", async () => {
  state.refuse = true;
  const report = await runProbe();
  assert.equal(value(report.summary.correct_fraction), 0);
  assert.equal(value(report.summary.successful_correct_tasks_per_second), 0);
  assert.deepEqual(counts(report), { admission_refusal: 12 });
  assert.equal(benchmark.compare(report, report).verdict, "INSUFFICIENT_EVIDENCE");
});

test("resource refusal stops dispatch even with healthy cached sample", async () => {
  state.refuse = true;
  state.refusalBody = { error: "resource unavailable", code: "resource_admission_unavailable", resource_admission: { status: "held" } };
  const report = await runProbe();
  assert.equal(report.complete, false);
  assert.ok(state.requests.length <= 2);
  assert.ok(report.stop_reasons.includes("resource_admission_unavailable"));
  assert.ok((counts(report).admission_refusal ?? 0) > 0);
  assert.equal(value(report.summary.correct_fraction), 0);
});

test("incomplete response stops new dispatch", async () => {
  state.done = false;
  const report = await runProbe();
  assert.equal(report.complete, false);
  assert.ok(state.requests.length <= 2);
  assert.ok(report.stop_reasons.includes("request_completion_unknown"));
  assert.ok((counts(report).incomplete_response ?? 0) > 0);
});

test("context option is recorded, sent, and comparison frozen", async () => {
  const baseline = await runProbe({ context_tokens: 8192, include_compacted: true });
  assert.equal(baseline.workload_config.context_tokens, 8192);
  assert.ok(state.requests.every((request) => request.context_tokens === 8192));
  const candidate = clone(baseline);
  candidate.workload_config.context_tokens = 4096;
  assert.equal(benchmark.compare(baseline, candidate).verdict, "NOT_COMPARABLE");
});

test("context option rejects out of range without requests", async () => {
  for (const option of ["2047", "32769"]) {
    await assert.rejects(
      benchmark.main(["run", "--endpoint", `http://127.0.0.1:${server.address().port}`, "--model", MODEL.name, "--context-tokens", option, "--output", ".octocode/context-validation-never-written.json"]),
      (error) => error instanceof benchmark.CliExit && error.code === 2,
    );
  }
  assert.deepEqual(state.requests, []);
});

test("holding prevents all dispatch", async () => {
  state.holdAfter = 1;
  const report = await runProbe();
  assert.equal(report.complete, false);
  assert.deepEqual(state.requests, []);
  assert.ok(report.stop_reasons.includes("host_holding"));
  assert.equal(value(report.summary.correct_fraction), 0);
});

test("pressure after catalog prevents dispatch", async () => {
  state.holdAfter = 2;
  const report = await runProbe();
  assert.equal(report.complete, false);
  assert.deepEqual(state.requests, []);
});

test("wall deadline is absolute", async () => {
  state.delay = 0.5;
  const start = performance.now() / 1000;
  const report = await runProbe({ max_wall_seconds: 0.15, request_timeout_seconds: 1 });
  assert.ok(performance.now() / 1000 - start < 0.35);
  assert.ok(report.stop_reasons.includes("wall_deadline"));
  assert.ok(state.requests.length <= 2);
  assert.equal(report.complete, false);
});

test("request deadline stops new work", async () => {
  state.delay = 0.5;
  const report = await runProbe({ request_timeout_seconds: 0.08 });
  // A transport timeout also stops dispatch, even if it arrives just before the controller's
  // absolute deadline.
  assert.ok(state.requests.length <= 2);
  assert.equal(report.complete, false);
});

test("unknown thermal is honest and blocks comparison", async () => {
  state.observation.thermal_throttled = null;
  const report = await runProbe();
  assert.equal(report.complete, true);
  assert.ok(report.summary.unknown_metrics.includes("thermal_throttled"));
  assert.equal(benchmark.compare(report, report).verdict, "INSUFFICIENT_EVIDENCE");
});

test("missing or zero CPU denominator cannot hide high load", async () => {
  const baseline = await runProbe();
  for (const cpus of [null, 0]) {
    const candidate = clone(baseline);
    candidate.elapsed_seconds *= 0.8;
    for (const sample of candidate.health_samples) {
      Object.assign(sample.body.admission.resources.observation, { logical_cpus: cpus, load_average_one_minute: 100 });
    }
    candidate.summary = benchmark.summarize(candidate);
    assert.ok(candidate.summary.unknown_metrics.includes("logical_cpus"));
    assert.equal(benchmark.compare(baseline, candidate).verdict, "INSUFFICIENT_EVIDENCE");
  }
});

test("preexisting resident model is not unloaded", async () => {
  state.model.resident = true;
  const report = await runProbe();
  assert.equal(report.complete, false);
  assert.deepEqual(state.requests, []);
});

test("CPU requires existing assignment", async () => {
  const report = await runProbe({ cpu_model: MODEL.name });
  assert.equal(report.complete, false);
  assert.deepEqual(state.requests, []);
});

test("exact model and digest required", async () => {
  for (const change of [{ model: "fixture" }, {}]) {
    if (Object.keys(change).length === 0) state.model.digest = null;
    const report = await runProbe(change);
    assert.equal(report.complete, false);
  }
  assert.deepEqual(state.requests, []);
});

test("comparison guards smoke, partial, forged, and changed budget", async () => {
  const baseline = await runProbe();
  for (const mutate of [
    (report) => { report.workload_config.smoke = true; },
    (report) => { report.tasks.pop(); },
    (report) => { report.tasks[0].correct = false; },
    (report) => { report.summary.correct = 900; },
    (report) => { report.case_hash = "wrong"; },
    (report) => { report.models.primary.digest = null; },
  ]) {
    const candidate = clone(baseline);
    mutate(candidate);
    assert.equal(benchmark.compare(baseline, candidate).verdict, "INSUFFICIENT_EVIDENCE");
  }
  const candidate = clone(baseline);
  candidate.workload_config.max_wall_seconds = 6;
  assert.equal(benchmark.compare(baseline, candidate).verdict, "NOT_COMPARABLE");
});

test("comparable improvement is never ACCEPT", async () => {
  const baseline = await runProbe();
  const candidate = clone(baseline);
  candidate.elapsed_seconds *= 0.8;
  candidate.summary = benchmark.summarize(candidate);
  const result = benchmark.compare(baseline, candidate);
  assert.equal(result.verdict, "MEASURED_IMPROVEMENT");
  assert.ok(result.promotion.includes("No ACCEPT"));
});

test("grader rejects wrong types and extra fields", () => {
  const item = benchmark.loadSuite().cases[2];
  assert.equal(benchmark.grade(item, '{"count":7,"owner":"Rina"}'), true);
  for (const text of ['{"count":"7","owner":"Rina"}', '{"count":7,"owner":"Rina","extra":1}', "```json\n{}\n```", "I cannot help"]) {
    assert.equal(benchmark.grade(item, text), false);
  }
});

test("malformed response and health", async () => {
  const item = benchmark.loadSuite().cases[0];
  for (const body of [null, [], { response: [] }, { response: {}, execution: {}, route: [] }]) {
    const result = benchmark.classify(item, { http_status: 200, body }, MODEL);
    assert.equal(result.status, "malformed_metadata");
  }
  state.observation.available_memory_bytes = "lots";
  const report = await runProbe();
  assert.ok(report.stop_reasons.includes("health_metadata_invalid"));
  assert.deepEqual(state.requests, []);
});

test("pressure thresholds", () => {
  const guards = JSON.parse(readFileSync(benchmark.KPI, "utf8")).guardrails;
  for (const [patch, reason] of [[{ swap_out_pages: 11 }, "swap_out_growth"], [{ available_memory_bytes: 1 }, "available_memory_below_guard"], [{ load_average_one_minute: 100 }, "load_above_guard"], [{ thermal_throttled: true }, "thermal_throttling"]]) {
    const body = health();
    Object.assign(body.admission.resources.observation, patch);
    assert.ok(benchmark.pressureReasons({ http_status: 200, body }, 10, guards).includes(reason));
  }
});

test("unavailable required memory prevents dispatch", async () => {
  state.observation.available_memory_bytes = null;
  const report = await runProbe();
  assert.ok(report.stop_reasons.includes("required_available_memory_unavailable"));
  assert.deepEqual(state.requests, []);
});
