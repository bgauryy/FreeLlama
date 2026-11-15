type Fields = Record<string, unknown>;
const object = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? value as Fields : undefined;
const list = (value: unknown): unknown[] => Array.isArray(value) ? value : [];
const owns = (value: Fields, key: string): boolean => Object.hasOwn(value, key);

export const TOOL_CALL_WITHOUT_TOOLS = "the model answered with a tool call although no tools were sent: discard it and rerun on a stronger pick that fits (models {view:\"catalog\", job}) or pass `model`";

function hasType(value: unknown, type: unknown): boolean {
  if (Array.isArray(type)) return type.some((one) => hasType(value, one));
  switch (type) {
    case "string": return typeof value === "string";
    case "integer": return typeof value === "number" && Number.isFinite(value) && Number.isInteger(value);
    case "number": return typeof value === "number" && Number.isFinite(value);
    case "boolean": return typeof value === "boolean";
    case "array": return Array.isArray(value);
    case "object": return object(value) !== undefined;
    case "null": return value === null;
    default: return false;
  }
}

const equal = (a: unknown, b: unknown): boolean => {
  if (a === b) return true;
  if (Array.isArray(a) && Array.isArray(b)) return a.length === b.length && a.every((value, i) => equal(value, b[i]));
  const aa = object(a), bb = object(b);
  return !!aa && !!bb && Object.keys(aa).length === Object.keys(bb).length && Object.keys(aa).every((key) => owns(bb, key) && equal(aa[key], bb[key]));
};

// Explicit subset: unsupported validation keywords produce an unchecked warning, never a pass.
const SUPPORTED = new Set(["type", "properties", "required", "additionalProperties", "items", "enum", "const",
  "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "minLength", "maxLength", "minItems", "maxItems",
  "description", "title", "default", "examples", "$comment"]);

function malformedKeywords(schema: Fields): string[] {
  const names = new Set(["string", "integer", "number", "boolean", "array", "object", "null"]);
  const types = Array.isArray(schema.type) ? schema.type : [schema.type];
  const bad = (key: string, valid: boolean) => owns(schema, key) && !valid ? [key] : [];
  return [
    ...bad("type", types.length > 0 && types.every((type) => typeof type === "string" && names.has(type))),
    ...bad("properties", object(schema.properties) !== undefined),
    ...bad("required", Array.isArray(schema.required) && schema.required.every((key) => typeof key === "string") && new Set(schema.required).size === schema.required.length),
    ...bad("enum", Array.isArray(schema.enum) && schema.enum.length > 0),
    ...["additionalProperties", "items"].flatMap((key) => bad(key, typeof schema[key] === "boolean" || object(schema[key]) !== undefined)),
    ...["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"].flatMap((key) => bad(key, typeof schema[key] === "number" && Number.isFinite(schema[key]))),
    ...["minLength", "maxLength", "minItems", "maxItems"].flatMap((key) => bad(key, typeof schema[key] === "number" && Number.isInteger(schema[key]) && schema[key] >= 0)),
  ];
}

