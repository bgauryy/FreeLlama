import { mkdtempSync, realpathSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { segments, validateCommand } from "../../src/research/sandbox.js";

const root = realpathSync(mkdtempSync(path.join(tmpdir(), "freellama-parens-")));
writeFileSync(path.join(root, "a.ts"), "robots");

describe("literal parentheses in research commands", () => {
  it("accepts find's escaped grouping and quoted parentheses as arguments", () => {
    expect(segments(String.raw`find . \( -name '*.ts' -o -name '*.rs' \)`)).toEqual([["find", ".", "(", "-name", "*.ts", "-o", "-name", "*.rs", ")"]]);
    expect(() => validateCommand(root, String.raw`find . -type f \( -name "*.ts" -o -name "*.js" \) 2>/dev/null | head -30`)).not.toThrow();
    expect(() => validateCommand(root, `grep -rn "fn main(" .`)).not.toThrow();
    expect(() => validateCommand(root, `grep -rn 'catch (e)' .`)).not.toThrow();
  });

  it("still refuses bare parentheses and every substitution, quoted or not", () => {
    expect(() => validateCommand(root, "(cat a.ts)")).toThrow(/shell operator/);
    expect(() => validateCommand(root, "find . ( -name x )")).toThrow(/shell operator/);
    expect(() => validateCommand(root, `grep "$(id)" .`)).toThrow();
    expect(() => validateCommand(root, "grep \"`id`\" .")).toThrow();
    expect(() => validateCommand(root, `cat <(echo x)`)).toThrow();
  });

  it("points a refused xargs at a direct content search", () => {
    expect(() => validateCommand(root, "find . -name '*.ts' | xargs grep -l robots")).toThrow(/grep -rln --include/);
  });
});
