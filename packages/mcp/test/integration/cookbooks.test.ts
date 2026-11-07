import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import path from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { REPO_ROOT } from "../../src/config.js";
import { CATALOG_JOBS, MODEL_CATALOG, catalogText, catalogView } from "../../src/model-catalog.js";
import { connectClient } from "../setup/client.js";

const DIR = path.join(REPO_ROOT, "docs/cookbooks");
const recipes = readdirSync(DIR).filter((name) => name.endsWith(".md") && name !== "README.md");
type Schema = { type?: string | string[]; enum?: unknown[]; properties?: Record<string, Schema>; items?: Schema; anyOf?: Schema[]; additionalProperties?: boolean | Schema };

/** Field names and enum values in `value` that the published schema does not accept. */
function problems(value: unknown, schema: Schema | undefined, at: string): string[] {
  if (!schema) return [];
  if (schema.anyOf) return schema.anyOf.some((option) => problems(value, option, at).length === 0) ? [] : problems(value, schema.anyOf[0], at);
  if (schema.enum && !schema.enum.includes(value)) return [`${at}: ${String(value)} is not one of ${schema.enum.join(", ")}`];
  if (Array.isArray(value)) return value.flatMap((item, index) => problems(item, schema.items, `${at}[${index}]`));
  if (value !== null && typeof value === "object" && schema.properties) {
    return Object.entries(value).flatMap(([key, item]) => (key in schema.properties! ? problems(item, schema.properties![key], `${at}.${key}`) : [`${at}.${key}: unknown field`]));
  }
  return [];
}

describe("cookbooks and catalog", () => {
  let client: Client;
  let schemas: Map<string, Schema>;
  beforeAll(async () => {
    client = await connectClient({ FREELLAMA_MCP_AUTOSTART_SERVE: "0" });
    schemas = new Map((await client.listTools()).tools.map((tool) => [tool.name, tool.inputSchema as Schema]));
  });
  afterAll(async () => { await client?.close(); });

  it.each(recipes)("%s: every call matches the published tool schema", (name) => {
    const calls = [...readFileSync(path.join(DIR, name), "utf8").matchAll(/```json\n(.*?)\n```/gs)].map((match) => JSON.parse(match[1]!) as { tool: string; arguments: unknown });
    expect(calls.length).toBeGreaterThan(0);
    for (const call of calls) {
      expect(schemas.has(call.tool), `${name}: unknown tool ${call.tool}`).toBe(true);
      expect(problems(call.arguments, schemas.get(call.tool), call.tool)).toEqual([]);
    }
  });

  it("gives every job a recipe that uses its default pick, and links every recipe from the index", () => {
    for (const job of MODEL_CATALOG.jobs) {
      const file = path.join(REPO_ROOT, "docs", `${job.cookbook}.md`);
      expect(existsSync(file), job.cookbook).toBe(true);
      const defaults = job.picks.filter((pick) => pick.role === "default");
      expect(defaults, job.job).toHaveLength(1);
      const recipe = readFileSync(file, "utf8");
      if (!recipe.includes('job:"')) expect(recipe, job.job).toContain(defaults[0]!.tag);
    }
    const index = readFileSync(path.join(DIR, "README.md"), "utf8");
    for (const name of recipes) expect(index, name).toContain(`(${name})`);
  });

  it("labels every pick and keeps unknown state unknown", () => {
    for (const job of MODEL_CATALOG.jobs) for (const pick of job.picks) {
      expect(["measured", "published", "untested"]).toContain(pick.quality);
      expect(["default", "stronger", "small", "alternative"]).toContain(pick.role);
      expect(pick.evidence.length).toBeGreaterThan(0);
    }
    const unknown = catalogView("classify", null, null);
    expect(unknown.jobs[0]!.picks[0]).toMatchObject({ installed: null, fits_now: null });
    const known = catalogView("classify", new Set(["strands-decider:2b-nvfp4"]), 4e9);
    expect(known.jobs[0]!.picks[0]).toMatchObject({ installed: true, fits_now: true });
    expect(known.jobs[0]!.picks[1]).toMatchObject({ installed: false, fits_now: false });
    // A pick with no previewed memory figure stays unknown even when available memory is known.
    const speech = catalogView("text-to-speech", new Set(["phonon:en"]), 4e9);
    expect(speech.jobs[0]!.picks[0]).toMatchObject({ tag: "phonon:en", memory_gb: null, installed: true, fits_now: null });
    expect(catalogText(known)).toContain("freellama://docs/cookbooks/classify");
    expect(CATALOG_JOBS).toContain("speech-to-text");
  });

  it("serves the catalog, and refuses fields of other views", async () => {
    const shown = await client.callTool({ name: "models", arguments: { view: "catalog", job: "ocr" } }) as unknown as { structuredContent: { jobs: Array<{ job: string; cookbook: string }> } };
    expect(shown.structuredContent.jobs.map((job) => job.job)).toEqual(["ocr"]);
    expect(shown.structuredContent.jobs[0]!.cookbook).toBe("freellama://docs/cookbooks/ocr");
    expect((await client.callTool({ name: "models", arguments: { view: "catalog", limit: 3 } })).isError).toBe(true);
    expect((await client.callTool({ name: "models", arguments: { view: "installed", job: "ocr" } })).isError).toBe(true);
  });

  it("ships cookbooks and the Octocode instructions as resources", async () => {
    const uris = (await client.listResources()).resources.map((resource) => resource.uri);
    expect(uris).toContain("freellama://docs/cookbooks/classify");
    expect(uris).toContain("freellama://docs/octocode/LOCAL");
    expect(uris).toContain("freellama://docs/LOCAL_MODELS");
  });
});
