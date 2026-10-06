// Contract tests for the research adapters' context management, one case per contract.
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { setFlagsFromString } from "node:v8";
import { runInNewContext } from "node:vm";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { callSignature } from "../../src/research/actions.js";
import { AgentContextManager } from "../../src/research/context-manager.js";
import {
  AgentRuntimeConfig, CHARS_PER_TOKEN, ContextPolicy, IMAGE_TOKEN_ESTIMATE, contextBudget, type Message,
} from "../../src/research/context-policy.js";
import { ContextBudgeter, clip, compact, fitToContext, messagesTokens } from "../../src/research/fit.js";
import { ObservationStore, pageFooter, paginate } from "../../src/research/observations.js";
import { cpLength } from "../../src/research/text.js";

// Record every write the adapter code makes so streaming bounds can be asserted directly; V8's
// heap counters include uncollected garbage and cannot measure a peak the way tracemalloc does.
const writes = vi.hoisted(() => ({ largest: 0 }));
vi.mock("node:fs", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:fs")>();
  const writeSync = ((fd: number, buffer: NodeJS.ArrayBufferView | string, ...rest: unknown[]) => {
    writes.largest = Math.max(writes.largest, typeof buffer === "string" ? Buffer.byteLength(buffer) : buffer.byteLength);
    return (actual.writeSync as (...args: unknown[]) => number)(fd, buffer, ...rest);
  }) as typeof actual.writeSync;
  return { ...actual, writeSync, default: { ...actual, writeSync } };
});

setFlagsFromString("--expose-gc");
const collect = runInNewContext("gc") as () => void;

function retainedBytes(run: () => void): number {
  collect();
  const before = process.memoryUsage().heapUsed;
  run();
  collect();
  return process.memoryUsage().heapUsed - before;
}

function buildConversation(turns: number, observationChars: number): Message[] {
  const messages: Message[] = [
    { role: "system", content: "SYSTEM_PROMPT_SENTINEL " + "x".repeat(4000) },
    { role: "user", content: "QUESTION_SENTINEL where is the router defined?" },
  ];
  for (let turn = 0; turn < turns; turn += 1) {
    messages.push({ role: "assistant", content: `{"action":"octocode","tool":"t${turn}"}` });
    messages.push({ role: "user", content: `Observation ${turn}:\n` + "y".repeat(observationChars) });
  }
  return messages;
}

const fit = (messages: Message[], numCtx: number, numPredict: number, extra: Partial<Parameters<typeof fitToContext>[1]> = {}) =>
  fitToContext(messages, { numCtx, numPredict, ...extra });
const count = (text: string, needle: string) => text.split(needle).length - 1;

describe("clip", () => {
  const headMarker = "IMPORTANT_FIRST_LINE";
  const body = headMarker + "\n" + "filler\n".repeat(2000) + "LAST_LINE";
  const clipped = clip(body, 3000);
  it("keeps the head of a long tool result", () => expect(clipped).toContain(headMarker));
  it("keeps the tail too", () => expect(clipped).toContain("LAST_LINE"));
  it("respects the character limit", () => expect(cpLength(clipped)).toBeLessThanOrEqual(3000));
  it("leaves short text untouched", () => expect(clip("short", 3000)).toBe("short"));
  it("has a configurable head/tail ratio", () => {
    const tailHeavy = clip("H".repeat(1000) + "T".repeat(1000), 200, 0.25);
    expect(count(tailHeavy, "T")).toBeGreaterThan(count(tailHeavy, "H"));
  });
  it("rejects invalid ratios even when no clipping is needed", () => {
    expect(() => clip("short", 3000, 0)).toThrow(/head_ratio must be between 0 and 1/);
  });
});

