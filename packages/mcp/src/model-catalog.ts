// The shipped model catalog: for each job, the tested picks and their evidence, annotated with what
// is installed and fits right now. rust-core owns the data, so the CLI and MCP read one file.
import catalogData from "../../rust-core/src/model_catalog.json" with { type: "json" };

type Pick = { tag: string; role: string; file_gb: number; memory_gb: number | null; quality: string; evidence: string };
type Job = { job: string; title: string; call: string; cookbook: string; picks: Pick[] };

export const MODEL_CATALOG = catalogData as { snapshot: string; measured_on: string; how_to_read: string; jobs: Job[] };
export const CATALOG_JOBS = MODEL_CATALOG.jobs.map((job) => job.job);

/**
 * Annotate picks with live state. `installed` is null when the inventory could not be read, and
 * `fits_now` is null when the available memory or the model's need is unknown: unknown is not "no".
 */
export function catalogView(jobName: string | undefined, installed: ReadonlySet<string> | null, availableBytes: number | null) {
  const jobs = MODEL_CATALOG.jobs.filter((job) => jobName === undefined || job.job === jobName).map((job) => ({
    ...job,
    cookbook: `freellama://docs/${job.cookbook}`,
    picks: job.picks.map((pick) => ({
      ...pick,
      installed: installed === null ? null : installed.has(pick.tag),
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
    pick.installed === null ? "" : pick.installed ? "installed" : "not installed",
    pick.fits_now === null ? "" : pick.fits_now ? "fits now" : "no room now",
  ].filter(Boolean).join(", ");
  const lines = view.jobs.map((job) => `${job.job}: ${job.picks.map((pick) => `${pick.tag} (${state(pick)})`).join("; ")}. Recipe: ${job.cookbook}`);
  return `${lines.join("\n")}\nAvailable memory: ${view.available_gb ?? "unknown"} GB. Measured on ${view.measured_on}.`;
}
