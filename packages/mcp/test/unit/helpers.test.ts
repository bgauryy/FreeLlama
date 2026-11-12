import { describe, expect, it } from "vitest";
import {
  configuredExternalCost,
  decisionAnswerText,
  costTelemetry,
  errorResult,
  refusalLine,
  parsedResult,
  jobsText,
  lifecycleText,
  RESULT_TEXT_LIMIT,
  structuredResult,
  taskAnswerText,
  batchAnswerText,
  summarizeEmbeddings,
  withRequiredCapability,
} from "../../src/helpers.js";
import { REPO_ROOT } from "../../src/config.js";
import { extractExistingWorkspacePath, parseAdapterResult } from "../../src/delegate.js";
import { summarizeOllamaPullStream } from "../../src/tools/ollama-lifecycle.js";

describe("refusalLine nested errors", () => {
  it.each(["null", "[]", "42", "true", '"unstructured"'])("falls back for non-object JSON %s", (body) => {
    expect(refusalLine(body, 400)).toBe("HTTP 400");
  });
  it.each(["", "  \t\n"])("falls back for blank nested message %j", (message) => {
    expect(refusalLine(JSON.stringify({ error: { message, secret: "do not expose" } }), 400)).toBe("HTTP 400");
  });
  it("preserves only the nested message and existing fallback and formatting", () => {
    expect(refusalLine(JSON.stringify({ error: { message: "voice unavailable; available voices: default", secret: "do not expose" }, code: "bad_voice" }), 400))
      .toBe("voice unavailable; available voices: default [bad_voice]");
    expect(refusalLine(JSON.stringify({ error: { message: 42, secret: "do not expose" } }), 400)).toBe("HTTP 400");
    expect(refusalLine(JSON.stringify({ error: { secret: "do not expose" } }), 400)).toBe("HTTP 400");
    expect(refusalLine(JSON.stringify({ error: "" }), 400)).toBe("");
    expect(refusalLine(JSON.stringify({ error: "  " }), 400)).toBe("  ");
    expect(refusalLine(JSON.stringify({ error: "existing failure", code: "bad" }), 400)).toBe("existing failure [bad]");
    expect(refusalLine("x".repeat(500), 500)).toHaveLength(400);
    expect(refusalLine(JSON.stringify({ error: { message: "x".repeat(500) } }), 400)).toHaveLength(400);
    expect(refusalLine(JSON.stringify({ error: { message: " ".repeat(400) + "x" } }), 400)).toBe("HTTP 400");
  });
});

describe("extractExistingWorkspacePath", () => {
  it("extracts an existing workspace-relative file from a Bash command", () => {
    expect(
      extractExistingWorkspacePath("grep -n name packages/mcp/package.json", REPO_ROOT),
    ).toBe("packages/mcp/package.json");
  });

  it("does not turn an outside path or a find pattern into a citation", () => {
    expect(extractExistingWorkspacePath("grep root /etc/passwd", REPO_ROOT)).toBeNull();
    expect(extractExistingWorkspacePath("find . -name package.json", REPO_ROOT)).toBeNull();
  });
});

describe("decision answers", () => {
  it("print one line per question, using the fields /v1/systemone returns", () => {
    expect(decisionAnswerText({
      kind: { type: "choice", choice: "bug", probabilities: { bug: 0.978, feature: 0.022 }, confidence: 0.89 },
      refund: { type: "noul", noul: 0.9989 },
      urgency: { type: "score", score: 0.83, legend: { "0": "Routine", "1": "Soon", "2": "Immediate" },
        probabilities: { "0": 0.3, "1": 0.57, "2": 0.13 }, confidence: 0.4 },
    })).toBe([
      "kind: bug (p 0.98, confidence 0.89)",
      "refund: true (p(true) 1.00)",
      'urgency: 0.83 of 0-2, likeliest "Soon" (confidence 0.40)',
    ].join("\n"));
  });

  it("is the answer text of a decision task", () => {
    expect(taskAnswerText({ route: { selected_model: "d:1" }, response: { answers: { a: { type: "noul", noul: 0.2 } } } }))
      .toBe("[d:1] a: false (p(true) 0.20)");
  });
});