describe("fitToContext", () => {
  const messages = buildConversation(10, 3000);
  const budget = contextBudget(8192, 512);
  const [fitted, compacted] = fit(messages, 8192, 512);
  it("a 10-turn run genuinely overflows num_ctx=8192 before fitting", () => expect(messagesTokens(messages)).toBeGreaterThan(budget));
  it("reports that it compacted", () => expect(compacted).toBe(true));
  it("brings the conversation inside the budget", () => expect(messagesTokens(fitted)).toBeLessThanOrEqual(budget));
  it("keeps the system prompt", () => expect(fitted[0]!.content).toContain("SYSTEM_PROMPT_SENTINEL"));
  it("keeps the original question", () => expect(fitted[1]!.content).toContain("QUESTION_SENTINEL"));
  it("drops no messages, only shrinks them", () => expect(fitted.length).toBe(messages.length));
  it("keeps the two most recent observations verbatim", () => {
    expect(fitted.at(-1)!.content).toBe(messages.at(-1)!.content);
    expect(fitted.at(-3)!.content).toBe(messages.at(-3)!.content);
  });
  it("leaves a breadcrumb for older observations", () => expect(fitted[3]!.content).toContain("compacted"));

  const small = buildConversation(1, 100);
  const [fittedSmall, compactedSmall] = fit(small, 8192, 512);
  it("does not compact a small conversation", () => {
    expect(compactedSmall).toBe(false);
    expect(fittedSmall).toEqual(small);
  });
  it("returns the original prefix object when it fits", () => expect(fittedSmall).toBe(small));

  let recoverable = "Observation (step 7):\nFIRST_EVIDENCE\n" + "middle ".repeat(2000) + "\nLAST_EVIDENCE";
  recoverable += pageFooter(7, 2, 4, 20_000);
  const breadcrumb = compact(recoverable, new ContextPolicy());
  it("compaction retains an exact stored-page recovery action", () => expect(breadcrumb).toContain('{"action":"page","step":7,"page":2}'));
  it("compaction retains completeness state", () => expect(breadcrumb).toContain("page 2 of 4"));
  it("compaction retains evidence at both ends", () => {
    expect(breadcrumb).toContain("FIRST_EVIDENCE");
    expect(breadcrumb).toContain("LAST_EVIDENCE");
  });
  it("compacting a breadcrumb again is byte-stable", () => expect(compact(breadcrumb, new ContextPolicy())).toBe(breadcrumb));
  it("breadcrumb tail is evidence rather than an obsolete turn reminder", () => {
    let single = "Observation (step 8):\nFIRST_EVIDENCE\n" + "middle ".repeat(500) + "\nLAST_EVIDENCE";
    single += "\n\nCommands remaining: 2. Finish now if the task is answerable; do not repeat prior commands.";
    expect(compact(single, new ContextPolicy())).toContain("LAST_EVIDENCE");
  });
  it("overflow compaction reclaims modest headroom when old observations suffice", () => {
    expect(messagesTokens(fitted)).toBeLessThanOrEqual(Math.trunc(budget * 0.9));
  });
  it("next fitting append leaves the compacted prefix unchanged", () => {
    const extended = [...fitted, { role: "assistant", content: '{"action":"finish","answer":"done"}' }];
    const [extendedFit, extendedCompacted] = fit(extended, 8192, 512);
    expect(extendedCompacted).toBe(false);
    expect(extendedFit).toBe(extended);
  });

  const huge = buildConversation(2, 60_000);
  const [fittedHuge] = fit(huge, 8192, 512);
  it("clips even oversized recent observations to fit", () => expect(messagesTokens(fittedHuge)).toBeLessThanOrEqual(contextBudget(8192, 512)));
  it("keeps the system prompt in the pathological case", () => expect(fittedHuge[0]!.content).toContain("SYSTEM_PROMPT_SENTINEL"));
  it("never alters the pinned system prompt for pathological observations", () => expect(fittedHuge[0]).toEqual(huge[0]));
  it("never alters the pinned question for pathological observations", () => expect(fittedHuge[1]).toEqual(huge[1]));

  const pinnedOnly: Message[] = [
    { role: "system", content: "SYSTEM_PINNED " + "s".repeat(40_000) },
    { role: "user", content: "QUESTION_PINNED " + "q".repeat(40_000) },
  ];
  it("fails closed on oversized pinned content by default", () => {
    expect(() => fit(pinnedOnly, 4096, 512)).toThrow(/FREELLAMA_AGENT_PINNED_OVERFLOW=clip/);
  });
  const clipPolicy = new ContextPolicy({ pinned_overflow: "clip" });
  const [fittedPinned, clippedPinned] = fit(pinnedOnly, 4096, 512, { policy: clipPolicy });
  it("pinned clipping requires and honors explicit opt-in", () => expect(clippedPinned).toBe(true));
  it("opt-in pinned clipping fits", () => expect(messagesTokens(fittedPinned, clipPolicy)).toBeLessThanOrEqual(contextBudget(4096, 512)));
  it("opt-in pinned clipping preserves leading contract markers", () => {
    expect(fittedPinned[0]!.content).toContain("SYSTEM_PINNED");
    expect(fittedPinned[1]!.content).toContain("QUESTION_PINNED");
  });
});

