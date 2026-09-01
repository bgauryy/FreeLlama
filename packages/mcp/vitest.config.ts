import { defineConfig } from "vitest/config";

// Unit tier: pure functions imported straight from src/*.ts (vitest transpiles TS with esbuild,
// so no dist build is needed — this is the TDD loop). No Ollama, no network, no subprocesses.
export default defineConfig({
  test: {
    name: "mcp",
    environment: "node",
    include: ["test/unit/**/*.test.ts"],
  },
});