describe("withRequiredCapability", () => {
  it("adds a tool requirement without dropping or duplicating existing capabilities", () => {
    expect(withRequiredCapability(["vision", "tools"], "tools")).toEqual(["vision", "tools"]);
    expect(withRequiredCapability(undefined, "tools")).toEqual(["tools"]);
  });
});

describe("costTelemetry", () => {
  it("reports observed local usage without inventing an external bill", () => {
    expect(costTelemetry({ inputTokens: 100, outputTokens: 20 })).toMatchObject({
      local: { inputTokens: 100, outputTokens: 20, totalTokens: 120 },
      externalEquivalent: null,
    });
  });

  it("calculates only the configured same-token external equivalent", () => {
    expect(costTelemetry(
      { inputTokens: 1_000_000, outputTokens: 500_000 },
      { model: "external-large", input: 1.25, output: 10 },
    )).toMatchObject({
      externalEquivalent: { model: "external-large", inputUsd: 1.25, outputUsd: 5, totalUsd: 6.25 },
    });
  });

  it("refuses to estimate an external equivalent when Ollama omits token counts", () => {
    expect(costTelemetry(
      { inputTokens: null, outputTokens: 2 },
      { model: "external-large", input: 1.25, output: 10 },
    ).externalEquivalent).toBeNull();
  });
});

describe("configuredExternalCost", () => {
  it("requires a complete operator-owned rate card", () => {
    expect(() => configuredExternalCost({ FREELLAMA_EXTERNAL_COST_MODEL: "external" })).toThrow(/requires/);
  });

  it("reads a complete USD-per-million rate card", () => {
    expect(configuredExternalCost({
      FREELLAMA_EXTERNAL_COST_MODEL: "external",
      FREELLAMA_EXTERNAL_COST_INPUT_USD_PER_M: "1.25",
      FREELLAMA_EXTERNAL_COST_OUTPUT_USD_PER_M: "10",
    })).toEqual({ model: "external", input: 1.25, output: 10 });
  });
});

describe("parseAdapterResult", () => {
  it("parses a complete adapter result", () => {
    const full = parseAdapterResult(
      JSON.stringify({
        final_answer: "ok",
        tool_calls: [{ raw_name: "shell", status: "ok", arguments: { command: "ls" } }],
        usage: { input_tokens: 1, output_tokens: 2 },
      }),
    );
    expect(full.final_answer).toBe("ok");
    expect(full.tool_calls).toHaveLength(1);
  });

  it("preserves typed context-management receipts", () => {
    const parsed = parseAdapterResult(
      JSON.stringify({
        final_answer: "ok",
        model_metadata: {
          context_management: {
            token_counting: "local_chars_per_token",
            estimate_scale: 1.2,
            calibration_samples: 3,
            pinned_overflow: "error",
            compactions: 1,
          },
        },
      }),
    );
    expect(parsed.model_metadata?.context_management?.pinned_overflow).toBe("error");
    expect(parsed.model_metadata?.context_management?.calibration_samples).toBe(3);
  });

  it("defaults missing tool_calls to [] rather than throwing later", () => {
    const partial = parseAdapterResult(JSON.stringify({ final_answer: "only answer" }));
    expect(partial.tool_calls).toEqual([]);
    expect(partial.usage).toEqual({});
  });

  it("fails closed on a JSON object without final_answer", () => {
    expect(() => parseAdapterResult("{}")).toThrow();
  });

  it("fails closed on non-JSON input", () => {
    expect(() => parseAdapterResult("not json")).toThrow();
  });
});

