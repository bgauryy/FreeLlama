import { defineConfig } from "vitest/config";

// Integration tier: drives the built server (dist/index.js) through the real MCP protocol over
// stdio. Needs a live Ollama on :11434; `freellama serve` is optional (tests probe and adapt).
// The global setup rebuilds dist/ with esbuild and fails fast if Ollama is unreachable.
// Files run sequentially: they share one Ollama and spawn server subprocesses.
export default defineConfig({
  root: import.meta.dirname,
  test: {
    name: "mcp-integration",
    environment: "node",
    include: ["test/integration/**/*.test.ts"],
    globalSetup: ["test/setup/global.ts"],
    fileParallelism: false,
    testTimeout: 120_000,
    hookTimeout: 60_000,
  },
});
