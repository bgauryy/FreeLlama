import { describe, expect, it } from "vitest";
import { BASH_SYSTEM_PROMPT, githubSystemPrompt, octocodeSystemPrompt } from "../../src/research/prompts.js";

describe("research adapter prompts", () => {
  it("carry no benchmark-specific example", () => {
    expect(BASH_SYSTEM_PROMPT).not.toMatch(/click|core\.py/);
  });
  it("tell a JSON-action model that CLI-only conventions do not apply", () => {
    for (const prompt of [octocodeSystemPrompt("/w"), githubSystemPrompt()]) {
      expect(prompt).toContain("You do not run a shell");
    }
  });
});