describe("repeat detection", () => {
  it("identical calls share a signature regardless of key order", () => {
    expect(callSignature({ tool: "a", queries: { x: 1, y: 2 } })).toBe(callSignature({ queries: { y: 2, x: 1 }, tool: "a" }));
  });
  it("different calls do not collide", () => {
    expect(callSignature({ command: "ls" })).not.toBe(callSignature({ command: "ls -la" }));
  });
});

describe("budgeting", () => {
  const small = buildConversation(1, 100);
  it("token estimate scales with length", () => {
    expect(messagesTokens([{ role: "user", content: "z".repeat(CHARS_PER_TOKEN * 100) }])).toBeGreaterThanOrEqual(100);
  });
  it("budget never goes negative", () => expect(contextBudget(256, 512)).toBe(0));
  const customPolicy = new ContextPolicy({ chars_per_token: 2, safety_margin_tokens: 400, image_token_estimate: 77 });
  it("characters-per-token estimate is configurable", () => expect(messagesTokens(small, customPolicy)).toBeGreaterThan(messagesTokens(small)));
  it("context safety margin is configurable", () => expect(contextBudget(8192, 512, customPolicy.safety_margin_tokens)).toBe(7280));
  it("image token charge is configurable", () => {
    expect(messagesTokens([{ role: "user", content: "", images: ["bytes"] }], customPolicy)).toBeGreaterThanOrEqual(77);
  });

  const budgeter = new ContextBudgeter();
  const calibrationMessages = [{ role: "user", content: "calibrate ".repeat(100) }];
  const uncalibrated = budgeter.estimate(calibrationMessages);
  it("Ollama prompt counts calibrate future estimates", () => {
    budgeter.observe(calibrationMessages, uncalibrated * 2);
    expect(budgeter.estimate(calibrationMessages)).toBe(uncalibrated * 2);
  });
  it("calibration never makes budgeting less conservative", () => {
    budgeter.observe(calibrationMessages, 1);
    expect(budgeter.estimate(calibrationMessages)).toBe(uncalibrated * 2);
  });
});

