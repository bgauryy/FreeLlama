import { afterEach, describe, expect, it, vi } from "vitest";
import { checkAudioCapability } from "../../src/audio-capability.js";

afterEach(() => vi.unstubAllGlobals());

describe("audio capability preflight", () => {
  it("refuses known incompatible exact tags and preserves the raw endpoint for compatible ones", async () => {
    const fetch = vi.fn().mockImplementation(async () => new Response(JSON.stringify({ capabilities: ["audio"] })));
    vi.stubGlobal("fetch", fetch);
    await checkAudioCapability("http://local", "exact:tag", "transcribe", {});
    await expect(checkAudioCapability("http://local", "exact:tag", "speak", {})).rejects.toThrow(/speech capability/);
    expect(JSON.parse(fetch.mock.calls[0]![1].body)).toEqual({ model: "exact:tag", verbose: false });
  });

  it("does not turn unavailable or missing metadata into incompatible-model evidence", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("lookup unavailable")));
    await checkAudioCapability("http://local", "exact:tag", "speak", {});
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify({ details: {} }))));
    await checkAudioCapability("http://local", "exact:tag", "speak", {});
  });

  it("does not swallow caller cancellation as an unknown metadata result", async () => {
    const controller = new AbortController();
    controller.abort();
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("cancelled")));
    await expect(checkAudioCapability("http://local", "exact:tag", "speak", {}, controller.signal)).rejects.toThrow(/cancelled/);
  });
});
