import { createServer } from "node:http";
import { expect, it } from "vitest";
import { type IsolatedServe, releaseServeAvailable, serveAuthHeaders, startIsolatedServe } from "../setup/client.js";

it.runIf(releaseServeAvailable)("isolated E2E service uses the selected Ollama endpoint", async () => {
  const upstream = createServer((request, response) => {
    response.setHeader("content-type", "application/json");
    response.end(JSON.stringify(request.url === "/api/tags"
      ? { models: [{ name: "isolated-fixture:latest", digest: "isolated-digest", size: 1 }] }
      : request.url === "/api/show"
        ? { capabilities: ["completion"], model_info: { "llama.context_length": 4096 } }
        : { models: [] }));
  });
  await new Promise<void>((resolve) => upstream.listen(0, "127.0.0.1", resolve));
  const address = upstream.address();
  if (!address || typeof address === "string") throw new Error("fixture has no address");
  const expected = `http://127.0.0.1:${address.port}`;
  const previous = process.env.FREELLAMA_OLLAMA_ENDPOINT;
  process.env.FREELLAMA_OLLAMA_ENDPOINT = expected;
  let isolated: IsolatedServe | undefined;
  try {
    isolated = await startIsolatedServe();
    const status = await fetch(`${isolated.endpoint}/_freellama/v1/status`, { headers: serveAuthHeaders() }).then((response) => response.json());
    expect(status.backends.gpu.upstream).toBe(expected);
    const models = await fetch(`${isolated.endpoint}/_freellama/v1/models`, { headers: serveAuthHeaders() }).then((response) => response.json());
    expect(JSON.stringify(models)).toContain("isolated-fixture:latest");
  } finally {
    isolated?.child.kill();
    if (previous === undefined) delete process.env.FREELLAMA_OLLAMA_ENDPOINT;
    else process.env.FREELLAMA_OLLAMA_ENDPOINT = previous;
    await new Promise<void>((resolve, reject) => upstream.close((error) => error ? reject(error) : resolve()));
  }
});
