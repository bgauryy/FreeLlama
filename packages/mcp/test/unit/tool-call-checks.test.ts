import { describe, expect, it } from "vitest";
import { resultChecks } from "../../src/result-checks.js";

function check(parameters: unknown, args: unknown) {
  const calls = [{ function: { name: "reserve", arguments: args } }];
  return resultChecks({ answer: "", structured: { response: { message: { tool_calls: calls } } },
    tools: [{ type: "function", function: { name: "reserve", parameters } }] });
}

describe("tool argument structural checks", () => {
  it("recurses into objects and arrays and enforces closed empty objects", () => {
    const schema = { type: "object", required: ["request"], additionalProperties: false, properties: {
      request: { type: "object", required: ["days", "cities"], additionalProperties: false, properties: {
        days: { type: "integer", minimum: 1, maximum: 7 }, cities: { type: "array", minItems: 1, items: { type: "string" } },
      } },
    } };
    expect(check(schema, { request: { days: 0, cities: ["Paris", 17], extra: 1 } }).join("\n"))
      .toMatch(/request passes extra[\s\S]*request.days violates minimum 1[\s\S]*request.cities\[1\] is 17, not "string"/);
    expect(check({ type: "object", properties: {}, additionalProperties: false }, { extra: 1 })[0]).toMatch(/passes extra/);
  });

  it("accepts JSON Schema open objects, object enums, and nullable values", () => {
    expect(check({ type: "object", properties: { city: { type: ["string", "null"] } } }, { city: null, allowed: 1 })[0]).toMatch(/supported structural checks/);
    expect(check({ type: "object", properties: { data: { enum: [{ a: 1, b: 2 }] } } }, { data: { b: 2, a: 1 } })[0]).toMatch(/supported structural checks/);
  });

  it("reports unsupported schema keywords as unchecked", () => {
    expect(check({ type: "object", properties: { city: { type: "string", pattern: "^[A-Z]" } } }, { city: "paris" })[0]).toMatch(/keyword pattern is unchecked/);
    expect(check({ anyOf: [{ required: ["a"] }, { required: ["b"] }] }, {})[0]).toMatch(/keyword anyOf is unchecked/);
    expect(check({ $ref: "https://example.invalid/schema" }, {})[0]).toMatch(/keyword \$ref is unchecked/);
  });

  it("does not silently pass malformed supported schema keyword values", () => {
    for (const schema of [{ enum: "x" }, { minimum: "oops" }, { required: 5 }, { properties: [] },
      { additionalProperties: 7 }, { items: [] }, { minLength: -1 }, { type: ["string", "mystery"] }]) {
      expect(check(schema, {})[0], JSON.stringify(schema)).toMatch(/unsupported shape.*unchecked/);
      expect(check(schema, {}).join(" ")).not.toMatch(/match the tool schemas/);
    }
  });

  it("does not turn absent structured calls or missing arguments into a pass", () => {
    expect(resultChecks({ answer: "tool_calls: []", structured: {}, tools: [{ function: { name: "reserve" } }] })[0]).toMatch(/no structured calls/);
    expect(check({ type: "object" }, undefined)[0]).toMatch(/not a JSON object/);
    expect(check({ type: "object", properties: { days: { type: "number" } } }, { days: Infinity })[0]).toMatch(/not "number"/);
  });

  it("flags malformed native tool-call containers without mistaking unrelated fields for calls", () => {
    for (const tool_calls of [{ function: { name: "reserve", arguments: {} } }, "[]", false, 17]) {
      expect(resultChecks({ answer: "READY", structured: { response: { message: { tool_calls } } } })[0])
        .toMatch(/response.message.tool_calls is not an array/);
    }
    expect(resultChecks({ answer: "READY", structured: { response: { tool_calls: "unrelated" } } })).toEqual([]);
    expect(resultChecks({ answer: "READY", structured: { response: { message: { tool_calls: null } } } })).toEqual([]);
  });

  it("does not coerce malformed call names or choose between duplicate definitions", () => {
    const tools = [{ type: "function", function: { name: "42", parameters: {} } }];
    const structured = { response: { message: { tool_calls: [{ function: { name: 42, arguments: {} } }] } } };
    expect(resultChecks({ answer: "", structured, tools })[0]).toMatch(/function name is not a nonempty string/);
    expect(resultChecks({ answer: "", structured, tools: [...tools, ...tools] })[0]).toMatch(/duplicate tool names/);
  });

  it("enforces constants, lengths, bounds, booleans and required own properties", () => {
    expect(check({ type: "object", required: ["toString"], additionalProperties: false, properties: {} }, {})[0]).toMatch(/lacks required toString/);
    expect(check({ type: "object", properties: { code: { const: "A", minLength: 2 }, days: { type: "integer", exclusiveMaximum: 3 } } }, { code: "B", days: 3 }).join("\n"))
      .toMatch(/differs from const[\s\S]*violates minLength[\s\S]*violates exclusiveMaximum/);
    expect(check({ type: "object", properties: { forbidden: false } }, { forbidden: true })[0]).toMatch(/forbidden by its schema/);
  });
});
