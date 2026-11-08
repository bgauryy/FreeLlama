import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { assertPublicHost, crawl, htmlToText, pageLinks, robotsDisallow } from "../../src/sweep/sources.js";
import { defaultFlags, SweepRunner, verdictOf, type Executor } from "../../src/sweep/runner.js";
import { SweepStore } from "../../src/sweep/store.js";

const noul = (p: number) => ({ type: "noul", noul: p });

describe("sweep verdicts", () => {
  const flags = defaultFlags({ bug: { type: "noul", instructions: "Is there a bug?" }, kind: { type: "choice", instructions: "Kind?", criteria: ["a", "b"] } });

  it("flags yes/no questions by default and leaves choice questions to explicit rules", () => {
    expect(flags).toEqual([{ question: "bug", when: "true", atLeast: 0.5 }]);
  });

  it("is flagged at the threshold, unsure just below it, clear well below", () => {
    expect(verdictOf({ bug: noul(0.9) }, flags, 0.15)).toBe("flagged");
    expect(verdictOf({ bug: noul(0.4) }, flags, 0.15)).toBe("unsure");
    expect(verdictOf({ bug: noul(0.1) }, flags, 0.15)).toBe("clear");
    expect(verdictOf({}, flags, 0.15)).toBe("unsure");
  });

  it("reads a choice option's probability", () => {
    const rules = [{ question: "kind", when: "bug", atLeast: 0.6 }];
    expect(verdictOf({ kind: { type: "choice", choice: "bug", probabilities: { bug: 0.8, other: 0.2 } } }, rules, 0.1)).toBe("flagged");
    expect(verdictOf({ kind: { type: "choice", choice: "other", probabilities: { bug: 0.1, other: 0.9 } } }, rules, 0.1)).toBe("clear");
  });
});

describe("sweep sources", () => {
  it("turns HTML into readable lines without scripts or markup", () => {
    const { title, text } = htmlToText("<html><head><title> Docs  Home </title><style>p{}</style></head><body><script>x()</script><h1>Hello</h1><p>A &amp; B<br>next</p><ul><li>one</li><li>two</li></ul></body></html>");
    expect(title).toBe("Docs Home");
    expect(text).toContain("Hello");
    expect(text).toContain("A & B\nnext");
    expect(text).toContain("- one");
    expect(text).not.toMatch(/x\(\)|p\{\}|<|>/);
    const chrome = htmlToText("<nav><ul><li><a>Home</a></li></ul></nav><main><p>Body</p><ul><li></li></ul></main><footer>© site</footer>").text;
    expect(chrome).toBe("Body");
  });

  it("resolves links, drops fragments and non-http schemes", () => {
    expect(pageLinks('<a href="/a#x">a</a><a href=\'b\'>b</a><a href="mailto:x@y">m</a><a href="/a">dup</a>', "https://site.test/docs/")).toEqual([
      "https://site.test/a", "https://site.test/docs/b",
    ]);
  });

  it("reads robots.txt rules that apply to every agent", () => {
    expect(robotsDisallow("User-agent: other\nDisallow: /x\n\nUser-agent: *\nDisallow: /private\nDisallow:\n")).toEqual(["/private"]);
  });

  it("refuses local and private hosts, including after DNS", async () => {
    await expect(assertPublicHost(new URL("http://localhost:11434/"))).rejects.toThrow(/local host/);
    await expect(assertPublicHost(new URL("http://192.168.1.1/"))).rejects.toThrow(/private/);
    await expect(assertPublicHost(new URL("http://[::1]/"))).rejects.toThrow(/private/);
    const resolve = (async () => [{ address: "10.0.0.5", family: 4 }]) as never;
    await expect(assertPublicHost(new URL("https://rebind.test/"), resolve)).rejects.toThrow(/private/);
    const publicResolve = (async () => [{ address: "93.184.216.34", family: 4 }]) as never;
    await expect(assertPublicHost(new URL("https://example.test/"), publicResolve)).resolves.toBeUndefined();
  });

  it("crawls same-origin pages breadth first, honors robots.txt and re-checks every redirect hop", async () => {
    const pages: Record<string, { status?: number; body?: string; type?: string; location?: string }> = {
      "https://site.test/robots.txt": { body: "User-agent: *\nDisallow: /secret", type: "text/plain" },
      "https://site.test/": { body: '<a href="/a">a</a><a href="/secret">s</a><a href="https://other.test/">o</a><a href="/hop">h</a>', type: "text/html" },
      "https://site.test/a": { body: "<p>page a</p>", type: "text/html" },
      "https://site.test/hop": { status: 302, location: "http://127.0.0.1/admin" },
    };
    const fetcher = async (url: string) => {
      const page = pages[url];
      if (!page) return new Response("missing", { status: 404 });
      return new Response(page.body ?? "", { status: page.status ?? 200, headers: { "content-type": page.type ?? "text/html", ...(page.location ? { location: page.location } : {}) } });
    };
    const fetched: string[] = [];
    const resolve = (async () => [{ address: "93.184.216.34", family: 4 }]) as never;
    const { pages: count, skipped } = await crawl(["https://site.test/"], {
      sameOrigin: true, maxPages: 10, maxDepth: 1, chunkBytes: 1000, fetcher, resolve, delayMs: 0,
      onPage: (url) => fetched.push(url),
    });
    expect(fetched).toEqual(["https://site.test/", "https://site.test/a"]);
    expect(count).toBe(2);
    expect(skipped.map((item) => item.reason).join(" ")).toMatch(/robots\.txt/);
    expect(skipped.map((item) => item.reason).join(" ")).toMatch(/private|local/);
  });
});

