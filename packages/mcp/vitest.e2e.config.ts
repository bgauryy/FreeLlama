import { defineConfig } from "vitest/config";

// E2E tier: the whole system live — release binary (`cargo build --release`), installed models,
// network to ollama.com. Includes the pull→delete lifecycle round trip and a real delegated
// research run, so it is slow and deliberately opt-in.
export default defineConfig({
  root: import.meta.dirname,
  test: {
    name: "mcp-e2e",
    environment: "node",
    include: ["test/e2e/**/*.test.ts"],
    globalSetup: ["test/setup/global.ts"],
    fileParallelism: false,
    testTimeout: 600_000,
    hookTimeout: 120_000,
  },
});
