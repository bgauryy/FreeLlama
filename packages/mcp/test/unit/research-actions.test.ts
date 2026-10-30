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
      '{"action":"octocode","tool":"localSearchCode","queries":{}}',
    ]) expectValueError(() => parseBashAction(invalid));
  });

  it("octocode accepts only complete declared actions", () => {
    const action = parseOctocodeAction('{"action":"octocode","tool":"localSearchCode","queries":{"keywords":["router"]}}');
    expect(action.tool).toBe("localSearchCode");
    for (const invalid of [
      '{"action":"octocode","tool":"unknown","queries":{}}',
      '{"action":"octocode","tool":"localSearchCode"}',
      '{"action":"page","step":"1","page":2}',
      '{"action":"finish","answer":7}',
      '{"action":"shell","command":"ls"}',
    ]) expectValueError(() => parseOctocodeAction(invalid));
  });
});
