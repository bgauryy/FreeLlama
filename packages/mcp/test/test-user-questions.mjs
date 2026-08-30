import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const WORKSPACE = "/Users/guybary/Documents/code/FreeLlama";
const MODEL = "qwen3.8:27b-mlx";

const questions = [
  "Where is the retry logic implemented for the FreeLlama proxy, and what is the max attempt count?",
  "What does the ollama_delete MCP tool do and what safety rule does its description state?",
  "What is the name of the field in Cargo.toml that excludes the CLI binary from the napi build?"
];

async function callDelegate(questionIndex, question) {
  const transport = new StdioClientTransport({ command: "node", args: ["dist/index.js"] });
  const client = new Client({ name: "test-client", version: "0.0.1" });
  await client.connect(transport);

  console.log(`\n${"=".repeat(80)}`);
  console.log(`QUESTION ${questionIndex + 1}: ${question}`);
  console.log("=".repeat(80));

  const result = await client.callTool({
    name: "delegate_research",
    arguments: {
      question,
      workspacePath: WORKSPACE,
      model: MODEL,
    },
  });

  console.log("\nRaw Result:");
  console.log(result.content[0].text);

  await client.close();
  return result.content[0].text;
}

// Run all three questions sequentially
for (let i = 0; i < questions.length; i++) {
  await callDelegate(i, questions[i]);
}

process.exit(0);
