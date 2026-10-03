import type { IncomingMessage, ServerResponse } from "node:http";
import { win32 } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

const stat = vi.fn(async () => ({ isFile: () => true, size: 16 }));
const readFile = vi.fn(async () => "<h1>Runtime</h1>");

afterEach(() => {
  vi.doUnmock("node:path");
  vi.doUnmock("node:fs/promises");
  vi.resetModules();
  vi.clearAllMocks();
});

describe("static files with Windows path semantics", () => {
  async function request(url: string) {
    vi.doMock("node:path", () => win32);
    vi.doMock("node:fs/promises", () => ({ stat, readFile }));
    const { createApp } = await import("../src/server/app");
    const result: { status?: number; body?: unknown } = {};
    await createApp({
      staticDir: "C:\\FreeLlama\\dist\\client",
      collector: { snapshot: async () => ({}) } as never,
    })(
      { url, method: "GET", headers: { host: "127.0.0.1:5173" } } as IncomingMessage,
      {
        setHeader() {},
        writeHead(status: number) { result.status = status; },
        end(body: unknown) { result.body = body; },
      } as unknown as ServerResponse,
    );
    return result;
  }

  it.each([
    ["/", "C:\\FreeLlama\\dist\\client\\index.html"],
    ["/assets/main.js", "C:\\FreeLlama\\dist\\client\\assets\\main.js"],
  ])("serves %s from the built client", async (url, file) => {
    expect(await request(url)).toEqual({ status: 200, body: "<h1>Runtime</h1>" });
    expect(stat).toHaveBeenCalledWith(file);
  });

  it.each(["/..%5csecret", "/.env", "/assets%5c.hidden", "/C:%5csecret", "/D:%5csecret"])
    ("rejects traversal, dotfiles, and foreign roots: %s", async (url) => {
      expect((await request(url)).status).toBe(404);
      expect(stat).not.toHaveBeenCalled();
    });
});
