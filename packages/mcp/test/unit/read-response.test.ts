import { mkdir, mkdtemp, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { ResponseStore } from "../../src/response-store.js";
import { page, readResponse } from "../../src/tools/read-response.js";

async function savedResponse() {
  const root = await mkdtemp(path.join(tmpdir(), "read-response-"));
  const store = new ResponseStore(root, 16 * 1024 * 1024);
  const file = await store.save("run_task", {
    tool: "run_task",
    arguments: { prompt: "p" },
    result: {
      content: [{ type: "text", text: "[m] a" }],
      structuredContent: {
        selected_model: "m",
        route: { selected_model: "m", rejected: [{ model: "n" }] },
        response: { message: { content: "a", thinking: "abcdefghij" }, embeddings: [[1, 2], [3, 4], [5, 6]] },
        observations: [{ step: 1, result: "r1" }, { step: 2, result: "r2" }],
      },
    },
  });
  return { root, file: file! };
}

type View = { isError?: boolean; content: { text: string }[]; structuredContent?: Record<string, any> };

describe("read_response", () => {
  it("returns named sections, top-level fields and JSON pointers", async () => {
    const { root, file } = await savedResponse();
    const thinking = await readResponse({ file, section: "thinking" }, root) as View;
    expect(thinking.structuredContent).toMatchObject({ section: "thinking", type: "string", total: 10, value: "abcdefghij", next_offset: null });
    expect(thinking.content[0]!.text).toBe("abcdefghij");
    const route = await readResponse({ file, section: "route" }, root) as View;
    expect(route.structuredContent!.value).toEqual({ selected_model: "m", rejected: [{ model: "n" }] });
    const pointed = await readResponse({ file, section: "/result/structuredContent/route/rejected/0/model" }, root) as View;
    expect(pointed.structuredContent!.value).toBe("n");
    const args = await readResponse({ file, section: "arguments" }, root) as View;
    expect(args.structuredContent!.value).toEqual({ prompt: "p" });
  });

  it("pages strings by character and arrays by item", async () => {
    const { root, file } = await savedResponse();
    const first = await readResponse({ file, section: "thinking", limit: 4 }, root) as View;
    expect(first.structuredContent).toMatchObject({ value: "abcd", offset: 0, next_offset: 4, total: 10 });
    expect(first.content[0]!.text).toContain("[more: offset 4 of 10]");
    const last = await readResponse({ file, section: "thinking", offset: 8, limit: 4 }, root) as View;
    expect(last.structuredContent).toMatchObject({ value: "ij", next_offset: null });
    const vectors = await readResponse({ file, section: "embeddings", offset: 1, limit: 1 }, root) as View;
    expect(vectors.structuredContent).toMatchObject({ type: "array", value: [[3, 4]], next_offset: 2, total: 3 });
  });

  it("pages a large object as JSON text and rejoins losslessly", () => {
    const value = { text: "x".repeat(30_000) };
    const first = page(value);
    expect(first.type).toBe("json_text");
    const second = page(value, first.next_offset as number);
    expect(JSON.parse(String(first.value) + String(second.value))).toEqual(value);
    expect(page({ small: true })).toEqual({ type: "json", value: { small: true } });
  });

  it("outlines the file without a section and names the sections on a miss", async () => {
    const { root, file } = await savedResponse();
    const outline = await readResponse({ file }, root) as View;
    const names = (outline.structuredContent!.sections as { name: string }[]).map((entry) => entry.name);
    expect(names).toEqual(expect.arrayContaining(["arguments", "receipt", "thinking", "embeddings", "observations", "route"]));
    const missing = await readResponse({ file, section: "nope" }, root) as View;
    expect(missing.isError).toBe(true);
    expect(missing.content[0]!.text).toMatch(/No section nope.*Sections: arguments/);
  });

  it("refuses files outside the response root, including through symlinks", async () => {
    const { root, file } = await savedResponse();
    const outside = await mkdtemp(path.join(tmpdir(), "outside-"));
    const secret = path.join(outside, "secret.json");
    await writeFile(secret, JSON.stringify({ tool: "x", result: {} }));
    for (const target of [secret, path.join(root, "..", path.basename(outside), "secret.json"), "../secret.json"]) {
      const refused = await readResponse({ file: target }, root) as View;
      expect(refused.isError, target).toBe(true);
    }
    const link = path.join(path.dirname(file), "000099-link.json");
    await symlink(secret, link);
    expect(((await readResponse({ file: link }, root)) as View).isError).toBe(true);
    await mkdir(path.join(root, "dir.json"));
    expect(((await readResponse({ file: path.join(root, "dir.json") }, root)) as View).isError).toBe(true);
    // A path relative to the root is accepted.
    const relative = path.relative(root, file);
    expect(((await readResponse({ file: relative, section: "thinking" }, root)) as View).isError).toBeUndefined();
  });
});
