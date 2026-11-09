import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { editBlocks, resultChecks, withChecks } from "../../src/result-checks.js";

const dir = mkdtempSync(path.join(tmpdir(), "freellama-checks-"));
const source = path.join(dir, "config.ts");
writeFileSync(source, "export const port = 8080;\nexport const host = \"localhost\";\nexport const port2 = 8080;\n");
const read = { files_read: [{ path: source, bytes: 10, role: "prompt" }] };

describe("result-derived checks", () => {
  it("names uncertain decision answers instead of a fixed spot-check line", () => {
    const checks = resultChecks({ answer: "", structured: { response: { answers: {
      bug: { type: "noul", noul: 0.52 }, kind: { type: "choice", choice: "feature", probabilities: { feature: 0.4, bug: 0.35 } }, ok: { type: "noul", noul: 0.97 },
    } } } });
    expect(checks).toEqual(["uncertain: bug (p 0.52), kind (p 0.40); decide these yourself or rephrase the question"]);
    expect(resultChecks({ answer: "", structured: { response: { answers: { a: { type: "noul", noul: 0.02 } } } } })[0]).toMatch(/outside 0.35-0.65/);
  });

  it("asks to confirm decider answers under its documented 0.9 confidence", () => {
    const checks = resultChecks({ answer: "", structured: { response: { answers: {
      kind: { type: "choice", choice: "bug", probabilities: { bug: 0.95 }, confidence: 0.79 },
      urgency: { type: "score", score: 1.2, confidence: 0.93 },
    } } } });
    expect(checks).toEqual(["confidence under 0.9: kind (0.79); the decider is right about 95% of the time only at 0.9 or more, so confirm these"]);
  });

  it("flags close and weak nearest matches and likely duplicate pairs", () => {
    expect(resultChecks({ answer: "", structured: { nearest: { mode: "query", matches: [{ score: 0.71, label: "a" }, { score: 0.705, label: "b" }] } } }))
      .toEqual([expect.stringMatching(/top two are within 0.02 \(0.710 vs 0.705\): read both/)]);
    expect(resultChecks({ answer: "", structured: { nearest: { mode: "query", matches: [{ score: 0.31, label: "a" }] } } })[0]).toMatch(/weak/);
    expect(resultChecks({ answer: "", structured: { nearest: { mode: "pairs", pairs: [{ score: 0.93 }, { score: 0.7 }] } } })[0]).toMatch(/1 pair\(s\) at 0.90/);
  });

  it("checks each SEARCH block against the file that was sent", () => {
    const answer = "config.ts\n<<<<<<< SEARCH\nexport const host = \"localhost\";\n=======\nexport const host = env.HOST;\n>>>>>>> REPLACE\n\nconfig.ts\n<<<<<<< SEARCH\n= 8080;\n=======\n= 9090;\n>>>>>>> REPLACE";
    expect(editBlocks(answer).map((block) => block.path)).toEqual(["config.ts", "config.ts"]);
    expect(resultChecks({ job: "coding", answer, structured: read })).toEqual(["block 2's SEARCH matches 2 times in config.ts: it is not unique"]);
    expect(resultChecks({ job: "coding", answer, structured: {} })[0]).toMatch(/unchecked: no file was sent/);
  });

  it("reads the path of a fenced or decorated edit block", () => {
    const block = "<<<<<<< SEARCH\nexport const host = \"localhost\";\n=======\nexport const host = env.HOST;\n>>>>>>> REPLACE";
    const answers = [
      `config.ts\n\`\`\`ts\n${block}\n\`\`\``,
      `**config.ts**\n${block}`,
      `\`config.ts\`:\n\`\`\`typescript\n${block}\n\`\`\``,
      `### config.ts\n${block}`,
    ];
    for (const answer of answers) {
      expect(editBlocks(answer).map((edit) => edit.path), answer).toEqual(["config.ts"]);
      expect(resultChecks({ job: "coding", answer, structured: read }), answer).toEqual(["all 1 SEARCH block(s) match exactly once; apply them, then run the tests"]);
    }
    // A fence with no path above it names no file.
    expect(editBlocks(`\`\`\`\n${block}`).map((edit) => edit.path)).toEqual([""]);
  });

  it("lists numbers and quoted names a summary has that its source does not", () => {
    expect(resultChecks({ answer: "- port 8080\n- host `localhost`\n- timeout 30 s", structured: read }))
      .toEqual(["not in the source: 30; verify these before reusing them"]);
  });

  it("explains an empty or looping read", () => {
    expect(resultChecks({ job: "ocr", task: "vision", answer: "[Non-Text]", structured: {} })[0]).toMatch(/blank or transparent/);
    expect(resultChecks({ task: "vision", answer: "x", structured: { output_check: { repetition_loop: true } } })[0]).toMatch(/looped/);
    // answer-image's "not visible" is an answer about the content, not a blank image.
    expect(resultChecks({ job: "image-recognition", task: "vision", answer: "not visible", structured: {} })).toEqual([
      "the model says the image does not show it: crop or enlarge the region, or ask about something visible",
    ]);
  });

  it("checks tool calls against the tools that were sent", () => {
    const tools = [{ type: "function", function: { name: "get_temperature", parameters: {
      type: "object", required: ["city"], properties: { city: { type: "string" }, unit: { type: "string", enum: ["C", "F"] }, days: { type: "integer" } },
    } } }];
    const answered = (...calls: unknown[]) => ({ answer: `tool_calls: ${JSON.stringify(calls)}`, structured: { response: { message: { role: "assistant", content: "", tool_calls: calls } } } });
    const call = (name: string, args: unknown) => ({ function: { name, arguments: args } });
    expect(resultChecks({ ...answered(call("get_temperature", { city: "Paris", unit: "C" })), tools })[0]).toMatch(/^1 tool call\(s\) match the tool schemas: run them, then send each result back as \{role:"tool", tool_name, content\}/);
    // Ollama returns arguments as an object; a JSON string is read too.
    expect(resultChecks({ ...answered(call("get_temperature", JSON.stringify({ city: "Paris" }))), tools })[0]).toMatch(/^1 tool call\(s\) match/);
    expect(resultChecks({ ...answered(call("get_weather", { city: "Paris" }), call("get_temperature", { unit: "K", days: "3", country: "FR" })), tools })).toEqual([
      "call 1 (get_weather) is not one of the tools sent (get_temperature)",
      "call 2 (get_temperature) lacks required city",
      "call 2 (get_temperature) passes country, which the tool does not declare",
      'call 2 (get_temperature): unit is "K", not one of ["C","F"]',
      'call 2 (get_temperature): days is "3", not "integer"',
      "fix or refuse these calls before running any; a rerun often corrects them",
    ]);
    // With no tools sent, a tool call is the model inventing one.
    expect(resultChecks(answered(call("read_file", { path: "a.ts" })))[0]).toMatch(/tool call instead of text/);
  });

  it("appends check lines to the answer and keeps them in structuredContent", () => {
    const result = withChecks({ content: [{ type: "text", text: "[m] - timeout 30 s" }], structuredContent: { ...read } }, undefined, "completion");
    expect(result.content[0]!.text).toMatch(/\ncheck: not in the source: 30/);
    expect((result.structuredContent as Record<string, unknown>).checks).toHaveLength(1);
  });
});
