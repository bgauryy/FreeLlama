import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { createApp } from "./app";
import { createCollector } from "./collector";
import { loadConfig } from "./config";
import { listen } from "./listen";

const config = await loadConfig();
const staticDir = fileURLToPath(new URL("../client/", import.meta.url));
const server = createServer(
  createApp({ collector: createCollector(config), staticDir }),
);
await listen(server, config);
for (const signal of ["SIGINT", "SIGTERM"] as const)
  process.once(signal, () => {
    server.close();
    server.closeAllConnections();
  });
