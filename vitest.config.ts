import { defineConfig } from "vitest/config";

// Root config discovers each package's own vitest.config.ts as a project.
// Fast, dependency-free unit tests only — `vitest run` at the root must always be safe.
// Slower tiers live in explicit configs: packages/mcp/vitest.integration.config.ts (needs a live
// Ollama) and packages/mcp/vitest.e2e.config.ts (needs the release binary and installed models).
export default defineConfig({
  test: {
    projects: ["packages/*/vitest.config.ts"],
  },
});
