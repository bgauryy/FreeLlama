import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

// Exercises run_task and route/recommend's requiredCapabilities against a real freellama serve
// instance — these only do anything meaningful with the control-plane routes up, unlike the
// other smoke tests which mostly exercise the doctor/no-serve-required path.
const REPO_ROOT = path.resolve(fileURLToPath(import.meta.url), "../../../");
const SERVE_ENDPOINT = "http://127.0.0.1:11435";

async function waitForServe(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${SERVE_ENDPOINT}/_freellama/v1/machine`);
      if (response.ok) return;
    } catch {
      // not up yet
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`freellama serve did not come up within ${timeoutMs}ms`);
}

const serve = spawn(
  "./target/release/freellama",
  ["serve", "--recommendation-catalog", "recommendations.example.toml"],
  { cwd: REPO_ROOT, stdio: "ignore" },
);

try {
  await waitForServe(15_000);

  const transport = new StdioClientTransport({ command: "node", args: ["dist/index.js"] });
  const client = new Client({ name: "run-task-test", version: "0.0.1" });
  await client.connect(transport);

  console.log("=== route with requiredCapabilities: [\"vision\"] (should find a model) ===");
  const visionRoute = await client.callTool({
    name: "route",
    arguments: { task: "completion", objective: "fastest", requiredCapabilities: ["vision"] },
  });
  console.log("isError:", visionRoute.isError ?? false);
  const visionDecision = JSON.parse(visionRoute.content[0].text);
  if (visionRoute.isError || !visionDecision.required_capabilities?.includes("vision")) {
    throw new Error("expected a successful route honoring requiredCapabilities=[vision]");
  }
  console.log("selected_model:", visionDecision.selected_model);

  console.log('\n=== route with requiredCapabilities: ["audio"] (no audio model installed, should fail) ===');
  const audioRoute = await client.callTool({
    name: "route",
    arguments: { task: "completion", objective: "fastest", requiredCapabilities: ["audio"] },
  });
  console.log("isError:", audioRoute.isError ?? false, audioRoute.content[0].text);
  if (!audioRoute.isError) {
    throw new Error("expected requiredCapabilities=[audio] to fail — no audio model is installed");
  }

  console.log("\n=== run_task: actually execute a prompt ===");
  const taskResult = await client.callTool({
    name: "run_task",
    arguments: {
      task: "completion",
      objective: "fastest",
      prompt: "Reply with exactly the word: PONG",
      keepAlive: "0",
    },
  });
  console.log("isError:", taskResult.isError ?? false);
  const executed = JSON.parse(taskResult.content[0].text);
  if (taskResult.isError || !executed.response || !executed.route) {
    throw new Error("expected run_task to actually route and execute, got: " + taskResult.content[0].text);
  }
  console.log("selected_model:", executed.route.selected_model, "| got a real response:", Boolean(executed.response.message));

  await client.close();
  console.log("\nAll run_task/requiredCapabilities assertions passed.");
} finally {
  serve.kill();
}
process.exit(0);
