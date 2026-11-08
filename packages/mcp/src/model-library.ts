// Public model sources shared by `models{view:"library"}` and `ollama_pull`: one cached client per
// source, and the local inventory with digests. No native addon, so lifecycle tools load it freely.
import { ollamaFetch } from "./helpers.js";
import { HuggingFaceClient, huggingFaceReference } from "./huggingface.js";
import type { InstalledModel } from "./model-catalog.js";
import { libraryReference, OllamaLibraryClient } from "./model-knowledge.js";

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
  const tags = (await ollamaFetch(ollamaEndpoint, "/api/tags")) as { models?: Array<{ name?: string; model?: string; digest?: string }> };
  return (tags.models ?? []).map((m) => ({ name: m.name ?? m.model ?? "", digest: m.digest ?? null })).filter((m) => m.name);
}

