import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { connectClient } from "../setup/client.js";

describe("resident processor percentages", () => {
  let client: Client;
  let server: Server;
  let endpoint: string;
  let residentSize: number | undefined = 1000;
  let vram = 1000;
  beforeAll(async () => {
    server = createServer((_, response) => {
      response.setHeader("content-type", "application/json");
      response.end(JSON.stringify({ models: [{ name: "gpu:fixture", size: 4000, resident: true,
        resident_size: residentSize, resident_vram: vram,
        execution: { backend: "gpu", placement: "gpu", observation: {
          processor: residentSize === undefined ? "unknown" : vram < residentSize ? "mixed" : "gpu",
          status: residentSize === undefined ? "unavailable" : vram < residentSize ? "mismatch" : "verified",
        } },
      }] }));
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing fixture address");
    endpoint = `http://127.0.0.1:${address.port}`;
    client = await connectClient();
  });
  afterAll(async () => {
    await client?.close();
    await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  });
  it.each([
    { total: 1000, vram: 1000, percent: 100 },
    { total: 2000, vram: 500, percent: 25 },
  ])("uses resident bytes, preserving installed size ($percent% GPU)", async (fixture) => {
    residentSize = fixture.total; vram = fixture.vram;
    const result: any = await client.callTool({ name: "models", arguments: { endpoint, view: "resident" } });
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent.models[0]).toMatchObject({ size: 4000, placement: { gpu_percent: fixture.percent } });
  });
  it("does not infer processor percentages from disk size when resident size is missing", async () => {
    residentSize = undefined; vram = 500;
    const result: any = await client.callTool({ name: "models", arguments: { endpoint, view: "resident" } });
    expect(result.isError).not.toBe(true);
    expect(result.structuredContent.models[0]).not.toHaveProperty("placement");
    expect(result.structuredContent.models[0].execution.observation.processor).toBe("unknown");
  });
});
