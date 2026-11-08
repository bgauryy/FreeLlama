// `run_task {job}`: FreeLlama picks the model (the job's first catalog pick that is installed and
// fits now, judged by the same live state models{view:"catalog"} shows) and fills in the prompt,
// system prompt, options, think setting and embedding prefixes from local-prompts.ts. A field the
// caller passes always wins; `job_profile.applied` names every field job mode filled in.
import { catalogView, type InstalledModel } from "./model-catalog.js";
import { catalogLiveState } from "./model-library.js";
import { EMBEDDING_PREFIXES, LOCAL_JOBS, type JobName, type LocalJob, type ModelProfile, type Prefixes } from "./local-prompts.js";

export type JobProfile = { job: JobName; model: string | undefined; applied: string[] };

/** The fields job mode reads and fills; every other field passes through untouched. */
export type JobFields = {
  task?: string;
  model?: string;
  prompt?: string;
  systemPrompt?: string;
  messages?: unknown[];
  options?: Record<string, unknown>;
  think?: boolean | string;
  input?: unknown;
  nearest?: { query?: string; top?: number; minScore?: number; documentPrefix?: string };
};

const lower = (tag: string) => tag.toLowerCase();

/**
 * The job's model for this machine now: the first catalog pick that is installed and not known to
 * be short of memory (unknown free memory is not "no room"). A pick installed under another tag
 * runs under that tag. Throws a JSON refusal naming each pick's need and what is free.
 */
export function pickJobModel(job: JobName, installed: readonly InstalledModel[] | null, availableBytes: number | null): string {
  const spec = LOCAL_JOBS[job];
  if (installed === null) {
    throw new Error(JSON.stringify({
      code: "job_inventory_unavailable", job,
      error: `job "${job}": the installed models could not be read (is Ollama running?), so no pick can be chosen; pass \`model\`.`,
    }));
  }
  const view = catalogView(spec.catalogJob, installed, availableBytes).jobs[0]!;
  const pick = view.picks.find((entry) => entry.installed === true && entry.fits_now !== false);
  if (pick) return ("installed_as" in pick && pick.installed_as) || pick.tag;
  const free = availableBytes === null ? "unknown" : `${(availableBytes / 1e9).toFixed(1)} GB`;
  const picks = view.picks.map((entry) => ({ tag: entry.tag, memory_gb: entry.memory_gb, installed: entry.installed, fits_now: entry.fits_now }));
  const listed = picks.map((entry) => `${entry.tag} (needs ${entry.memory_gb ?? "?"} GB, ${entry.installed ? "installed" : "not installed"})`).join("; ");
  throw new Error(JSON.stringify({
    code: "job_no_model_fits", job, picks, available_gb: availableBytes === null ? null : Number((availableBytes / 1e9).toFixed(1)),
    error: `job "${job}": no catalog pick is installed and fits now (free: ${free}). Picks: ${listed}. ` +
      "Ask the user to free memory, pass `model` (an installed model with the same capability), or ask them to approve pulling one exact tag at its size.",
  }));
}

/** Read the live state and pick: only when the caller left `model` out. */
export async function pickLiveJobModel(job: JobName, endpoint: string | undefined): Promise<string> {
  const [installed, available] = await catalogLiveState(endpoint, undefined);
  return pickJobModel(job, installed, available);
}

