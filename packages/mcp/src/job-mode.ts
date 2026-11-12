// `run_task {job}`: FreeLlama picks the model (the job's first catalog pick that is installed and
// fits now, judged by the same live state models{view:"catalog"} shows) and fills in the prompt,
// system prompt, options, think setting and embedding prefixes from local-prompts.ts. A field the
// caller passes always wins; `job_profile.applied` names every field job mode filled in.
import { catalogView, type InstalledModel } from "./model-catalog.js";
import { catalogLiveState } from "./model-library.js";
import { EMBEDDING_PREFIXES, isOcrModel, LOCAL_JOBS, WORKFLOWS, type JobName, type LocalJob, type ModelProfile, type Prefixes, type Workflow, type WorkflowName } from "./local-prompts.js";

export type JobProfile = { job: JobName; workflow?: WorkflowName; model: string | undefined; applied: string[] };

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
  format?: unknown;
  tools?: unknown[];
  nearest?: { query?: string; top?: number; minScore?: number; documentPrefix?: string };
};

const lower = (tag: string) => tag.toLowerCase();

/** Catalog roles from most to least capable: a `capable` workflow tries them in this order. */
const CAPABLE_ORDER = ["stronger", "alternative", "default", "small"];

/** The Ollama capabilities a request needs from its model: tools when it sends tools, thinking when it asks for it. */
export function neededCapabilities(fields: { tools?: unknown[]; think?: unknown }): string[] {
  return [
    ...(fields.tools?.length ? ["tools"] : []),
    ...(fields.think === true || (typeof fields.think === "string" && fields.think !== "") ? ["thinking"] : []),
  ];
}

/**
 * The job's model for this machine now: the first catalog pick that is installed and not known to
 * be short of memory (unknown free memory is not "no room"), and that Ollama does not report
 * without a capability in `needs`; `capable` takes the strongest such pick. When every installed
 * pick misses only the memory preview, the first one goes to serve's memory gate. A pick installed
 * under another tag runs under that tag. Throws a JSON refusal naming each pick's need and what is
 * free when no suitable pick is installed.
 */
export function pickJobModel(
  job: JobName, installed: readonly InstalledModel[] | null, availableBytes: number | null, prefer: "default" | "capable" = "default", needs: readonly string[] = [],
): string {
  return pickJobModelDetailed(job, installed, availableBytes, prefer, needs).model;
}

/** A pick, plus a note when a capable workflow ran below a stronger pick that is installed but has no room now. */
export type JobPick = { model: string; note?: string };

