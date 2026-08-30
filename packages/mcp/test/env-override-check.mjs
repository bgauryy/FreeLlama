import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

// Point at a port nothing is listening on to prove the env var actually changes behavior.
const transport = new StdioClientTransport({
  command: "node",
  args: ["dist/index.js"],
  env: { ...process.env, FREELLAMA_OLLAMA_ENDPOINT: "http://127.0.0.1:9999" },
});
const client = new Client({ name: "env-override-test", version: "0.0.1" });
await client.connect(transport);

const result = await client.callTool({ name: "doctor", arguments: {} });
console.log("isError:", result.isError ?? false);
console.log(result.content[0].text);
await client.close();
process.exit(0);
