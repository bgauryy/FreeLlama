import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "../../../..");

const camel = (name: string) => name.replace(/_([a-z])/g, (_, letter: string) => letter.toUpperCase());

describe("native/index.d.ts", () => {
  it("declares exactly the functions napi.rs exports", () => {
    const napi = readFileSync(path.join(repo, "packages/rust-core/src/napi.rs"), "utf8");
    const dts = readFileSync(path.join(repo, "packages/mcp/native/index.d.ts"), "utf8");
    const exported = [...napi.matchAll(/#\[napi(?:\([^\]]*\))?\](?:\s*#\[[^\]]*\])*\s*pub (?:async )?fn (\w+)/g)].map((match) => camel(match[1]));
    const declared = [...dts.matchAll(/^export (?:declare )?function (\w+)/gm)].map((match) => match[1]);
    expect(exported.length).toBeGreaterThan(10);
    expect([...declared].sort()).toEqual([...exported].sort());
  });
});
