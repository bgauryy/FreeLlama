import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const transport = new StdioClientTransport({
  command: "node",
  args: ["dist/index.js"],
});

const client = new Client({ name: "test-client", version: "0.0.1" });
await client.connect(transport);

const tools = await client.listTools();
console.log("=== tools/list ===");
console.log(tools.tools.map((t) => t.name).join(", "));

console.log("\n=== tools/call: doctor ===");
const doctorResult = await client.callTool({ name: "doctor", arguments: {} });
console.log(doctorResult.content[0].text);

console.log("\n=== tools/call: machine (expect connection error, no serve running) ===");
const machineResult = await client.callTool({ name: "machine", arguments: {} });
console.log("isError:", machineResult.isError);
console.log(machineResult.content[0].text);

await client.close();
process.exit(0);