describe("summarizeOllamaPullStream", () => {
  it("reports success for an already-installed tag", () => {
    const result = summarizeOllamaPullStream('{"status":"pulling manifest"}\n{"status":"success"}\n');
    expect(result.status).toBe("success");
    expect((result.progress as Record<string, unknown>).lastStatus).toBe("success");
  });

  it("computes percent from the last byte snapshot", () => {
    const result = summarizeOllamaPullStream(
      '{"status":"pulling sha256:abc"}\n' +
        '{"status":"downloading","digest":"sha256:abc","total":1000,"completed":250}\n' +
        '{"status":"downloading","digest":"sha256:abc","total":1000,"completed":1000}\n' +
        '{"status":"success"}\n',
    );
    const progress = result.progress as Record<string, unknown>;
    expect(progress.percent).toBe(100);
    expect(progress.events).toBe(4);
  });

  it("reports mid-download percent when the stream ends early", () => {
    const result = summarizeOllamaPullStream(
      '{"status":"downloading","digest":"sha256:abc","total":2000,"completed":500}\n',
    );
    expect((result.progress as Record<string, unknown>).percent).toBe(25);
  });

  it("still parses a single JSON object (stream:false shape)", () => {
    expect(summarizeOllamaPullStream('{"status":"success"}').status).toBe("success");
  });

  it("classifies a stream that ends in an Ollama error event as status 'error'", () => {
    // Ollama answers HTTP 200 and reports failures as an {"error": ...} NDJSON line — verified
    // live: a bogus tag yields `{"status":"pulling manifest"}\n{"error":"pull model manifest: ..."}`.
    // Without this classification a failed pull came back success-shaped.
    const failed = summarizeOllamaPullStream(
      '{"status":"pulling manifest"}\n{"error":"pull model manifest: file does not exist"}\n',
    );
    expect(failed.status).toBe("error");
    expect(failed.error).toBe("pull model manifest: file does not exist");
  });

  it("returns 'empty' for whitespace-only input and 'unparsed' for noise", () => {
    expect(summarizeOllamaPullStream("  \n ").status).toBe("empty");
    expect(summarizeOllamaPullStream("plain text, not json").status).toBe("unparsed");
  });
});

