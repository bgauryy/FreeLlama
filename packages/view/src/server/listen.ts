import type { Server } from "node:http";
import type { ViewConfig } from "./config";

export async function listen(server: Server, config: ViewConfig) {
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(config.port, "127.0.0.1", () => {
      server.off("error", reject);
      resolve();
    });
  });
  console.log(`FreeLlama view: http://127.0.0.1:${config.port}`);
  console.log(`Control API: ${config.endpoint}`);
}