function familyPrefixes(spec: LocalJob, model: string | undefined): Prefixes | undefined {
  if (!spec.prefixes || model === undefined) return undefined;
  const name = lower(model).replace(/^(?:registry\.ollama\.ai\/)?library\//, "");
  return Object.entries(spec.prefixes).find(([family]) => name.startsWith(family))?.[1];
}

/** Any prefix a family documents: text that already starts with one is not prefixed again. */
const KNOWN_PREFIXES = Object.values(EMBEDDING_PREFIXES).flatMap((family) => Object.values(family) as string[]);
const prefixed = (text: string) => KNOWN_PREFIXES.some((prefix) => text.startsWith(prefix));

function modelProfile(spec: LocalJob, model: string | undefined): ModelProfile | undefined {
  if (!spec.models || model === undefined) return undefined;
  const wanted = lower(model);
  return Object.entries(spec.models).find(([tag]) => lower(tag) === wanted)?.[1];
}

/**
 * Fill `fields` from the job's profile. `model` is the caller's, else the picked one. `routingOnly`
 * (preview) sets only task and model. Throws when the job needs a field the caller did not send, or
 * when an explicit `task` contradicts the job.
 */
export function applyJob<T extends JobFields>(job: JobName, fields: T, model: string | undefined, routingOnly = false): { fields: T & JobFields; profile: JobProfile } {
  const spec = LOCAL_JOBS[job];
  const applied: string[] = [];
  const next: JobFields = { ...fields };
  if (next.task === undefined) {
    next.task = spec.task;
    applied.push("task");
  } else if (next.task !== spec.task) {
    throw new Error(`job "${job}" runs task "${spec.task}"; remove \`task\` (or \`job\`).`);
  }
  if (next.model === undefined && model !== undefined) {
    next.model = model;
    applied.push("model");
  }
  const profile = (): JobProfile => ({ job, model: next.model, applied });
  if (routingOnly || spec.task === "decision") return { fields: next as T, profile: profile() };
  if (spec.task === "embedding") {
    const prefixes = familyPrefixes(spec, next.model);
    const nearest = { ...next.nearest };
    if (spec.needs && nearest.query === undefined) throw new Error(`job "${job}" needs ${spec.needs}.`);
    if (next.nearest === undefined) applied.push("nearest");
    if (prefixes?.query && nearest.query !== undefined && !prefixed(nearest.query)) {
      nearest.query = `${prefixes.query}${nearest.query}`;
      applied.push("nearest.query prefix");
    }
    if (prefixes?.document && nearest.documentPrefix === undefined) {
      nearest.documentPrefix = prefixes.document;
      applied.push("nearest.documentPrefix");
      // Text items given inline are documents too; files get the prefix when they are read.
      const items = Array.isArray(next.input) ? next.input : next.input === undefined ? [] : [next.input];
      if (items.some((item) => typeof item === "string" && !prefixed(item))) {
        const withPrefix = items.map((item) => (typeof item === "string" && !prefixed(item) ? `${prefixes.document}${item}` : item));
        next.input = Array.isArray(next.input) ? withPrefix : withPrefix[0];
        applied.push("input prefix");
      }
    }
    next.nearest = nearest;
    return { fields: next as T, profile: profile() };
  }
  const own = modelProfile(spec, next.model);
  // A caller's conversation is theirs whole: no prompt or system prompt is added to it.
  if (next.messages === undefined) {
    if (next.prompt === undefined) {
      if (!spec.prompt) throw new Error(`job "${job}" needs ${spec.needs ?? "`prompt`"}.`);
      next.prompt = spec.prompt({});
      applied.push("prompt");
    }
    const systemPrompt = own?.systemPrompt ?? spec.systemPrompt;
    if (next.systemPrompt === undefined && systemPrompt !== undefined) {
      next.systemPrompt = systemPrompt;
      applied.push("systemPrompt");
    }
  }
  const think = own?.think ?? spec.think;
  if (next.think === undefined && think !== undefined) {
    next.think = think;
    applied.push("think");
  }
  // A model's documented options belong to its think setting; another one the caller chose voids them.
  const options = own?.think !== undefined && next.think !== own.think ? undefined : own?.options ?? spec.options;
  if (options && Object.keys(options).some((key) => next.options?.[key] === undefined)) {
    next.options = { ...options, ...next.options };
    applied.push("options");
  }
  return { fields: next as T, profile: profile() };
}

/** One profile for a batch: the model it picked and every field it filled in any item. */
export function mergeProfiles(job: JobName, profiles: JobProfile[]): JobProfile {
  const models = [...new Set(profiles.map((profile) => profile.model).filter((model): model is string => model !== undefined))];
  return { job, model: models.length === 1 ? models[0] : models.join(", ") || undefined, applied: [...new Set(profiles.flatMap((profile) => profile.applied))] };
}
