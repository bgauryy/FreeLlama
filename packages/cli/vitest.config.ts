import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    name: "cli",
    globalSetup: [fileURLToPath(new URL("../../vitest.tmp.global.ts", import.meta.url))],
    environment: "node",
    include: ["test/**/*.test.ts"],
    testTimeout: 30_000,
  },
});