/** pickJobModel, also saying which stronger installed pick was skipped for memory. */
export function pickJobModelDetailed(
  job: JobName, installed: readonly InstalledModel[] | null, availableBytes: number | null, prefer: "default" | "capable" = "default", needs: readonly string[] = [],
): JobPick {
  const spec = LOCAL_JOBS[job];
  if (installed === null) {
    throw new Error(JSON.stringify({
      code: "job_inventory_unavailable", job,
      error: `job "${job}": the installed models could not be read (is Ollama running?), so no pick can be chosen; pass \`model\`.`,
    }));
  }
  const view = catalogView(spec.catalogJob, installed, availableBytes).jobs[0]!;
  // A pick whose tag Ollama reports without a needed capability would be refused ("does not support
  // tools"); unknown capabilities are not "no".
  const capabilitiesOf = (tag: string) => installed.find((model) => model.name.toLowerCase() === tag.toLowerCase())?.capabilities;
  const lacking = (entry: (typeof view.picks)[number]) => {
    const capabilities = capabilitiesOf(("installed_as" in entry && entry.installed_as) || entry.tag);
    return capabilities ? needs.filter((need) => !capabilities.includes(need)) : [];
  };
  const usable = view.picks.filter((entry) => entry.installed === true && entry.fits_now !== false && lacking(entry).length === 0);
  // A capable workflow (writing tests, implementing, reviewing) takes the strongest pick that fits;
  // catalog order is kept within a role. Light work keeps the catalog's first pick.
  const ranked = prefer === "capable"
    ? [...usable].sort((a, b) => CAPABLE_ORDER.indexOf(String((a as { role?: string }).role)) - CAPABLE_ORDER.indexOf(String((b as { role?: string }).role)))
    : usable;
  const pick = ranked[0];
  if (pick) {
    const model = ("installed_as" in pick && pick.installed_as) || pick.tag;
    if (prefer !== "capable") return { model };
    // The answer gets worse silently when the strong pick waits for memory: say so.
    const rank = (entry: (typeof view.picks)[number]) => CAPABLE_ORDER.indexOf(String((entry as { role?: string }).role));
    const skipped = view.picks.filter((entry) => entry.installed === true && entry.fits_now === false && lacking(entry).length === 0 && rank(entry) < rank(pick));
    if (skipped.length === 0) return { model };
    const free = availableBytes === null ? "unknown" : `${(availableBytes / 1e9).toFixed(1)} GB`;
    const names = skipped.map((entry) => `${("installed_as" in entry && entry.installed_as) || entry.tag} (needs ${entry.memory_gb ?? "?"} GB)`).join(", ");
    return { model, note: `ran on ${model} because the stronger installed pick ${names} did not fit now (free: ${free}); if the answer matters, rerun when memory frees or pass that model` };
  }
  // The catalog's memory_gb is a conservative preview (it asks for a reserve); serve's memory gate
  // measures the real load. An installed pick that only misses the preview goes to the gate, which
  // runs it or refuses with what holds memory (exit 75), as delegate_research does. Measured
  // 2026-11-12: bonsai-2:9b previewed at 10.3 GB, the gate admitted it with 6.0 GB free.
  const installedPick = view.picks.find((entry) => entry.installed === true && lacking(entry).length === 0);
  if (installedPick) return { model: ("installed_as" in installedPick && installedPick.installed_as) || installedPick.tag };
  // Machines differ: with no tested pick installed, an installed model that has the job's
  // capability runs it, flagged as untested, instead of refusing every workflow.
  const untested = untestedPick(spec, installed, availableBytes, needs);
  if (untested) {
    return { model: untested, note: `no tested pick for job "${job}" is installed, so it ran on ${untested}, chosen by capability and untested for this job: verify the answer (models {view:"catalog", job:"${spec.catalogJob}"} lists tested picks)` };
  }
  const free = availableBytes === null ? "unknown" : `${(availableBytes / 1e9).toFixed(1)} GB`;
  const picks = view.picks.map((entry) => {
    const missing = entry.installed === true ? lacking(entry) : [];
    return { tag: entry.tag, memory_gb: entry.memory_gb, installed: entry.installed, fits_now: entry.fits_now, ...(missing.length > 0 ? { lacks: missing } : {}) };
  });
  const listed = picks.map((entry) => `${entry.tag} (needs ${entry.memory_gb ?? "?"} GB, ${entry.installed ? "installed" : "not installed"}${entry.lacks ? `, no ${entry.lacks.join(" or ")} support` : ""})`).join("; ");
  const wanted = needs.length > 0 ? ` with ${needs.join(" and ")} support` : "";
  throw new Error(JSON.stringify({
    code: "job_no_model_fits", job, picks, available_gb: availableBytes === null ? null : Number((availableBytes / 1e9).toFixed(1)),
    error: `job "${job}": no catalog pick${wanted} is installed and fits now (free: ${free}), and no installed model reports the ${[...JOB_CAPABILITIES[spec.task], ...needs].join(" + ")} capability to run it untested. Picks: ${listed}. ` +
      "Ask the user to free memory, pass `model` (an installed model with the same capability), or ask them to approve pulling one exact tag at its size.",
  }));
}

/** The Ollama capabilities a job's model must report, by the job's task. */
const JOB_CAPABILITIES: Record<LocalJob["task"], string[]> = {
  coding: ["completion"], completion: ["completion"], vision: ["completion", "vision"], embedding: ["embedding"], decision: ["decision"],
};

