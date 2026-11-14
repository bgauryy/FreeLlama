// Public model sources shared by `models{view:"library"}` and `ollama_pull`: one cached client per
// source, and the local inventory with digests. No native addon, so lifecycle tools load it freely.
import { DEFAULT_SERVE_ENDPOINT } from "./config.js";
import { ollamaFetch } from "./helpers.js";
import { HuggingFaceClient, huggingFaceReference } from "./huggingface.js";
import type { InstalledModel } from "./model-catalog.js";
import { libraryReference, OllamaLibraryClient } from "./model-knowledge.js";
import { serveAuthHeaders } from "./serve-http.js";

export const ollamaLibrary = new OllamaLibraryClient();
export const huggingFace = new HuggingFaceClient();

/** A library tag's reported download size, as `models{view:"library", model}` lists it. */
export type TagSizeLookup =
  | { found: true; tag: string; sizeBytes: number | null; sourcePage: string }
  | { found: false; tag: string; sourcePage: string | null; message: string };

const tagKey = (tag: string) => tag.toLowerCase();

/**
 * Look up one exact tag's size the same way library step 2 does (ollama.com tags page, or the
 * Hugging Face tree for hf.co/<owner>/<repo>). `found:false` means the source answered and has no
 * such tag; a source that could not be reached throws, so offline is never read as "absent".
 */
export async function libraryTagSize(model: string): Promise<TagSizeLookup> {
  if (huggingFaceReference(model)) {
    const hub = await huggingFace.lookup(model);
    const entry = hub.tags.find((tag) => tagKey(tag.tag) === tagKey(model));
    return entry
      ? { found: true, tag: entry.tag, sizeBytes: entry.sizeBytes, sourcePage: hub.url }
      : { found: false, tag: model, sourcePage: hub.url, message: `${hub.family} lists no GGUF build tagged ${model}.` };
  }
  const ref = libraryReference(model);
  if (!ref) throw new Error("Use an exact Ollama family:tag or hf.co/<owner>/<repo>:<quant>, not a URL or path.");
  const library = await ollamaLibrary.lookup(model);
  if (library.status === "not_found") {
    return { found: false, tag: model, sourcePage: ref.url, message: library.message ?? `ollama.com has no family ${ref.family}.` };
  }
  if (library.status !== "available") throw new Error(library.message ?? `ollama.com lookup ${library.status}.`);
  const entry = library.tags!.find((tag) => tagKey(libraryReference(tag.tag)?.tag ?? tag.tag) === tagKey(ref.tag));
  return entry
    ? { found: true, tag: entry.tag, sizeBytes: entry.sizeBytes, sourcePage: ref.url }
    : { found: false, tag: model, sourcePage: ref.url, message: `${ref.family} lists no tag ${ref.tag}.` };
}

/** Installed tags with their digests, read from Ollama's /api/tags. */
export async function installedInventory(ollamaEndpoint: string | undefined): Promise<InstalledModel[]> {
  const tags = (await ollamaFetch(ollamaEndpoint, "/api/tags")) as { models?: Array<{ name?: string; model?: string; digest?: string; capabilities?: unknown; size?: unknown }> };
  return (tags.models ?? []).map((m) => ({
    name: m.name ?? m.model ?? "", digest: m.digest ?? null,
    ...(Array.isArray(m.capabilities) ? { capabilities: m.capabilities.map(String) } : {}),
    ...(typeof m.size === "number" ? { size: m.size } : {}),
  })).filter((m) => m.name);
}


/** Memory free now, as serve's host sampler sees it (reclaimable included); null when serve is down. */
export async function liveAvailableBytes(endpoint: string | undefined): Promise<number | null> {
  try {
    const response = await fetch(`${(endpoint ?? DEFAULT_SERVE_ENDPOINT).replace(/\/$/, "")}/_freellama/v1/status`, { headers: await serveAuthHeaders(), signal: AbortSignal.timeout(3000) });
    const status = await response.json() as { host?: { available_memory_bytes?: number; effective_available_bytes?: number } };
    return status.host?.effective_available_bytes ?? status.host?.available_memory_bytes ?? null;
  } catch {
    return null;
  }
}

/** What models{view:"catalog"} and run_task {job} judge picks by: the inventory (null when unreadable) and free memory. */
export async function catalogLiveState(endpoint: string | undefined, ollamaEndpoint: string | undefined): Promise<[InstalledModel[] | null, number | null]> {
  let installed: InstalledModel[] | null = null;
  try {
    installed = await installedInventory(ollamaEndpoint);
  } catch { /* reported as null */ }
  return [installed, await liveAvailableBytes(endpoint)];
}