describe("runtime configuration and calibration", () => {
  const small = buildConversation(1, 100);
  const runtimeEnv: Record<string, string> = {
    FREELLAMA_AGENT_NUM_CTX: "16384",
    FREELLAMA_AGENT_NUM_PREDICT: "768",
    FREELLAMA_AGENT_TEMPERATURE: "0.2",
    FREELLAMA_AGENT_SEED: "7",
    FREELLAMA_AGENT_REQUEST_TIMEOUT_SECONDS: "90",
    FREELLAMA_AGENT_RETRY_ATTEMPTS: "3",
    FREELLAMA_AGENT_RETRY_BACKOFF_SECONDS: "1.5",
    FREELLAMA_AGENT_TOOL_TIMEOUT_SECONDS: "12",
    FREELLAMA_AGENT_KEEP_ALIVE: "0",
    FREELLAMA_AGENT_THINK: "true",
    FREELLAMA_AGENT_MAX_PARSE_REPAIRS: "4",
    FREELLAMA_AGENT_PARSE_REPAIR_ECHO_CHARS: "240",
    FREELLAMA_AGENT_CHARS_PER_TOKEN: "3.5",
    FREELLAMA_AGENT_SAFETY_MARGIN_TOKENS: "300",
    FREELLAMA_AGENT_IMAGE_TOKEN_ESTIMATE: "900",
    FREELLAMA_AGENT_KEEP_RECENT: "3",
    FREELLAMA_AGENT_COMPACT_PREVIEW_CHARS: "120",
    FREELLAMA_AGENT_COMPACT_RETAIN_RATIO: "0.7",
    FREELLAMA_AGENT_CLIP_HEAD_RATIO: "0.6",
    FREELLAMA_AGENT_OBSERVATION_PAGE_CHARS: "2048",
    FREELLAMA_AGENT_PINNED_OVERFLOW: "clip",
  };
  const runtime = AgentRuntimeConfig.fromEnv({ defaultToolTimeoutSeconds: 30, env: runtimeEnv });
  it("runtime schema applies model-call configuration", () => {
    expect([runtime.num_ctx, runtime.num_predict, runtime.think, runtime.parse_repair_echo_chars]).toEqual([16384, 768, true, 240]);
  });
  it("runtime schema applies context configuration", () => {
    expect(runtime.context.observation_page_chars).toBe(2048);
    expect(runtime.context.pinned_overflow).toBe("clip");
    expect(runtime.context.clip_head_ratio).toBe(0.6);
  });
  it("context manager reports resolved policy and calibration", () => {
    const manager = new AgentContextManager(runtime);
    manager.observe(small, messagesTokens(small, runtime.context) * 2);
    expect(manager.metadata().calibration_samples).toBe(1);
    expect(manager.metadata().estimated_input_budget).toBe(15316);
  });

  const calibrationDir = mkdtempSync(path.join(tmpdir(), "freellama-calibration-test-"));
  afterAll(() => rmSync(calibrationDir, { recursive: true, force: true }));
  const firstManager = new AgentContextManager(runtime, { model: "model-a", calibrationDir });
  const baselineEstimate = firstManager.budgeter.estimate(small);
  firstManager.observe(small, baselineEstimate * 2);
  const restartedManager = new AgentContextManager(runtime, { model: "model-a", calibrationDir });
  it("model calibration survives a fresh coding-agent process", () => {
    expect(restartedManager.budgeter.estimate(small)).toBeGreaterThanOrEqual(baselineEstimate * 2);
    expect(restartedManager.metadata().calibration_source).toBe("persistent_model_cache");
  });
  it("persistent token calibration never crosses model templates", () => {
    const otherModel = new AgentContextManager(runtime, { model: "model-b", calibrationDir });
    expect(otherModel.metadata().calibration_source).toBe("current_process");
    expect(otherModel.budgeter.scale).toBe(1);
  });
  it("calibration is isolated when the estimator policy changes", () => {
    const changedRuntime = AgentRuntimeConfig.fromEnv({ defaultToolTimeoutSeconds: 30, env: { ...runtimeEnv, FREELLAMA_AGENT_CHARS_PER_TOKEN: "8" } });
    expect(new AgentContextManager(changedRuntime, { model: "model-a", calibrationDir }).budgeter.scale).toBe(1);
  });
  describe("immutable calibration identity", () => {
    let same: AgentContextManager;
    let changed: AgentContextManager;
    beforeAll(() => {
      const identified = new AgentContextManager(runtime, { model: "model-a", calibrationDir, calibrationIdentity: "digest-a/template-a" });
      identified.observe(small, baselineEstimate * 3);
      same = new AgentContextManager(runtime, { model: "model-a", calibrationDir, calibrationIdentity: "digest-a/template-a" });
      changed = new AgentContextManager(runtime, { model: "model-a", calibrationDir, calibrationIdentity: "digest-b/template-a" });
    });
    it("matching immutable calibration identity reloads", () => expect(same.budgeter.scale).toBeGreaterThanOrEqual(3));
    it("changed calibration identity starts fresh", () => expect(changed.budgeter.scale).toBe(1));
  });
  it("tag-only calibration reports its limited scope", () => {
    expect(restartedManager.metadata().calibration_scope).toBe("model_tag_and_estimator");
  });
  it("malformed calibration records do not prevent loading valid samples", () => {
    writeFileSync(path.join(calibrationDir, `${firstManager.modelKey()}-invalid.json`), "[]", "utf8");
    expect(new AgentContextManager(runtime, { model: "model-a", calibrationDir }).budgeter.scale).toBeGreaterThanOrEqual(2);
  });
  it("non-finite cached sample counts do not break startup", () => {
    const contract = JSON.stringify(firstManager.calibrationContract()).slice(0, -1);
    writeFileSync(path.join(calibrationDir, `${firstManager.modelKey()}-invalid.json`), `${contract}, "scale": 2, "samples": Infinity}`, "utf8");
    expect(new AgentContextManager(runtime, { model: "model-a", calibrationDir }).budgeter.scale).toBeGreaterThanOrEqual(2);
  });
  it("invalid context configuration fails fast", () => {
    expect(() => ContextPolicy.fromEnv({ FREELLAMA_AGENT_COMPACT_RETAIN_RATIO: "1.0" })).toThrow(/compact_retain_ratio/);
  });
  it("empty keep-alive configuration fails fast", () => {
    expect(() => AgentRuntimeConfig.fromEnv({ defaultToolTimeoutSeconds: 30, env: { FREELLAMA_AGENT_KEEP_ALIVE: "" } })).toThrow(/KEEP_ALIVE/);
  });
});

