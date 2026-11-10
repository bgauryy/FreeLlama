import { describe, expect, it, vi } from "vitest";

const presence = vi.hoisted(() => ({ value: "missing" as "running" | "stopped" | "missing" }));
vi.mock("../../src/ollama-presence.js", async (importOriginal) => ({
  ...(await importOriginal<Record<string, unknown>>()),
  ollamaPresence: async () => presence.value,
}));
vi.mock("../../src/serve.js", async (importOriginal) => ({ ...(await importOriginal<Record<string, unknown>>()), configureServeAutostart: vi.fn() }));

import { OLLAMA_DOWNLOAD_URL, OLLAMA_MISSING, ollamaInstalled } from "../../src/ollama-presence.js";
const { ollamaPresence: realPresence } = await vi.importActual<typeof import("../../src/ollama-presence.js")>("../../src/ollama-presence.js");
import { main } from "../../src/agent-cli/main.js";

function capture() {
  const out: string[] = [];
  const err: string[] = [];
  return { io: { out: (line: string) => out.push(line), err: (line: string) => err.push(line) }, out, err };
}

describe("ollamaInstalled", () => {
  it("finds ollama on PATH, then in the installers' default places", () => {
    const at = (...files: string[]) => (file: string) => files.includes(file);
    expect(ollamaInstalled({ PATH: "/a:/b" }, "linux", at("/b/ollama"))).toBe(true);
    expect(ollamaInstalled({ PATH: "/a" }, "darwin", at("/Applications/Ollama.app"))).toBe(true);
    expect(ollamaInstalled({ PATH: "/a" }, "linux", at())).toBe(false);
  });
});

describe("ollamaPresence", () => {
  const down = (async () => { throw new TypeError("fetch failed"); }) as unknown as typeof fetch;
  it("is running when /api/version answers", async () => {
    const up = vi.fn(async () => new Response("{}")) as unknown as typeof fetch;
    expect(await realPresence("http://127.0.0.1:11434", { fetch: up, installed: () => false })).toBe("running");
  });
  it("is stopped or missing by whether Ollama is installed", async () => {
    expect(await realPresence("http://127.0.0.1:11434", { fetch: down, installed: () => true })).toBe("stopped");
    expect(await realPresence("http://127.0.0.1:11434", { fetch: down, installed: () => false })).toBe("missing");
  });
});

describe("freellama pre-flight", () => {
  it("says at once to install Ollama, with the link, and exits 1", async () => {
    presence.value = "missing";
    const { io, err } = capture();
    expect(await main(["status"], io)).toBe(1);
    expect(err.join("\n")).toBe(`freellama: ${OLLAMA_MISSING}`);
    expect(err.join("\n")).toContain(OLLAMA_DOWNLOAD_URL);
  });
  it("says to start a stopped Ollama and exits 75, retryable", async () => {
    presence.value = "stopped";
    const { io, err } = capture();
    expect(await main(["chat"], io)).toBe(75);
    expect(err.join("\n")).toMatch(/not running.*ollama serve/);
  });
  it("still answers --help and offline commands without Ollama", async () => {
    presence.value = "missing";
    const { io, out } = capture();
    expect(await main(["run", "--help"], io)).toBe(0);
    expect(await main(["workflows"], io)).toBe(0);
    expect(out.join("\n")).toContain("fix-bug");
  });
});