function validate(value: unknown, raw: unknown, at: string, depth = 0): string[] {
  if (raw === true) return [];
  if (raw === false) return [`${at} is forbidden by its schema`];
  const schema = object(raw);
  if (!schema || depth > 32) return [`${at}: schema unchecked (${depth > 32 ? "depth limit" : "invalid schema"})`];
  const problems = Object.keys(schema).filter((key) => !SUPPORTED.has(key)).map((key) => `${at}: schema keyword ${key} is unchecked; validate it separately`);
  problems.push(...malformedKeywords(schema).map((key) => `${at}: schema keyword ${key} has an unsupported shape and is unchecked; fix the schema before running`));
  if (schema.type !== undefined && !hasType(value, schema.type)) return [...problems, `${at} is ${JSON.stringify(value)}, not ${JSON.stringify(schema.type)}`];
  if (Array.isArray(schema.enum) && !schema.enum.some((one) => equal(one, value))) problems.push(`${at} is ${JSON.stringify(value)}, not one of ${JSON.stringify(schema.enum)}`);
  if (owns(schema, "const") && !equal(schema.const, value)) problems.push(`${at} differs from const ${JSON.stringify(schema.const)}`);
  const bounds = typeof value === "number" ? [["minimum", value < Number(schema.minimum)], ["maximum", value > Number(schema.maximum)],
    ["exclusiveMinimum", value <= Number(schema.exclusiveMinimum)], ["exclusiveMaximum", value >= Number(schema.exclusiveMaximum)]] as const
    : typeof value === "string" ? [["minLength", [...value].length < Number(schema.minLength)], ["maxLength", [...value].length > Number(schema.maxLength)]] as const
    : Array.isArray(value) ? [["minItems", value.length < Number(schema.minItems)], ["maxItems", value.length > Number(schema.maxItems)]] as const : [];
  for (const [key, violated] of bounds) if (owns(schema, key) && violated) problems.push(`${at} violates ${key} ${JSON.stringify(schema[key])}`);
  const given = object(value);
  if (given) {
    const properties = object(schema.properties) ?? {};
    const missing = list(schema.required).map(String).filter((key) => !owns(given, key));
    if (missing.length) problems.push(`${at} lacks required ${missing.join(", ")}`);
    const extra = Object.keys(given).filter((key) => !owns(properties, key));
    if (schema.additionalProperties === false && extra.length) problems.push(`${at} passes ${extra.join(", ")}, which the tool does not declare`);
    for (const [key, item] of Object.entries(given)) {
      if (owns(properties, key)) problems.push(...validate(item, properties[key], `${at}.${key}`, depth + 1));
      else if (object(schema.additionalProperties)) problems.push(...validate(item, schema.additionalProperties, `${at}.${key}`, depth + 1));
    }
  }
  if (Array.isArray(value) && owns(schema, "items")) value.forEach((item, i) => problems.push(...validate(item, schema.items, `${at}[${i}]`, depth + 1)));
  return problems;
}

/** Structural diagnostics only: valid arguments do not establish tool choice, truth or authority. */
export function toolCallChecks(calls: unknown[], tools: unknown[] | undefined): string[] {
  if (!tools?.length) return [TOOL_CALL_WITHOUT_TOOLS];
  if (!calls.length) return ["tool-call text has no structured calls: do not execute it; inspect the raw response"];
  const definitions = tools.map((tool) => object(object(tool)?.function)).filter((fn): fn is Fields => fn !== undefined);
  const names = definitions.map((fn) => fn.name);
  if (new Set(names).size !== names.length) return ["duplicate tool names make the schemas ambiguous: fix the tool definitions before running any call"];
  const schemas = new Map(definitions.map((fn) => [String(fn.name), fn.parameters ?? {}]));
  const problems: string[] = [];
  calls.forEach((raw, i) => {
    const fn = object(object(raw)?.function) ?? {};
    const name = String(fn.name ?? "?");
    const at = `call ${i + 1} (${name})`;
    if (typeof fn.name !== "string" || !fn.name) {
      problems.push(`${at}: its function name is not a nonempty string`);
      return;
    }
    if (!schemas.has(name)) {
      problems.push(`${at} is not one of the tools sent (${[...schemas.keys()].join(", ")})`);
      return;
    }
    let args: unknown = fn.arguments;
    if (typeof args === "string") {
      try { args = JSON.parse(args); } catch { args = undefined; }
    }
    if (!object(args)) problems.push(`${at}: its arguments are not a JSON object`);
    else problems.push(...validate(args, schemas.get(name), at));
  });
  return problems.length
    ? [...problems, "fix or refuse these calls before running any; a rerun often corrects them"]
    : [`${calls.length} tool call(s) match the tool schemas' supported structural checks; verify tool choice and authorization, then send each result back as {role:"tool", tool_name, content} after the assistant message, with the same tools`];
}