/**
 * An installed model for a job none of whose tested picks is installed: one that reports every
 * capability the job and the request need (models whose capabilities are unknown are skipped:
 * a guess could send a chat to an embedding model). OCR-only families read text from images only,
 * so they serve the ocr job and nothing else. Among the rest, the largest whose file leaves half of
 * free memory for its context and the host is preferred (bigger answers better), else the smallest
 * (serve's memory gate decides). Image search prefers an embedding model that also takes images.
 */
function untestedPick(spec: LocalJob, installed: readonly InstalledModel[], availableBytes: number | null, needs: readonly string[] = []): string | undefined {
  const wanted = [...JOB_CAPABILITIES[spec.task], ...needs];
  const ocrJob = spec.job === "ocr";
  let candidates = installed.filter((model) => model.capabilities !== undefined
    && wanted.every((capability) => model.capabilities!.includes(capability))
    && (ocrJob || !isOcrModel(model.name)));
  if (ocrJob && candidates.some((model) => isOcrModel(model.name))) candidates = candidates.filter((model) => isOcrModel(model.name));
  if (spec.job === "image-search" && candidates.some((model) => model.capabilities!.includes("vision"))) candidates = candidates.filter((model) => model.capabilities!.includes("vision"));
  if (candidates.length === 0) return undefined;
  const size = (model: InstalledModel) => model.size ?? Number.MAX_SAFE_INTEGER;
  const roomy = availableBytes === null ? [] : candidates.filter((model) => model.size !== undefined && model.size * 2 <= availableBytes);
  const ordered = roomy.length > 0 ? [...roomy].sort((a, b) => size(b) - size(a)) : [...candidates].sort((a, b) => size(a) - size(b));
  return ordered[0]!.name;
}

/** Read the live state and pick: only when the caller left `model` out. */
export async function pickLiveJobModel(job: JobName, endpoint: string | undefined, prefer: "default" | "capable" = "default", needs: readonly string[] = []): Promise<string> {
  return (await pickLiveJobModelDetailed(job, endpoint, prefer, needs)).model;
}

/** pickLiveJobModel with the note a capable workflow's caller should see. */
export async function pickLiveJobModelDetailed(job: JobName, endpoint: string | undefined, prefer: "default" | "capable" = "default", needs: readonly string[] = []): Promise<JobPick> {
  const [installed, available] = await catalogLiveState(endpoint, undefined);
  return pickJobModelDetailed(job, installed, available, prefer, needs);
}

function familyPrefixes(spec: LocalJob, model: string | undefined): Prefixes | undefined {
  return familyMatch(spec.prefixes, model);
}

/** Any prefix a family documents: text that already starts with one is not prefixed again. */
const KNOWN_PREFIXES = Object.values(EMBEDDING_PREFIXES).flatMap((family) => Object.values(family) as string[]);
const prefixed = (text: string) => KNOWN_PREFIXES.some((prefix) => text.startsWith(prefix));

