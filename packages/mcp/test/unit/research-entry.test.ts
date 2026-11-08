import { describe, expect, it } from "vitest";
import { pathToFileURL } from "node:url";
import { isEntry } from "../../src/research/entry.js";

describe("research adapter entry", () => {
  const bundle = "/x/dist/research/github-agent.js";
  const url = pathToFileURL(bundle).href;
  it("runs only the adapter the script is named after, even inside another adapter's bundle", () => {
    expect(isEntry(url, "github-agent", bundle)).toBe(true);
    expect(isEntry(url, "octocode-agent", bundle)).toBe(false);
    expect(isEntry(pathToFileURL("/x/src/research/bash-agent.ts").href, "bash-agent", "/x/src/research/bash-agent.ts")).toBe(true);
    expect(isEntry(url, "github-agent", "/x/other.js")).toBe(false);
    expect(isEntry(url, "github-agent", undefined)).toBe(false);
  });
});
