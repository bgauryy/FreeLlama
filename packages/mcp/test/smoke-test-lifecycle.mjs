import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { fileURLToPath } from "node:url";

// Resolve the built server from this file, not the working directory. Every one of these
// scripts used to pass a bare "dist/index.js", so they only ran from packages/mcp — the
// command the README documents (`node packages/mcp/test/validate-all.mjs`, from the repo
// root) failed with MODULE_NOT_FOUND before the server ever started.
const SERVER_ENTRY = fileURLToPath(new URL("../dist/index.js", import.meta.url));

const transport = new StdioClientTransport({ command: "node", args: [SERVER_ENTRY] });
const client = new Client({ name: "lifecycle-test", version: "0.0.1" });
await client.connect(transport);

console.log("=== tools/list ===");
console.log((await client.listTools()).tools.map((t) => t.name).join(", "));

console.log("\n=== models (raw view) ===");
const list1 = await client.callTool({ name: "models", arguments: { view: "raw" } });
const before = JSON.parse(list1.content[0].text).models.map((m) => m.name);
console.log("installed:", before.join(", "));

console.log("\n=== models (resident view) ===");
console.log((await client.callTool({ name: "models", arguments: { view: "resident" } })).content[0].text);

console.log("\n=== round-trip: ollama_manage pull qwen2.5:0.5b ===");
const pullResult = await client.callTool({
  name: "ollama_manage",
  arguments: { action: "pull", model: "qwen2.5:0.5b" },
});
console.log("isError:", pullResult.isError ?? false, pullResult.content[0].text);

const list2 = await client.callTool({ name: "models", arguments: { view: "raw" } });
const afterPull = JSON.parse(list2.content[0].text).models.map((m) => m.name);
console.log("now installed 0.5b:", afterPull.includes("qwen2.5:0.5b"));

console.log("\n=== ollama_delete qwen2.5:0.5b (undo the pull, net-zero) ===");
const deleteResult = await client.callTool({
  name: "ollama_delete",
  arguments: { model: "qwen2.5:0.5b" },
});
console.log("isError:", deleteResult.isError ?? false, deleteResult.content[0].text);

const list3 = await client.callTool({ name: "models", arguments: { view: "raw" } });
const afterDelete = JSON.parse(list3.content[0].text).models.map((m) => m.name);
console.log("still installed 0.5b:", afterDelete.includes("qwen2.5:0.5b"));
console.log("net-zero vs before:", JSON.stringify(before) === JSON.stringify(afterDelete));

await client.close();
process.exit(0);
