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

  it("lists numbers and quoted names a summary has that its source does not", () => {
    expect(resultChecks({ answer: "- port 8080\n- host `localhost`\n- timeout 30 s", structured: read }))
      .toEqual(["not in the source: 30; verify these before reusing them"]);
  });

  it("explains an empty or looping read", () => {
    expect(resultChecks({ job: "ocr", task: "vision", answer: "[Non-Text]", structured: {} })[0]).toMatch(/blank or transparent/);
    expect(resultChecks({ task: "vision", answer: "x", structured: { output_check: { repetition_loop: true } } })[0]).toMatch(/looped/);
  });

  it("appends check lines to the answer and keeps them in structuredContent", () => {
    const result = withChecks({ content: [{ type: "text", text: "[m] - timeout 30 s" }], structuredContent: { ...read } }, undefined, "completion");
    expect(result.content[0]!.text).toMatch(/\ncheck: not in the source: 30/);
    expect((result.structuredContent as Record<string, unknown>).checks).toHaveLength(1);
  });
});
