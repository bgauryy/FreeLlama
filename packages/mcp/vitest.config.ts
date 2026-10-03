import { defineConfig } from "vitest/config";

// Unit tier: pure functions imported straight from src/*.ts (vitest transpiles TS with esbuild,
// so no dist build is needed — this is the TDD loop). No Ollama or external services;
// startup fixtures use disposable local listeners and child processes.
export default defineConfig({
  test: {
    name: "mcp",
    environment: "node",
    include: ["test/unit/**/*.test.ts"],
  },
});
