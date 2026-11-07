// Action-schema contracts shared by the bundled research adapters.
import { describe, expect, it } from "vitest";
import { parseAction as parseBashAction } from "../../src/research/bash-agent.js";
import { parseAction as parseOctocodeAction } from "../../src/research/octocode-agent.js";
import { ValidationError } from "../../src/research/errors.js";

const expectValueError = (run: () => unknown) => {
  let caught: unknown;
  try {
    run();
  } catch (error) {
    caught = error;
  }
  expect(caught).toBeInstanceOf(ValidationError);
};

describe("action schemas", () => {
  it("bash accepts only complete declared actions", () => {
    expect(parseBashAction('{"action":"shell","command":"grep -n x src/a.py"}').action).toBe("shell");
    expect(parseBashAction('{"action":"page","step":1,"page":2}').page).toBe(2);
    expect(parseBashAction('{"action":"finish","answer":"done"}').answer).toBe("done");
    for (const invalid of [
      '{"action":"shell","command":""}',
      '{"action":"page","step":0,"page":1}',
      '{"action":"finish"}',
      '{"action":"octocode","tool":"localSearch","queries":[{}]}',
    ]) expectValueError(() => parseBashAction(invalid));
  });

  it("octocode accepts only complete declared actions", () => {
    const action = parseOctocodeAction('{"action":"octocode","tool":"localSearch","queries":[{"path":"src","matchString":["router"]}]}');
    expect(action.tool).toBe("localSearch");
    // The shapes small models write: the tool as the action, or query fields at the top level.
    expect(parseOctocodeAction('{"action":"lspSearch","queries":[{"path":"a.rs","symbolName":"x","line":1}]}')).toMatchObject({ action: "octocode", tool: "lspSearch" });
    expect(parseOctocodeAction('{"action":"localSearch","path":"src","matchString":["x"]}')).toMatchObject({ action: "octocode", tool: "localSearch", queries: { path: "src", matchString: ["x"] } });
    expect(parseOctocodeAction('{"action":"octocode","tool":"localFetch","path":"a.rs"}')).toMatchObject({ queries: { path: "a.rs" } });
    // A small model's bare query object is accepted as a one-query list.
    expect(parseOctocodeAction('{"action":"octocode","tool":"localSearch","queries":{"path":"src","matchString":["x"]}}').tool).toBe("localSearch");
    for (const invalid of [
      '{"action":"octocode","tool":"unknown","queries":[{}]}',
      '{"action":"octocode","tool":"localSearch","queries":[]}',
      '{"action":"octocode","tool":"localSearchCode","queries":[{}]}',
      '{"action":"octocode","tool":"ghCloneRepo","queries":[{}]}',
      '{"action":"octocode","tool":"graphBuild","queries":[{}]}',
      '{"action":"page","step":"1","page":2}',
      '{"action":"finish","answer":7}',
      '{"action":"shell","command":"ls"}',
    ]) expectValueError(() => parseOctocodeAction(invalid));
  });
});

describe("adapter results with query lists", () => {
  it("parses a tool call whose queries are the CLI's array", async () => {
    const { parseAdapterResult } = await import("../../src/delegate.js");
    const result = parseAdapterResult(JSON.stringify({ final_answer: "a", tool_calls: [
      { raw_name: "octocode", status: "ok", arguments: { tool: "localSearch", queries: [{ path: "src", matchString: ["x"] }] } },
    ] }));
    expect(result.tool_calls).toHaveLength(1);
  });
});
