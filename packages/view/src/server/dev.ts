import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import { createServer as createVite } from "vite";
import { createApp } from "./app";
import { createCollector } from "./collector";
import { loadConfig } from "./config";
import { listen } from "./listen";

const config = await loadConfig();
const server = createServer();
const root = fileURLToPath(new URL("../../", import.meta.url));
const vite = await createVite({
  root,
  envPrefix: [],
  server: {
    middlewareMode: true,
    hmr: { server },
    allowedHosts: ["localhost"],
    fs: {
      allow: [
        root,
        fileURLToPath(new URL("../../../../node_modules/", import.meta.url)),
      ],
      deny: [
        ".env",
        ".env.*",
        "*.{crt,pem}",
        "**/.git/**",
        ...(process.env.FREELLAMA_AUTH_TOKEN_FILE
          ? [resolve(process.env.FREELLAMA_AUTH_TOKEN_FILE)]
          : []),
      ],
    },
  },
});
server.on(
  "request",
  createApp({
    collector: createCollector(config),
    middleware: vite.middlewares,
    development: true,
  }),
);
await listen(server, config);

for (const signal of ["SIGINT", "SIGTERM"] as const)
  process.once(signal, () => {
    void vite.close().then(() => {
      server.close();
      server.closeAllConnections();
    });
  });