describe("typed coding-agent history", () => {
  const typed: Message[] = [
    { role: "system", content: "SYSTEM_TYPED" },
    { role: "user", content: "TASK_TYPED", images: ["base64-is-not-counted-as-text"] },
    { role: "assistant", content: "", tool_calls: [{ function: { name: "read", arguments: { path: "src/a.py" } } }] },
    { role: "tool", tool_name: "read", content: "z".repeat(40_000) },
  ];
  it("images use a bounded multimodal estimate rather than base64 character count", () => {
    expect(messagesTokens(typed.slice(0, 2))).toBeGreaterThanOrEqual(IMAGE_TOKEN_ESTIMATE);
  });
  const [fittedTyped, compactedTyped] = fit(typed, 4096, 512, { keepRecent: 0 });
  it("typed tool history compacts", () => expect(compactedTyped).toBe(true));
  it("tool role survives compaction", () => expect(fittedTyped[3]!.role).toBe("tool"));
  it("tool name survives compaction", () => expect(fittedTyped[3]!.tool_name).toBe("read"));
  it("assistant tool_calls survive compaction", () => expect(fittedTyped[2]!.tool_calls).toEqual(typed[2]!.tool_calls));
  it("typed history fits after compaction", () => expect(messagesTokens(fittedTyped)).toBeLessThanOrEqual(contextBudget(4096, 512)));
  it("forced finalization is fitted after its instruction is appended", () => {
    const forced = buildConversation(10, 3000);
    forced.push({ role: "user", content: "Return the final answer now." });
    expect(messagesTokens(fit(forced, 8192, 512)[0])).toBeLessThanOrEqual(contextBudget(8192, 512));
  });
});

