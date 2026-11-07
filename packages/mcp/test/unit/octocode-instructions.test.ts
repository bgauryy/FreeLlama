import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { REPO_ROOT } from "../../src/config.js";
import { OCTOCODE_GITHUB_INSTRUCTIONS, OCTOCODE_LOCAL_INSTRUCTIONS } from "../../src/research/octocode-instructions.js";
import { GITHUB_TOOLS, OCTOCODE_TOOLS } from "../../src/research/octocode-agent.js";

const tools = (text: string) => [...text.matchAll(/^(\w+): /gm)].map((match) => match[1]).filter((name) => /^[a-z]+[A-Z]/.test(name!));

describe.each([
  ["LOCAL.md", OCTOCODE_LOCAL_INSTRUCTIONS, OCTOCODE_TOOLS],
  ["GITHUB.md", OCTOCODE_GITHUB_INSTRUCTIONS, GITHUB_TOOLS],
])("octocode instructions %s", (file, text, allowed) => {
  it("match the docs page and stay within 30 lines", () => {
    expect(readFileSync(path.join(REPO_ROOT, "docs/octocode", file), "utf8")).toBe(text);
    expect(text.trimEnd().split("\n").length).toBeLessThanOrEqual(30);
  });

  it("explain exactly the tools the adapter allows", () => {
    expect(new Set(tools(text))).toEqual(allowed);
  });

  it("give examples that parse as JSON", () => {
    const examples = text.split("\n").map((line) => line.match(/^\s+(\{.*?\})(\s\s.*)?$/)?.[1]).filter(Boolean) as string[];
    expect(examples.length).toBeGreaterThan(8);
    for (const example of examples) expect(() => JSON.parse(example)).not.toThrow();
  });
});
