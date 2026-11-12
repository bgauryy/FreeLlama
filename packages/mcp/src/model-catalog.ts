// The shipped model catalog: for each job, the tested picks and their evidence, annotated with what
// is installed and fits right now. rust-core owns the data, so the CLI and MCP read one file.
import catalogData from "../../rust-core/src/model_catalog.json" with { type: "json" };
import { libraryReference } from "./model-knowledge.js";

// `digest` is optional: a pick that names its weights' digest is matched by it too.
type Pick = { tag: string; role: string; file_gb: number; memory_gb: number | null; quality: string; evidence: string; digest?: string };
type Job = { job: string; title: string; call: string; cookbook: string; picks: Pick[] };

export const MODEL_CATALOG = catalogData as { snapshot: string; measured_on: string; how_to_read: string; jobs: Job[] };
export const CATALOG_JOBS = MODEL_CATALOG.jobs.map((job) => job.job);

/** One `/api/tags` entry: Ollama reports each installed tag with its manifest digest. */
/** `capabilities`: what Ollama's /api/tags reports for the tag (tools, thinking, vision, ...), when it does. */
export type InstalledModel = { name: string; digest?: string | null; capabilities?: string[]; /** file bytes, from /api/tags */ size?: number };

/** `glm-ocr`, `library/glm-ocr:latest` and `GLM-OCR:latest` name one tag. */
function canonicalTag(name: string): string {
  return (libraryReference(name)?.tag ?? name).toLowerCase();
}

function canonicalDigest(digest: string | null | undefined): string | null {
  const hex = digest?.toLowerCase().replace(/^sha256:/, "") ?? "";
  return /^[\da-f]{12,64}$/.test(hex) ? hex : null;
}

/**
 * Whether `tag` is installed: by name, else by digest when both sides report one (ollama.com tag
 * pages show a 12-character prefix, /api/tags the whole digest). Weights installed under another
 * tag are reported with that name, so the agent uses them instead of pulling the same bytes again.
 */
export function installedMatch(
  tag: string, inventory: readonly InstalledModel[], digest?: string | null,
): { installed: boolean; installed_as?: string } {
  const wanted = canonicalTag(tag);
  if (inventory.some((entry) => canonicalTag(entry.name) === wanted)) return { installed: true };
  const expected = canonicalDigest(digest);
  if (expected) {
    const same = inventory.find((entry) => {
      const actual = canonicalDigest(entry.digest);
      return actual !== null && (actual.startsWith(expected) || expected.startsWith(actual));
    });
    if (same) return { installed: true, installed_as: same.name };
  }
  return { installed: false };
}

function inventoryOf(installed: readonly InstalledModel[] | ReadonlySet<string>): readonly InstalledModel[] {
  return installed instanceof Set ? [...installed as ReadonlySet<string>].map((name) => ({ name })) : installed as readonly InstalledModel[];
}

/**
 * Annotate picks with live state. `installed` is null when the inventory could not be read, and
 * `fits_now` is null when the available memory or the model's need is unknown: unknown is not "no".
 */
export function catalogView(
  jobName: string | undefined, installed: readonly InstalledModel[] | ReadonlySet<string> | null, availableBytes: number | null,
) {
  const inventory = installed === null ? null : inventoryOf(installed);
  const jobs = MODEL_CATALOG.jobs.filter((job) => jobName === undefined || job.job === jobName).map((job) => ({
    ...job,
    cookbook: `freellama://docs/${job.cookbook}`,
    picks: job.picks.map((pick) => ({
      ...pick,
      ...(inventory === null ? { installed: null } : installedMatch(pick.tag, inventory, pick.digest)),
      fits_now: availableBytes === null || pick.memory_gb === null ? null : availableBytes >= pick.memory_gb * 1e9,
    })),
  }));
  return {
    snapshot: MODEL_CATALOG.snapshot,
    measured_on: MODEL_CATALOG.measured_on,
    how_to_read: MODEL_CATALOG.how_to_read,
    available_gb: availableBytes === null ? null : Number((availableBytes / 1e9).toFixed(1)),
    jobs,
  };
}

/** One line per job, for clients that show only text. */
export function catalogText(view: ReturnType<typeof catalogView>): string {
  const state = (pick: ReturnType<typeof catalogView>["jobs"][number]["picks"][number]) => [
    pick.role, pick.quality,
    pick.installed === null ? "" : pick.installed ? ("installed_as" in pick && pick.installed_as ? `installed as ${pick.installed_as}` : "installed") : "not installed",
    pick.fits_now === null ? "" : pick.fits_now ? "fits now" : "no room now",
  ].filter(Boolean).join(", ");
  const lines = view.jobs.map((job) => `${job.job}: ${job.picks.map((pick) => `${pick.tag} (${state(pick)})`).join("; ")}.`);
  // One recipe pointer for every job, not one per line: the URIs share a pattern.
  const recipes = view.jobs.length === 1 ? `Recipe: ${view.jobs[0]!.cookbook}` : "Recipes: freellama://docs/cookbooks/<job>";
  return `${lines.join("\n")}\n${recipes}. Available memory: ${view.available_gb ?? "unknown"} GB. Measured on ${view.measured_on}.`;
}
