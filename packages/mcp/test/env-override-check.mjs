import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { fileURLToPath } from "node:url";

// Resolve the built server from this file, not the working directory. Every one of these
// scripts used to pass a bare "dist/index.js", so they only ran from packages/mcp — the
// command the README documents (`node packages/mcp/test/validate-all.mjs`, from the repo
// root) failed with MODULE_NOT_FOUND before the server ever started.
const SERVER_ENTRY = fileURLToPath(new URL("../dist/index.js", import.meta.url));

// Point at a port nothing is listening on to prove the env var actually changes behavior.
const transport = new StdioClientTransport({
  command: "node",
  args: [SERVER_ENTRY],
  env: { ...process.env, FREELLAMA_OLLAMA_ENDPOINT: "http://127.0.0.1:9999" },
});
const client = new Client({ name: "env-override-test", version: "0.0.1" });
await client.connect(transport);

const result = await client.callTool({ name: "doctor", arguments: {} });
console.log("isError:", result.isError ?? false);
console.log(result.content[0].text);
await client.close();
process.exit(0);
