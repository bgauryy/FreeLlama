import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const transport = new StdioClientTransport({ command: "node", args: ["dist/index.js"] });
const client = new Client({ name: "test-client", version: "0.0.1" });
await client.connect(transport);

console.log("=== tools/call: delegate_research ===");
const result = await client.callTool({
  name: "delegate_research",
  arguments: {
    question:
      "In packages/rust-core/Cargo.toml, what is the name of the optional feature that enables the Node addon?",
    workspacePath: "/Users/guybary/Documents/code/FreeLlama",
  },
});
console.log("isError:", result.isError ?? false);
console.log(result.content[0].text);

await client.close();
process.exit(0);
