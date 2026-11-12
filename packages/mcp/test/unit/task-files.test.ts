import { mkdtempSync, mkdirSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { applyTaskFiles, chunkLines, expandFileEntries, fileRoots, inputLabels } from "../../src/task-files.js";
import { nearestText } from "../../src/nearest.js";

const root = mkdtempSync(path.join(tmpdir(), "freellama-files-"));
const file = (name: string, content: string | Buffer) => {
  const at = path.join(root, name);
  writeFileSync(at, content);
  return at;
};
const PNG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

describe("files on run_task", () => {
  const roots = [root];

  it("embeds each text file as one item, after the given input", () => {
    const a = file("a.rs", "fn a() {}");
    const b = file("b.rs", "fn b() {}");
    const { payload, files } = applyTaskFiles({ task: "embedding", input: "query" }, [a, b], roots);
    expect(payload.input).toEqual(["query", `${a}\nfn a() {}`, `${b}\nfn b() {}`]);
    expect(files.map((entry) => entry.role)).toEqual(["input", "input"]);
    expect(inputLabels(3, files)).toEqual(["input[0]", `${a}:L1-1`, `${b}:L1-1`]);
    // Given items show the start of their own text, without the prefix FreeLlama added.
    expect(inputLabels(2, [], ["search_document: App crashes when uploading a PNG larger than 10MB", "  short  "], "search_document: "))
      .toEqual(['input[0] "App crashes when uploading a PNG larger than 10…"', 'input[1] "short"']);
  });

  it("embeds an image or WAV file as one multimodal item, in the order named", () => {
    const note = file("note2.txt", "caption");
    const shot = file("shot2.png", PNG);
    const clip = file("clip.wav", "RIFF0000WAVE");
    const { payload, files } = applyTaskFiles({ task: "embedding" }, [note, shot, clip], roots);
    expect(payload.input).toEqual([`${note}\ncaption`, { image: PNG.toString("base64") }, { audio: Buffer.from("RIFF0000WAVE").toString("base64") }]);
    expect(inputLabels(3, files)).toEqual([`${note}:L1-1`, shot, clip]);
  });

  it("labels decision WAV as audio while preserving Ollama's images transport", () => {
    const clip = file("decision.wav", "RIFF0000WAVE");
    const { payload, files } = applyTaskFiles({ task: "decision", state: "listen" }, [clip], roots);
    expect(payload.images).toEqual([Buffer.from("RIFF0000WAVE").toString("base64")]);
    expect(files[0]!.role).toBe("audio");
    expect(() => applyTaskFiles({ task: "decision" }, [file("mislabeled.wav", PNG)], roots)).toThrow(/RIFF\/WAVE/);
  });

  it("heads each chunk with the file's opening comment", () => {
    const source = file("holders.rs", "//! The programs holding the most memory.\n//! Sampled with ps.\nuse std::fs;\nfn sample() {}");
    const { payload } = applyTaskFiles({ task: "embedding" }, [source], roots);
    expect((payload.input as string[])[0]).toBe(`${source}\nThe programs holding the most memory. Sampled with ps.\n//! The programs holding the most memory.\n//! Sampled with ps.\nuse std::fs;\nfn sample() {}`);
  });

  it("splits a long file into line chunks under the embedding limit, prefixed and labelled by line range", () => {
    const long = file("long.rs", Array.from({ length: 400 }, (_, index) => `fn item_${index}() { /* body */ }`).join("\n"));
    const { payload, files } = applyTaskFiles({ task: "embedding", documentPrefix: "search_document: " }, [long], roots);
    const items = payload.input as string[];
    expect(items.length).toBeGreaterThan(2);
    // Each chunk is headed by the file's path (and opening comment, none here).
    expect(items.every((item) => item.startsWith(`search_document: ${long}\nfn item_`) && Buffer.byteLength(item) <= 1_536 + 17 + long.length + 1)).toBe(true);
    expect(files[0]!.lines).toEqual([1, files[0]!.lines![1]]);
    expect(files.at(-1)!.lines![1]).toBe(400);
    expect(chunkLines("x".repeat(10_000), 4_096).length).toBe(5);
  });

  it("appends text to the prompt and turns images into base64 images", () => {
    const log = file("build.log", "error[E0308]: mismatched types");
    const shot = file("shot.png", PNG);
    const { payload, files } = applyTaskFiles({ task: "vision", prompt: "What failed?" }, [log, shot], roots);
    expect(payload.prompt).toBe(`What failed?\n\n--- file: ${log} ---\nerror[E0308]: mismatched types`);
    expect(payload.images).toEqual([PNG.toString("base64")]);
    expect(files.map((entry) => entry.role)).toEqual(["image", "prompt"]);
  });

  it("makes one file the decision state, keeping JSON as JSON, and several a state keyed by path", () => {
    const issue = file("issue.json", '{"title":"Crash on save"}');
    const note = file("note.txt", "plain");
    expect(applyTaskFiles({ task: "decision" }, [issue], roots).payload.state).toEqual({ title: "Crash on save" });
    expect(applyTaskFiles({ task: "decision" }, [issue, note], roots).payload.state)
      .toEqual({ [issue]: '{"title":"Crash on save"}', [note]: "plain" });
    expect(() => applyTaskFiles({ task: "decision", state: "x" }, [note], roots)).toThrow(/either as `state`/);
  });

  it("refuses what it cannot read faithfully", () => {
    const binary = file("blob.bin", Buffer.from([1, 0, 2]));
    const outside = mkdtempSync(path.join(tmpdir(), "freellama-outside-"));
    const secret = path.join(outside, "secret.txt");
    writeFileSync(secret, "s");
    mkdirSync(path.join(root, "links"), { recursive: true });
    const link = path.join(root, "links", "escape.txt");
    symlinkSync(secret, link);
    expect(() => applyTaskFiles({ task: "completion", prompt: "p" }, [binary], roots)).toThrow(/binary/);
    expect(() => applyTaskFiles({ task: "completion", prompt: "p" }, [secret], roots)).toThrow(/outside the readable roots/);
    expect(() => applyTaskFiles({ task: "completion", prompt: "p" }, [link], roots)).toThrow(/outside the readable roots/);
    expect(() => applyTaskFiles({ task: "completion", prompt: "p" }, [path.join(root, "missing")], roots)).toThrow(/not found/);
    expect(() => applyTaskFiles({ task: "completion" }, [file("t.txt", "t")], roots)).toThrow(/give the instructions in `prompt`/);
    expect(() => applyTaskFiles({ task: "vision", prompt: "p" }, [file("a.wav", "RIFF0000WAVE")], roots)).toThrow(/decision/);
  });

  it("expands globs from the working directory, sorted, and refuses a glob that matches nothing", () => {
    const globRoot = mkdtempSync(path.join(tmpdir(), "freellama-glob-"));
    mkdirSync(path.join(globRoot, "src", "a"), { recursive: true });
    writeFileSync(path.join(globRoot, "src", "b.rs"), "b");
    writeFileSync(path.join(globRoot, "src", "a", "c.rs"), "c");
    writeFileSync(path.join(globRoot, "src", "d.md"), "d");
    expect(expandFileEntries(["src/**/*.rs"], globRoot)).toEqual([
      path.join(globRoot, "src", "a", "c.rs"), path.join(globRoot, "src", "b.rs"),
    ]);
    expect(() => expandFileEntries(["src/**/*.py"], globRoot)).toThrow(/no files match/);
  });

  it("reads under the working directory, the temp directory, FREELLAMA_FILE_ROOTS, the handoffs and the default output directory", () => {
    const roots = fileRoots({ FREELLAMA_FILE_ROOTS: [root, "/nonexistent-root"].join(path.delimiter), FREELLAMA_HOME: root }, root);
    expect(roots).toContain(realpathSync(root));
    expect(roots.at(-2)).toBe(path.join(realpathSync(root), "handoffs"));
    expect(roots.at(-1)).toBe(path.join(realpathSync(root), "tmp"));
    expect(roots.length).toBe(6);
  });

  it("does not treat a working directory of / or the home directory as a project root", () => {
    const home = mkdtempSync(path.join(tmpdir(), "freellama-home-"));
    expect(fileRoots({}, "/", home)).not.toContain("/");
    expect(fileRoots({}, home, home)).not.toContain(realpathSync(home));
    expect(fileRoots({}, root, home)).toContain(realpathSync(root));
  });
});

describe("nearest text", () => {
  // The ranking itself is the core's (rust-core platform/execution/nearest_tests.rs).
  it("prints one line per match or pair, and says when nothing reached minScore", () => {
    expect(nearestText({ mode: "query", matches: [{ index: 2, label: "c", score: 0.999 }, { index: 0, label: "a", score: 0.994 }] }))
      .toBe("0.999  c\n0.994  a");
    expect(nearestText({ mode: "query", matches: [] })).toBe("no item reached minScore");
    expect(nearestText({ mode: "pairs", pairs: [{ a: { index: 0, label: "w" }, b: { index: 3, label: "z" }, score: 0.999 }] }))
      .toBe("0.999  w  ~  z");
    expect(nearestText({ mode: "pairs", pairs: [] })).toBe("no pair reached minScore");
  });
});
