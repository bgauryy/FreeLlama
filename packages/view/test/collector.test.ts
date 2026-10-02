import { describe, expect, it, vi } from "vitest";
import { createCollector } from "../src/server/collector";
import fixture from "./fixtures/control-api.json";
import { snapshotSchema } from "../src/shared/contracts";

const health = {
  status: "ok",
  version: "0.2.0",
  sessions: { active: 2, max_sessions: 128, idle_ttl_seconds: 300 },
  security: { authentication: "none", remote_access: false },
};

describe("runtime collection", () => {
  it("validates a complete control snapshot with null GPU telemetry, runner activity, and optional jobs", async () => {
    const fetcher = vi.fn(async (url: string) =>
      Response.json(
        fixture[
          new URL(url).pathname.split("/").at(-1) as keyof typeof fixture
        ],
      ),
    );
    const collector = createCollector({
      endpoint: "http://127.0.0.1:11435",
      fetcher,
    });
    const snapshot = snapshotSchema.parse(await collector.snapshot());
    expect(
      Object.values(snapshot.sources).every(
        (source) => source.state === "live",
      ),
    ).toBe(true);
    expect(
      snapshot.sources.status.data?.host.gpu_memory_total_bytes,
    ).toBeNull();
    expect(snapshot.sources.status.data?.loaded_models).toHaveLength(2);
    expect(snapshot.sources.status.data?.task_jobs?.jobs[0]?.status).toBe(
      "waiting_for_resources",
    );
    expect(snapshot.sources.models.data?.models).toHaveLength(3);
  });

  it("isolates failed sources, validates shapes, and preserves the last valid observation", async () => {
    let now = 1_000_000;
    let healthy = true;
    const fetcher = vi.fn(async (url: string) => {
      if (url.endsWith("/health"))
        return Response.json(healthy ? health : { status: 42 });
      return new Response("not available", { status: 503 });
    });
    const collector = createCollector({
      endpoint: "http://127.0.0.1:11435",
      fetcher,
      now: () => now,
    });
    const first = await collector.snapshot();
    expect(first.sources.health.state).toBe("live");
    expect(first.sources.health.data?.version).toBe("0.2.0");
    expect(first.sources.status.state).toBe("unavailable");
    healthy = false;
    now += 61_000;
    const next = await collector.snapshot();
    expect(next.sources.health.state).toBe("stale");
    expect(next.sources.health.data).toEqual(first.sources.health.data);
    expect(next.sources.health.updated_at).toBe(
      first.sources.health.updated_at,
    );
    expect(next.sources.health.error).toContain("schema");
  });

  it("deduplicates simultaneous reads and caches slower sources", async () => {
    let now = 1_000_000;
    const fetcher = vi.fn(async () => Response.json(health));
    const collector = createCollector({
      endpoint: "http://127.0.0.1:11435",
      fetcher,
      now: () => now,
    });
    await Promise.all([
      collector.snapshot(),
      collector.snapshot(),
      collector.snapshot(),
    ]);
    expect(fetcher).toHaveBeenCalledTimes(6);
    now += 2_100;
    await collector.snapshot();
    expect(fetcher).toHaveBeenCalledTimes(7);
  });

  it("forwards auth only to fixed control paths and never exposes a reflected token", async () => {
    const token = "server-only-sensitive-token";
    const fetcher = vi.fn(async () => new Response(token, { status: 401 }));
    const collector = createCollector({
      endpoint: "http://127.0.0.1:11435",
      token,
      fetcher,
    });
    const result = await collector.snapshot();
    expect(JSON.stringify(result)).not.toContain(token);
    expect(result.sources.health.error).toContain("token file");
    for (const [url, options] of fetcher.mock.calls as unknown as [
      string,
      RequestInit,
    ][]) {
      expect(url).toMatch(/^http:\/\/127\.0\.0\.1:11435\/_freellama\/v1\//);
      expect(options.headers).toEqual({
        Authorization: `Bearer ${token}`,
        Accept: "application/json",
      });
      expect(options.redirect).toBe("error");
    }
  });
});