describe("result shapes", () => {
  it("structuredResult keeps the object canonical and mirrors it as compact JSON text", () => {
    const result = structuredResult({ hello: "world", n: [1, 2] });
    expect(result.structuredContent).toEqual({ hello: "world", n: [1, 2] });
    expect(result.content[0].text).toBe('{"hello":"world","n":[1,2]}');
  });

  it("structuredResult keeps an explicit summary as the text", () => {
    expect(structuredResult({ summary: " Done. ", x: 1 }).content[0].text).toBe("Done.");
  });

  it("structuredResult caps the JSON text and points at structuredContent", () => {
    const big = { blob: "x".repeat(RESULT_TEXT_LIMIT * 2) };
    const { content, structuredContent } = structuredResult(big);
    expect(structuredContent).toEqual(big);
    expect(content[0].text.startsWith('{"blob":"xxx')).toBe(true);
    expect(content[0].text).toMatch(/JSON truncated at 4000 of \d+ characters.*structuredContent/s);
    expect(content[0].text.length).toBeLessThan(RESULT_TEXT_LIMIT + 400);
  });

  const diagnostics = { blob: "x".repeat(20_000), nested: { more: "y".repeat(5_000) } };
  const finishedJob = (result: Record<string, unknown>) => ({ job: { id: "29953a64-0000-4000-8000-000000000000", status: "completed", task: "completion",
    selected_model: "lfm3:1.7b", reason: "completed", result: { route: { selected_model: "lfm3:1.7b" }, execution: diagnostics, ...result } } });

  it("a deferred job get shows its answer in the text, ahead of any diagnostics", () => {
    const text = jobsText(finishedJob({ response: { message: { role: "assistant", content: "The answer is 42." } } }))!;
    expect(text.split("\n")[0]).toBe("29953a64-0000-4000-8000-000000000000  completion  lfm3:1.7b  completed");
    expect(text).toContain("The answer is 42.");
    expect(text.length).toBeLessThan(500);
  });

  it("a job get shows a nearest ranking, decision answers, or the error", () => {
    const ranked = jobsText(finishedJob({ response: { embeddings_omitted: { count: 3 } },
      nearest: { mode: "query", matches: [{ index: 1, label: "b", score: 0.9 }, { index: 0, label: "a", score: 0.5 }] } }))!;
    expect(ranked).toContain("0.900  b\n0.500  a");
    const decided = jobsText(finishedJob({ response: { answers: { kind: { type: "choice", choice: "bug", probabilities: { bug: 0.97 } } } } }))!;
    expect(decided).toContain("kind: bug (p 0.97");
    const failed = jobsText({ job: { id: "j", status: "failed", task: "completion", error: { error: "model not found", code: "x" } } })!;
    expect(failed).toContain("error: model not found [x]");
  });

  it("a job list is one line per job", () => {
    const text = jobsText({ jobs: [
      { id: "a", task: "completion", selected_model: "m:1", status: "running", reason: "generating" },
      { id: "b", task: "embedding", requested_model: "e:1", status: "queued", reason: "discovering_model" },
    ] })!;
    expect(text).toBe("a  completion  m:1  running (generating)\nb  embedding  e:1  queued (discovering_model)");
    expect(jobsText({ jobs: [] })).toBe("no jobs");
  });

  it("warm and unload are one short line; a deferred warm names its job", () => {
    const warm = lifecycleText("warm", "lfm3:1.7b", { route: { selected_model: "lfm3:1.7b" }, execution: { ...diagnostics, keep_alive: "5m" },
      warm: { loaded: true }, metrics: { load_duration_ns: 8.9e9 }, admission: { total_wait_ms: 12 } })!;
    expect(warm).toBe("lfm3:1.7b: resident, keep_alive 5m, load 8.9 s, waited 12 ms");
    expect(warm.length).toBeLessThan(500);
    const adaptive = lifecycleText("warm", "m:1", { execution: { keep_alive: { mode: "adaptive", value: "31s", bounds: { max_seconds: 900 }, pressure_reasons: ["x"] } }, warm: { loaded: true } })!;
    expect(adaptive).toBe("m:1: resident, keep_alive 31s (adaptive)");
    expect(lifecycleText("unload", "m:1", { model: "m:1", unloaded: true, via: "freellama_serve", unload_ms: 40 })).toBe("m:1: unloaded, via freellama_serve, 40 ms");
    expect(lifecycleText("warm", "m:1", { deferred: true, job: { id: "j", status: "queued", task: "completion" } })).toContain("deferred: j  completion");
  });

  it("parsedResult parses upstream JSON once", () => {
    const result = parsedResult('{"x": 1}');
    expect("structuredContent" in result && result.structuredContent).toEqual({ x: 1 });
  });

  it("parsedResult turns invalid JSON into an error result, not a throw", () => {
    const result = parsedResult("not json at all");
    expect("isError" in result && result.isError).toBe(true);
    expect(result.content[0].text).toMatch(/not valid JSON/);
  });

  it("parsedResult rejects non-object JSON", () => {
    const result = parsedResult("[1,2,3]");
    expect("isError" in result && result.isError).toBe(true);
  });

  it("errorResult stringifies non-Error values", () => {
    expect(errorResult("boom").content[0].text).toBe("boom");
    expect(errorResult(new Error("bang")).content[0].text).toBe("bang");
  });

  it("makes an unavailable managed serve actionable without rewriting other errors", () => {
    const unavailable = errorResult(
      new Error("error sending request for url (http://127.0.0.1:11435/_freellama/v1/models): connection refused"),
    );
    expect(unavailable.content[0].text).toContain("Start it with `freellama serve`");
    expect(unavailable.content[0].text).toContain("FREELLAMA_SERVE_ENDPOINT");
    expect(errorResult(new Error("connection refused at http://127.0.0.1:11434/api/tags")).content[0].text).not.toContain(
      "FreeLlama managed serve",
    );
  });

  it("recognizes the configured serve address even without the managed API path", () => {
    const unavailable = errorResult(new Error("tcp connect error: 127.0.0.1:11435: connection refused"));
    expect(unavailable.content[0].text).toContain("Start it with `freellama serve`");
  });
});

describe("summarizeEmbeddings", () => {
  it("withholds the matrix, keeping count/dimensions/preview", () => {
    const vector = Array.from({ length: 768 }, (_, i) => i / 768);
    const summarized = summarizeEmbeddings({ response: { model: "m", embeddings: [vector] } });
    const response = summarized?.response as Record<string, unknown>;
    expect(response.embeddings).toBeUndefined();
    const omitted = response.embeddings_omitted as Record<string, unknown>;
    expect(omitted.count).toBe(1);
    expect(omitted.dimensions).toBe(768);
    expect(omitted.preview).toEqual(vector.slice(0, 8));
  });

  it("passes non-embedding payloads through untouched (null)", () => {
    expect(summarizeEmbeddings({ response: { message: "hi" } })).toBeNull();
    expect(summarizeEmbeddings({ response: { embeddings: [] } })).toBeNull();
    expect(summarizeEmbeddings({})).toBeNull();
  });
});

