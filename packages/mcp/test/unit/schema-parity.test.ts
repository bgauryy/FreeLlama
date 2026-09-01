import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { REQUIRED_CAPABILITIES, TASK_KINDS } from "../../src/helpers.js";
import { REPO_ROOT } from "../../src/config.js";

function rustEnumMembers(file: string, name: string): string[] {
  const source = readFileSync(path.join(REPO_ROOT, file), "utf8");
  const body = source.match(new RegExp(`pub enum ${name} \\{([\\s\\S]*?)\\n\\}`))?.[1];
  if (!body) throw new Error(`could not find Rust enum ${name} in ${file}`);
  return body
    .replace(/#\[[^\]]+\]/g, "")
    .split(",")
    .map((entry) => entry.trim().match(/^([A-Z][A-Za-z0-9]*)/)?.[1])
    .filter((entry): entry is string => Boolean(entry))
    .map((entry) => entry.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase());
}

describe("MCP schema parity with Rust routing types", () => {
  it("advertises every TaskKind and no unsupported task strings", () => {
    expect([...TASK_KINDS]).toEqual(
      rustEnumMembers("packages/rust-core/src/platform/routing.rs", "TaskKind"),
    );
  });

  it("advertises every routable capability and excludes the internal Other bucket", () => {
    const capabilities = rustEnumMembers("packages/rust-core/src/model_bench.rs", "Capability").filter(
      (capability) => capability !== "other",
    );
    expect([...REQUIRED_CAPABILITIES]).toEqual(capabilities);
  });
});