/** `hf.co/openbmb/MiniCPM-OCR-1.0-gguf:Q4_K_M` -> `minicpm-ocr-1.0-gguf:q4_k_m`: the name a family key matches. */
export function modelName(model: string): string {
  return lower(model).replace(/^(?:hf\.co|huggingface\.co)\/[^/]+\//, "").replace(/^registry\.ollama\.ai\/library\//, "");
}

/**
 * The profile for `model`: the longest key that is its name or a prefix ending at a name boundary
 * (`:` or `-`), so `lfm3` covers lfm3:1.7b and lfm3:latest, `bonsai-2:27b` wins over `bonsai-2`
 * for bonsai-2:27b-q4, and any quantization of a family gets that family's documented values.
 */
export function familyMatch<V>(table: Record<string, V> | undefined, model: string | undefined): V | undefined {
  if (!table || model === undefined) return undefined;
  const name = modelName(model);
  let best: [string, V] | undefined;
  for (const [key, value] of Object.entries(table)) {
    const wanted = modelName(key);
    const fits = name === wanted || (name.startsWith(wanted) && /[:-]/.test(name[wanted.length] ?? "")) || (wanted.endsWith(":") && name.startsWith(wanted));
    if (fits && (!best || wanted.length > best[0].length)) best = [wanted, value];
  }
  return best?.[1];
}

function modelProfile(spec: LocalJob, model: string | undefined): ModelProfile | undefined {
  return familyMatch(spec.models, model);
}

/**
 * Fill `fields` from the job's profile. `model` is the caller's, else the picked one. `routingOnly`
 * (preview) sets only task and model. Throws when the job needs a field the caller did not send, or
 * when an explicit `task` contradicts the job.
 */
export function applyJob<T extends JobFields>(
  job: JobName, fields: T, model: string | undefined, routingOnly = false, workflow?: WorkflowName,
): { fields: T & JobFields; profile: JobProfile } {
  const spec = LOCAL_JOBS[job];
  const flow: Workflow | undefined = workflow ? WORKFLOWS[workflow] : undefined;
  if (flow && flow.job !== job) throw new Error(`workflow "${workflow}" runs job "${flow.job}", not "${job}".`);
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
  const profile = (): JobProfile => ({ job, ...(workflow ? { workflow } : {}), model: next.model, applied });
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
    if (flow?.prompt) {
      // The workflow builds the user message, from the caller's request when it takes one.
      next.prompt = flow.prompt(next.prompt, next.model);
      applied.push("prompt");
    } else if (next.prompt === undefined) {
      if (flow) {
        // The workflow's instructions carry the task; the inputs follow this line.
        next.prompt = "Apply your instructions to the input below.";
        applied.push("prompt");
      } else {
        if (!spec.prompt) throw new Error(`job "${job}" needs ${spec.needs ?? "`prompt`"}.`);
        next.prompt = spec.prompt({ model: next.model });
        applied.push("prompt");
      }
    }
    // A workflow says what to do, so it wins over a model's own style prompt; a job without one
    // keeps the model's documented system prompt, else the job's default workflow.
    let systemPrompt = flow ? flow.system ?? own?.systemPrompt : own?.systemPrompt ?? spec.systemPrompt;
    if (next.systemPrompt === undefined && systemPrompt !== undefined) {
      next.systemPrompt = systemPrompt;
      applied.push("systemPrompt");
    }
  }
  if (flow?.format && next.format === undefined) {
    // A request that is a JSON schema constrains the output to it; it stays in the prompt too, as
    // https://docs.ollama.com/capabilities/structured-outputs advises.
    next.format = jsonSchema(fields.prompt) ?? flow.format;
    applied.push("format");
  }
  const think = flow?.think ?? own?.think ?? spec.think;
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
  // A ceiling on the answer, about 8x what the workflow's eval answers needed: a looping or
  // rambling model stops in seconds instead of minutes. Only with thinking off, where reasoning
  // cannot spend it; run_task turns stopOnRepetition on with it (a cap would turn it off).
  if (flow?.maxOutput !== undefined && next.think === false && next.options?.num_predict === undefined) {
    next.options = { ...next.options, num_predict: flow.maxOutput };
    applied.push("num_predict");
  }
  return { fields: next as T, profile: profile() };
}

/** `text` parsed as a JSON schema of an object (it has `properties`), else undefined. */
function jsonSchema(text: string | undefined): Record<string, unknown> | undefined {
  if (!text?.trim().startsWith("{")) return undefined;
  try {
    const parsed: unknown = JSON.parse(text);
    const schema = parsed !== null && typeof parsed === "object" && !Array.isArray(parsed) ? parsed as Record<string, unknown> : undefined;
    return schema?.properties !== null && typeof schema?.properties === "object" ? schema : undefined;
  } catch {
    return undefined; // Not JSON: the request is a field list.
  }
}

/** One profile for a batch: the model it picked and every field it filled in any item. */
export function mergeProfiles(job: JobName, profiles: JobProfile[]): JobProfile {
  const models = [...new Set(profiles.map((profile) => profile.model).filter((model): model is string => model !== undefined))];
  return { job, model: models.length === 1 ? models[0] : models.join(", ") || undefined, applied: [...new Set(profiles.flatMap((profile) => profile.applied))] };
}
