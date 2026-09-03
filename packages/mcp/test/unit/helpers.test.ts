import { describe, expect, it } from "vitest";
import {
  belowConfidence,
  canonicalTaskKind,
  clipText,
  configuredExternalCost,
  costTelemetry,
  errorResult,
  parseAdapterResult,
  parsedResult,
  serialize,
  structuredResult,
  summarizeEmbeddings,
  summarizeOllamaPullStream,
  extractExistingWorkspacePath,
  withRequiredCapability,
} from "../../src/helpers.js";
import { REPO_ROOT } from "../../src/config.js";

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

describe("managed task aliases", () => {
  it("normalizes the natural code-review alias at the MCP boundary", () => {
    expect(canonicalTaskKind("code_review")).toBe("coding");
    expect(canonicalTaskKind("code_repair")).toBe("code_repair");
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
            token_counting: "model_calibrated_estimate",
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

describe("clipText", () => {
  it("returns short text unchanged", () => {
    expect(clipText("short", 100)).toBe("short");
  });

  it("keeps both ends and states how much was dropped", () => {
    const long = "A".repeat(100) + "MIDDLE" + "Z".repeat(100);
    const clipped = clipText(long, 60);
    expect(clipped.length).toBeLessThanOrEqual(60);
    expect(clipped.startsWith("A")).toBe(true);
    expect(clipped.endsWith("Z")).toBe(true);
    expect(clipped).toMatch(/more chars/);
  });

  it("falls back to a plain head slice when the limit can't fit the marker", () => {
    expect(clipText("ABCDEFGHIJ", 3)).toBe("ABC");
  });
});

describe("serialize", () => {
  it("pretty-prints small payloads", () => {
    expect(serialize({ a: 1 })).toBe('{\n  "a": 1\n}');
  });

  it("goes compact once the payload is large", () => {
    const big = { xs: Array.from({ length: 3000 }, (_, i) => i) };
    expect(serialize(big)).not.toContain("\n");
  });
});

describe("result shapes", () => {
  it("structuredResult keeps the object canonical and emits a compact text cue", () => {
    const result = structuredResult({ hello: "world" });
    expect(result.structuredContent).toEqual({ hello: "world" });
    expect(result.content[0].text).toBe("Structured result available (hello).");
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
});

describe("belowConfidence", () => {
  const lowDecision = {
    confidence: "low",
    evidence: "capability_metadata_only",
    selected_model: "some-model",
    reasons: ["only_candidate"],
  };

  it("passes when no minimum is requested", () => {
    expect(belowConfidence(lowDecision, undefined)).toBeNull();
  });

  it("passes when the decision meets the minimum", () => {
    expect(belowConfidence(lowDecision, "low")).toBeNull();
    expect(belowConfidence({ ...lowDecision, confidence: "medium" }, "medium")).toBeNull();
  });

  it("fails closed below the minimum, naming the rejected model", () => {
    const refusal = belowConfidence(lowDecision, "medium");
    expect(refusal?.isError).toBe(true);
    expect(refusal?.content[0].text).toMatch(/some-model/);
    expect(refusal?.content[0].text).toMatch(/fail-closed refusal/);
  });

  it("ranks an unknown grade below 'low' rather than failing open", () => {
    const refusal = belowConfidence({ ...lowDecision, confidence: "banana" }, "low");
    expect(refusal?.isError).toBe(true);
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