describe("pagination loses nothing", () => {
  const big = Array.from({ length: 900 }, (_, i) => `src/mod_${i}.py:${i}: def handler_${i}(request, ctx):`).join("\n");
  const pages: string[] = [];
  let total = 0;
  for (let page = 1; ; page += 1) {
    const [chunk, got, tot] = paginate(big, page);
    total = tot;
    pages.push(chunk);
    if (got >= tot) break;
  }
  it("covers a large observation in several pages", () => expect(total).toBeGreaterThan(1));
  it("reassembles pages byte-identical to the input", () => expect(pages.join("")).toBe(big));
  it("never exceeds the page budget", () => expect(pages.every((chunk) => cpLength(chunk) <= 3000)).toBe(true));
  it("splits pages on line boundaries, never mid-line", () => expect(pages.slice(0, -1).every((chunk) => chunk.endsWith("\n"))).toBe(true));
  it("keeps a short observation on a single page", () => expect(paginate("one line")[2]).toBe(1));
  it("returns one empty page for empty input", () => expect(paginate("")).toEqual(["", 1, 1]));
  it("clamps page numbers instead of erroring", () => expect(paginate(big, 9999)[1]).toBe(total));
  const longLine = "prefix\n" + "界".repeat(10_000) + "\r\ntail";
  const longLineTotal = paginate(longLine, 1, 128)[2];
  const longLinePages = Array.from({ length: longLineTotal }, (_, i) => paginate(longLine, i + 1, 128)[0]);
  it("oversized Unicode lines obey the hard page limit", () => expect(longLinePages.every((chunk) => cpLength(chunk) <= 128)).toBe(true));
  it("oversized-line pagination remains lossless", () => expect(longLinePages.join("")).toBe(longLine));
  it("rejects a non-positive page size even for empty input", () => expect(() => paginate("", 1, 0)).toThrow(/page size/));

  const foot = pageFooter(3, 1, 4, cpLength(big));
  it("footer names the page and the total", () => expect(foot).toContain("page 1 of 4"));
  it("footer gives an exact next-page action", () => expect(foot).toContain('"action":"page","step":3,"page":2'));
  it("footer states nothing was discarded", () => expect(foot).toContain("nothing discarded"));
  it("footer says paging does not re-run the command", () => expect(foot).toContain("does NOT re-run"));
  it("a single page gets no footer at all", () => expect(pageFooter(1, 1, 1, 10)).toBe(""));
  const finalFoot = pageFooter(3, 4, 4, cpLength(big));
  it("final page explicitly ends pagination", () => expect(finalFoot).toContain("End of stored output"));
  it("final page never instructs a wraparound", () => {
    expect(finalFoot).not.toContain('"action":"page"');
    expect(finalFoot).not.toContain("next page");
  });

  describe("ObservationStore", () => {
    const store = new ObservationStore();
    const smallPages = new ObservationStore(128);
    afterAll(() => {
      store.close();
      smallPages.close();
    });
    store.put(1, big);
    store.put(2, "small");
    const [body, footer] = store.view(1, 2);
    it("serves an arbitrary page of a stored observation", () => expect(body).toBe(pages[1]));
    it("advertises page 3 next", () => expect(footer).toContain('"page":3'));
    it("returns a full short observation with no footer", () => expect(store.view(2)).toEqual(["small", ""]));
    it("round-trips the original exactly", () => expect(store.get(1)).toBe(big));
    const [missingBody] = store.view(99);
    it("explains an unknown step rather than returning empty", () => expect(missingBody).toContain("no stored output"));
    it("lists the steps that do exist for an unknown step", () => expect(missingBody).toContain("1, 2"));
    it("has a configurable observation page size", () => {
      smallPages.put(1, big);
      const [, smallFooter] = smallPages.view(1);
      expect(smallFooter).toContain("page 1 of");
      expect(smallFooter).not.toContain("page 1 of 1 ");
    });
  });

  describe("disk spool", () => {
    // The hot loop retains page descriptors, not every full observation or page offset.
    // Decoding yields a flat string, so V8 rope flattening is not charged to the store.
    const diskPayload = Buffer.from("開始\n" + "🦙\"\\\r\n".repeat(100_000) + "END", "utf8").toString("utf8");
    const diskStore = new ObservationStore(128);
    const spoolDirectory = diskStore.directory;
    const resultRoot = mkdtempSync(path.join(tmpdir(), "freellama-result-test-"));
    const resultPath = path.join(resultRoot, "result.json");
    afterAll(() => rmSync(resultRoot, { recursive: true, force: true }));

    it("spools with bounded additional memory", () => {
      writes.largest = 0;
      const retained = retainedBytes(() => diskStore.put(1, diskPayload, { action: "shell", status: "ok", source: "src/router.py" }));
      expect(retained).toBeLessThan(1_000_000);
      // One page (128 code points of at most 4 UTF-8 bytes) per write, never the whole output.
      expect(writes.largest).toBeLessThanOrEqual(128 * 4);
    });
    it("keeps full Unicode output exact", () => expect(diskStore.get(1)).toBe(diskPayload));
    it("supports arbitrary page seeks losslessly", () => {
      const recovered: string[] = [];
      for (let page = diskStore.pageCount(1); page > 0; page -= 1) recovered.push(diskStore.view(1, page)[0]);
      expect(recovered.reverse().join("")).toBe(diskPayload);
    });
    it("structured compaction preserves action, status, source and page", () => {
      const ledgerCompact = compact(diskStore.message(1, 2), new ContextPolicy());
      for (const part of ['"action":"shell"', '"status":"ok"', '"source":"src/router.py"', '"page":2']) expect(ledgerCompact).toContain(part);
    });
    const result = { final_answer: "ok", tool_calls: [{ name: "shell", status: "ok" }], usage: { input_tokens: 1 } };
    it("streams the final audit JSON with bounded memory", () => {
      writes.largest = 0;
      const retained = retainedBytes(() => diskStore.writeResult(resultPath, result));
      expect(retained).toBeLessThan(2_000_000);
      // 64 KiB read chunks, each JSON-escaped (at most 6 bytes per input byte), per write.
      expect(writes.largest).toBeLessThan(2_000_000);
      expect(writes.largest).toBeLessThan(Buffer.byteLength(diskPayload));
    });
    it("preserves the public JSON shape in the streamed final result", () => {
      expect(JSON.parse(readFileSync(resultPath, "utf8"))).toEqual({ ...result, tool_calls: [{ name: "shell", status: "ok", result: diskPayload }] });
    });
    describe("failed finalization", () => {
      let previous: Buffer;
      beforeAll(() => {
        previous = readFileSync(resultPath);
        expect(() => diskStore.writeResult(resultPath, { tool_calls: [{ name: "shell" }, { name: "missing" }] })).toThrow();
      });
      it("leaves the previous result intact", () => expect(readFileSync(resultPath).equals(previous)).toBe(true));
      it("removes its partial output", () => expect(readdirSync(resultRoot).filter((name) => name.endsWith(".tmp"))).toEqual([]));
    });
    it("removes its temporary spool on close", () => {
      diskStore.close();
      expect(existsSync(spoolDirectory)).toBe(false);
    });
    it("cleans the observation spool on an exceptional exit", () => {
      let failedDirectory = "";
      expect(() => {
        const failing = new ObservationStore();
        failedDirectory = failing.directory;
        try {
          throw new Error("test cleanup");
        } finally {
          failing.close();
        }
      }).toThrow("test cleanup");
      expect(existsSync(failedDirectory)).toBe(false);
    });
  });
});


