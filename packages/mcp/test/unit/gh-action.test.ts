import { describe, expect, it } from "vitest";
import { runGh, validateGhArgs } from "../../src/research/gh-action.js";

describe("read-only gh action", () => {
  it("accepts read-only commands", () => {
    for (const args of [
      ["search", "code", "SystemOneRequest", "--repo", "ollama/ollama"],
      ["repo", "view", "colinhacks/zod", "--json", "name"],
      ["pr", "view", "18606", "--repo", "ollama/ollama"],
      ["issue", "list", "--repo", "a/b", "--search", "x"],
      ["api", "repos/ollama/ollama/contents/api/types.go", "--jq", ".content"],
    ]) expect(validateGhArgs(args)).toEqual(args);
  });

  it("refuses anything that writes, sends a body, changes the method or opens a browser", () => {
    expect(() => validateGhArgs(["pr", "create"])).toThrow(/not read-only/);
    expect(() => validateGhArgs(["repo", "delete", "a/b"])).toThrow(/not read-only/);
    expect(() => validateGhArgs(["auth", "token"])).toThrow(/not allowed/);
    expect(() => validateGhArgs(["api", "repos/a/b/issues", "-f", "title=x"])).toThrow(/flag -f is not allowed/);
    expect(() => validateGhArgs(["api", "repos/a/b", "--method=DELETE"])).toThrow(/--method=DELETE is not allowed/);
    expect(() => validateGhArgs(["api", "repos/a/b", "-H", "X-HTTP-Method-Override: DELETE"])).toThrow(/-H is not allowed/);
    expect(() => validateGhArgs(["api", "graphql"])).toThrow(/no graphql/);
    expect(() => validateGhArgs(["repo", "view", "a/b", "--web"])).toThrow(/--web is not allowed/);
    expect(() => validateGhArgs("search code x")).toThrow(/array of 1-40 strings/);
    expect(() => validateGhArgs(["search", "code", "a\nb"])).toThrow(/array/);
  });

  it.runIf(process.env.FREELLAMA_TEST_GH === "1")("reads GitHub through the operator's gh login", async () => {
    const output = await runGh(["repo", "view", "ollama/ollama", "--json", "primaryLanguage", "--jq", ".primaryLanguage.name"], process.cwd(), 30);
    expect(output.trim()).toBe("Go");
  });
});