function workspace() {
  const root = mkdtempSync(path.join(tmpdir(), "freellama-sweep-"));
  mkdirSync(path.join(root, "src"));
  writeFileSync(path.join(root, "src", "a.ts"), "const a = 1;\n// TODO leak\n");
  writeFileSync(path.join(root, "src", "b.ts"), "const b = 2;\n");
  return root;
}

/** Answers "bug" yes for text containing "leak", like a model would; counts calls per model. */
function fakeModel(calls: string[] = [], failFirst = 0): Executor {
  let failures = failFirst;
  return async (request) => {
    calls.push(`${String(request.task)}:${String(request.model)}`);
    if (failures > 0) {
      failures -= 1;
      throw new Error(JSON.stringify({ error: "queue full", code: "queue_full", retry_safe: true }));
    }
    if (request.task === "completion") return { response: { message: { content: "It leaks." } } };
    const state = String(request.state);
    return { response: { answers: { bug: noul(state.includes("leak") ? 0.93 : 0.05) } } };
  };
}

describe("sweep runner", () => {
  const questions = { bug: { type: "noul" as const, instructions: "Is there a bug?" } };
  const base = (files: string[]) => ({
    model: "decider", questions, flags: defaultFlags(questions), unsureMargin: 0.15, priority: "background" as const,
    maxParallelism: 1, chunkBytes: 3000, source: { kind: "files" as const, files },
  });

  it("judges every chunk, verifies and explains flagged ones, and saves each result", async () => {
    const root = workspace();
    const store = new SweepStore(path.join(root, ".sweeps"));
    const calls: string[] = [];
    const runner = new SweepRunner(store, fakeModel(calls), async () => undefined);
    const started = await runner.start({ ...base([path.join(root, "src", "*.ts")]), verify: { model: "big" }, explain: { model: "writer", prompt: "Why?", maxTokens: 50 } });
    await runner.waitFor(started.id, 5_000);
    const view = runner.view(started.id);
    expect(view.phase).toBe("completed");
    expect(view.counts).toMatchObject({ total: 2, done: 2, flagged: 1, clear: 1, failed: 0 });
    expect(calls).toEqual(["decision:decider", "decision:big", "completion:writer", "decision:decider"]);
    const flagged = [...store.results(started.id).values()].find((result) => result.verdict === "flagged");
    expect(flagged).toMatchObject({ verified: { model: "big", verdict: "flagged" }, note: "It leaks." });
  });

  it("re-runs only changed chunks against a baseline and reports what changed", async () => {
    const root = workspace();
    const store = new SweepStore(path.join(root, ".sweeps"));
    const first = new SweepRunner(store, fakeModel(), async () => undefined);
    const glob = path.join(root, "src", "*.ts");
    const baseline = await first.start(base([glob]));
    await first.waitFor(baseline.id, 5_000);
    writeFileSync(path.join(root, "src", "b.ts"), "const b = 2;\n// leak added\n");
    writeFileSync(path.join(root, "src", "c.ts"), "const c = 3;\n");
    const calls: string[] = [];
    const second = new SweepRunner(store, fakeModel(calls), async () => undefined);
    const rerun = await second.start({ ...base([glob]), baseline: baseline.id });
    await second.waitFor(rerun.id, 5_000);
    const view = second.view(rerun.id);
    expect(calls).toHaveLength(2);
    expect(view.counts).toMatchObject({ total: 3, reused: 1, flagged: 2, changed: 1, added: 1, removed: 0 });
    const changes = [...store.results(rerun.id).values()].map((result) => result.change).sort();
    expect(changes).toEqual(["changed", "new", "unchanged"]);
  });

  it("retries transient refusals and resumes chunks that still failed", async () => {
    const root = workspace();
    const store = new SweepStore(path.join(root, ".sweeps"));
    const failing: Executor = async () => { throw new Error(JSON.stringify({ error: "model missing", code: "model_not_found" })); };
    const runner = new SweepRunner(store, failing, async () => undefined);
    const sweep = await runner.start(base([path.join(root, "src", "a.ts")]));
    await runner.waitFor(sweep.id, 5_000);
    expect(runner.view(sweep.id).counts).toMatchObject({ failed: 1, done: 0 });
    const calls: string[] = [];
    const retrying = new SweepRunner(store, fakeModel(calls, 2), async () => undefined);
    retrying.resume(sweep.id);
    await retrying.waitFor(sweep.id, 5_000);
    expect(calls).toHaveLength(3);
    expect(retrying.view(sweep.id).counts).toMatchObject({ failed: 0, done: 1, flagged: 1 });
  });

  it("reports a sweep this process is not running as interrupted", async () => {
    const root = workspace();
    const store = new SweepStore(path.join(root, ".sweeps"));
    let release: () => void = () => undefined;
    const hang: Executor = () => new Promise((resolve) => { release = () => resolve({ response: { answers: { bug: noul(0.1) } } }); });
    const runner = new SweepRunner(store, hang, async () => undefined);
    const sweep = await runner.start(base([path.join(root, "src", "a.ts")]));
    expect(new SweepRunner(store, hang).view(sweep.id).phase).toBe("interrupted");
    release();
    await runner.waitFor(sweep.id, 5_000);
  });
});