describe("task answer text", () => {
  it("puts the chat answer and selected model into TextContent", () => {
    const payload = {
      route: { selected_model: "qwen3:8b" },
      response: { model: "qwen3:8b", message: { role: "assistant", content: "  42  " } },
    };
    const result = structuredResult(payload, { text: taskAnswerText(payload) });
    expect(result.content[0].text).toBe("[qwen3:8b] 42");
    expect(result.structuredContent).toBe(payload);
  });

  it("adds one lifecycle clause for a cold load or evictions, and none for a warm call", () => {
    const answer = (metrics: Record<string, unknown>, evicted?: string[]) => taskAnswerText({
      route: { selected_model: "gpt-oss:20b" }, metrics,
      execution: { memory_reservation: evicted ? { evicted_idle_models: evicted } : {} },
      response: { message: { content: "hi" } },
    });
    expect(answer({ load_duration_ns: 8_900_000_000 }, ["glm-ocr:latest"])).toBe("[gpt-oss:20b · loaded 8.9 s, evicted glm-ocr:latest] hi");
    expect(answer({ load_duration_ns: 1_200_000_000 })).toBe("[gpt-oss:20b · loaded 1.2 s] hi");
    expect(answer({ load_duration_ns: 40_000_000 }, ["a:1", "b:2"])).toBe("[gpt-oss:20b · evicted a:1, b:2] hi");
    expect(answer({ load_duration_ns: 40_000_000 })).toBe("[gpt-oss:20b] hi");
  });

  it("includes tool calls and returns a long answer whole", () => {
    const calls = taskAnswerText({ response: { message: { content: "", tool_calls: [{ function: { name: "f" } }] } } });
    expect(calls).toContain('tool_calls: [{"function":{"name":"f"}}]');
    const content = "x".repeat(100_000);
    expect(taskAnswerText({ response: { message: { content } } })).toBe(content);
  });

  it("shows one copy of a looped answer and keeps the full text out of the answer only", () => {
    const content = "Total: $1.00\n```" + "\n```".repeat(50);
    const text = taskAnswerText({
      response: { message: { content } },
      output_check: { repetition_loop: true, unique_prefix_chars: 16 },
    });
    expect(text).toMatch(/^Total: \$1\.00\n```\n\[freellama: \d+ repeated characters omitted here \(full text in structuredContent\)/);
    expect(taskAnswerText({ response: { message: { content } } })).toBe(content.trim());
  });

  it("describes withheld embeddings instead of dumping vectors", () => {
    const text = taskAnswerText({ response: { embeddings_omitted: { count: 3 } } });
    expect(text).toContain("3 vectors");
    expect(text).toContain("returnEmbeddings:true");
  });

  it("says when a thinking model spent its whole output budget before answering", () => {
    const text = taskAnswerText({ response: { message: { content: "", thinking: "Let me think..." }, eval_count: 200 } });
    expect(text).toMatch(/empty answer: the model spent its output \(200 tokens\) thinking; pass think:false or raise options.num_predict/);
    expect(taskAnswerText({ response: { message: { content: "" } } })).toBe("(empty response)");
  });

  it("labels each batch item, including failures", () => {
    const text = batchAnswerText({
      results: [
        { id: "a", ok: true, response: { response: { message: { content: "one" } } } },
        { id: "b", ok: false, error: { message: "queue full" } },
      ],
    });
    expect(text).toContain("## a\none");
    expect(text).toContain("## b (failed)");
    expect(text).toContain("queue full");
  });
});

describe("errorResult recovery hints", () => {
  it("tells the caller Ollama itself is down when serve could not reach it", () => {
    const result = errorResult(new Error("error sending request for url (http://127.0.0.1:11434/api/tags)"));
    expect(result.content[0].text).toMatch(/Ollama is not running/);
  });
});
