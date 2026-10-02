import { DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS } from "./config.js";
import { parseModelCard, parseModelSearch, parseModelTags } from "./model-search.js";

const CACHE_TTL_MS = 60 * 60 * 1000;
const FAILURE_TTL_MS = 60 * 1000;
const MAX_PAGE_BYTES = 2 * 1024 * 1024;
const MAX_README_CHARS = 64 * 1024;

/** Resolve an exact public name; never guess an alias's base model or accept an arbitrary URL. */
export function libraryReference(model: string) {
  const match = model.match(/^(?:library\/)?([a-zA-Z0-9][\w.-]*(?:\/[a-zA-Z0-9][\w.-]*)?)(?::([\w.-]+))?$/);
  if (!match || match[1].split("/").some((part) => part === "." || part === "..")) return null;
  const family = match[1];
  const pathname = family.includes("/") ? family : `library/${family}`;
  return { family, tag: `${family}:${match[2] ?? "latest"}`, url: `https://ollama.com/${pathname}` };
}

type Card = ReturnType<typeof parseModelCard>;
type Tag = ReturnType<typeof parseModelTags>["tags"][number];
export type LibraryRecord = {
  status: "available" | "not_found" | "unavailable" | "parse_error" | "unmatched";
  sourceUrl: string | null;
  tagsSourceUrl: string | null;
  fetchedAt: string | null;
  expiresAt: string | null;
  cached: boolean;
  card?: Card;
  tags?: Tag[];
  readmeTruncated?: boolean;
  message?: string;
};

class LibraryFetchError extends Error {
  constructor(readonly status: "not_found" | "unavailable" | "parse_error", message: string) { super(message); }
}

/** GET-only, bounded Ollama enrichment. Failed lookups never erase local inventory. */
export class OllamaLibraryClient {
  private cache = new Map<string, LibraryRecord>();
  private pending = new Map<string, Promise<LibraryRecord>>();

  constructor(private readonly options: {
    fetch?: typeof fetch;
    now?: () => number;
    timeoutMs?: number;
    maxEntries?: number;
  } = {}) {}

  async search(params: URLSearchParams) {
    const url = `https://ollama.com/search?${params.toString()}`;
    const models = parseModelSearch(await this.page(url));
    return { url, models, fetchedAt: new Date((this.options.now ?? Date.now)()).toISOString() };
  }

  async lookup(model: string): Promise<LibraryRecord> {
    const ref = libraryReference(model);
    if (!ref) return { status: "unmatched", sourceUrl: null, tagsSourceUrl: null, fetchedAt: null,
      expiresAt: null, cached: false, message: "No exact Ollama public name could be resolved. Base-model provenance is unknown." };
    const now = this.options.now ?? Date.now;
    const cached = this.cache.get(ref.family);
    if (cached && Date.parse(cached.expiresAt!) > now()) {
      this.cache.delete(ref.family);
      this.cache.set(ref.family, cached);
      return { ...cached, cached: true };
    }
    const pending = this.pending.get(ref.family);
    if (pending) return { ...await pending, cached: true };
    const work = this.load(ref).then((record) => {
      this.cache.delete(ref.family);
      this.cache.set(ref.family, record);
      while (this.cache.size > (this.options.maxEntries ?? 128)) this.cache.delete(this.cache.keys().next().value!);
      return record;
    });
    this.pending.set(ref.family, work);
    try { return await work; } finally { this.pending.delete(ref.family); }
  }

  private async page(url: string): Promise<string> {
    const response = await (this.options.fetch ?? fetch)(url, {
      method: "GET", redirect: "error", headers: { accept: "text/html" },
      signal: AbortSignal.timeout(this.options.timeoutMs ?? Math.min(DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS * 1000, 10_000)),
    });
    if (!response.ok) {
      await response.body?.cancel();
      throw new LibraryFetchError(response.status === 404 ? "not_found" : "unavailable", `Ollama library returned HTTP ${response.status}.`);
    }
    if (!(response.headers.get("content-type") ?? "").includes("text/html")) {
      await response.body?.cancel();
      throw new LibraryFetchError("parse_error", "Ollama library did not return HTML.");
    }
    if (Number(response.headers.get("content-length")) > MAX_PAGE_BYTES) {
      await response.body?.cancel();
      throw new LibraryFetchError("unavailable", "Ollama library page exceeds the 2 MiB limit.");
    }
    const reader = response.body?.getReader();
    if (!reader) throw new LibraryFetchError("parse_error", "Ollama library returned an empty body.");
    const chunks: Uint8Array[] = [];
    let bytes = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        bytes += value.byteLength;
        if (bytes > MAX_PAGE_BYTES) throw new LibraryFetchError("unavailable", "Ollama library page exceeds the 2 MiB limit.");
        chunks.push(value);
      }
      return Buffer.concat(chunks).toString("utf8");
    } finally { await reader.cancel(); }
  }

  private async load(ref: NonNullable<ReturnType<typeof libraryReference>>): Promise<LibraryRecord> {
    let result: Pick<LibraryRecord, "status" | "card" | "tags" | "readmeTruncated" | "message">;
    try {
      // Independent pages; wait for both even if one fails so every request is accounted for.
      const pages = await Promise.allSettled([this.page(ref.url), this.page(`${ref.url}/tags`)]);
      if (pages[0].status === "rejected") throw pages[0].reason;
      if (pages[1].status === "rejected") throw pages[1].reason;
      let card: Card;
      try { card = parseModelCard(pages[0].value); }
      catch { throw new LibraryFetchError("parse_error", "Ollama model-card markup was not recognized."); }
      const { tags } = parseModelTags(pages[1].value, ref.family);
      if (tags.length === 0) throw new LibraryFetchError("parse_error", "No tag rows were recognized; check the source page before concluding a model is absent.");
      result = { status: "available", tags, readmeTruncated: card.readme.length > MAX_README_CHARS,
        card: { ...card, readme: card.readme.slice(0, MAX_README_CHARS) } };
    } catch (error) {
      result = { status: error instanceof LibraryFetchError ? error.status : "unavailable",
        message: error instanceof LibraryFetchError ? error.message : "Ollama library lookup failed or timed out. Local metadata remains available." };
    }
    const now = (this.options.now ?? Date.now)();
    return { ...result, sourceUrl: ref.url, tagsSourceUrl: `${ref.url}/tags`, fetchedAt: new Date(now).toISOString(),
      expiresAt: new Date(now + (result.status === "available" ? CACHE_TTL_MS : FAILURE_TTL_MS)).toISOString(), cached: false };
  }
}

const supportedUses: Record<string, { use: string; reason: string }> = {
  completion: { use: "text_generation", reason: "Ollama advertises completion support; task accuracy still needs evaluation." },
  tools: { use: "tool_calling", reason: "Ollama advertises tool-call support; this does not establish reliable agent execution." },
  vision: { use: "image_understanding", reason: "Ollama advertises image-input support; OCR and visual accuracy are unmeasured here." },
  audio: { use: "audio_input", reason: "Ollama advertises audio support; inspect the model instructions for the supported audio workflow." },
  embedding: { use: "embeddings", reason: "Ollama advertises embedding support for retrieval and similarity; retrieval quality needs evaluation." },
  thinking: { use: "thinking", reason: "Ollama advertises a thinking mode; it does not establish reasoning accuracy." },
  insert: { use: "fill_in_middle", reason: "Ollama advertises suffix-based insertion support." },
  image: { use: "image_generation", reason: "Ollama advertises image-generation support." },
};

const advertisedUses = [
  { task: "coding", pattern: /\b(coding|code generation|software engineering)\b/i, requires: "completion" },
  { task: "tool_calling", pattern: /\b(tool (?:use|calling)|function calling)\b/i, requires: "tools" },
  { task: "image_understanding", pattern: /\b(vision|image understanding|processing text and images|multimodal|OCR|optical character recognition)\b/i, requires: "vision" },
  { task: "reasoning", pattern: /\b(reasoning|mathematical|math tasks)\b/i, requires: "completion" },
  { task: "embeddings", pattern: /\b(embedding|embeddings|text encoder)\b/i, requires: "embedding" },
  { task: "summarization", pattern: /\b(summarization|summarisation|summarize|summarise)\b/i, requires: "completion" },
  { task: "question_answering", pattern: /\b(question answering|question-answering)\b/i, requires: "completion" },
  { task: "translation", pattern: /\b(translation|translate)\b/i, requires: "completion" },
  { task: "long_context", pattern: /\b(long[- ]context|repository[- ]scale understanding)\b/i, requires: "completion" },
] as const;

function prose(card: Card): string[] {
  return `${card.description}\n${card.readme}`.replace(/```[\s\S]*?```|~~~[\s\S]*?~~~/g, "")
    .replace(/<script\b[^>]*>[\s\S]*?<\/script>/gi, "")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, "").replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .split(/\n|(?<=[.!?])\s+/).map((line) => line.replace(/^[\s#>*-]+/, "").trim()).filter(Boolean);
}

/** Website suggestions stay family-scoped and cannot establish capabilities or quality. */
export function publicModelGuidance(record: LibraryRecord, capabilities?: string[], includeReadme = false) {
  const card = record.card;
  if (!card) return { advertisedUseCases: [], usageNotes: [] };
  const sentences = prose(card);
  const advertisedUseCases = advertisedUses.flatMap(({ task, pattern, requires }) => {
    if (capabilities && !capabilities.includes(requires)) return [];
    const sentence = sentences.find((sentence) => pattern.test(sentence) &&
      !/\b(not|never|cannot|unsupported|without)\b|doesn['’]t/i.test(sentence));
    const start = sentence ? Math.max(0, sentence.search(pattern) - 120) : 0;
    return sentence ? [{ task, basis: "advertised", scope: "family", quality: "unmeasured",
      evidence: { excerpt: sentence.slice(start, start + 320), sourceUrl: record.sourceUrl, fetchedAt: record.fetchedAt } }] : [];
  });
  const usageNotes = sentences.filter((sentence) => /\b(requires?|only|cannot|limitations?|must|memory|languages?|prefix)\b|not support|context (window|length)/i.test(sentence))
    .slice(0, 8).map((text) => ({ text: text.slice(0, 400), scope: "family", sourceUrl: record.sourceUrl }));
  return { description: card.description, advertisedFamilyCapabilities: card.capabilities, advertisedUseCases, usageNotes,
    readmeAvailable: card.readme.length > 0,
    ...(includeReadme ? { readme: { text: card.readme, format: card.readmeFormat, truncated: record.readmeTruncated ?? false } } : {}) };
}

function provenance(record: LibraryRecord) {
  const { card: _card, tags: _tags, readmeTruncated: _truncated, ...source } = record;
  return source;
}

export function modelKnowledge(model: string, data: Record<string, unknown>, record?: LibraryRecord, includeReadme = false,
  supportedFeaturesSource: "ollama_api" | "freellama_catalog" = "ollama_api") {
  const capabilities = Array.isArray(data.capabilities) ? data.capabilities.filter((item): item is string => typeof item === "string") : [];
  const supportedUseCases = capabilities.flatMap((capability) => Object.hasOwn(supportedUses, capability)
    ? [{ capability, ...supportedUses[capability], basis: "supported_feature" }] : []);
  const local = { supportedFeatures: capabilities, supportedFeaturesSource, supportedUseCases, quality: "metadata_only",
    note: "Supported features and advertised uses do not establish task quality. Keep exact-model benchmark and policy evidence separate." };
  if (data.remote_host || data.remote_model || /-cloud$/i.test(model)) {
    return { ...local, localModel: false, library: { status: "remote_model", message: "This Ollama entry points to a remote model." } };
  }
  if (!record) return { ...local, localModel: true, library: { status: "not_requested" } };
  if (record.status !== "available") return { ...local, localModel: true, library: provenance(record) };
  const ref = libraryReference(model)!;
  const tag = record.tags?.find((tag) => tag.tag === ref.tag);
  const digest = typeof data.digest === "string" ? data.digest.replace(/^sha256:/, "").toLowerCase() : null;
  const validDigest = digest && /^[\da-f]{64}$/.test(digest);
  const matched = validDigest && tag?.digest && digest.startsWith(tag.digest);
  const status = !tag ? "tag_not_found" : !validDigest || !tag.digest ? "unverified" : matched ? "matched" : "digest_mismatch";
  if (!matched || tag?.cloud) return { ...local, localModel: true, library: { ...provenance(record), status: tag?.cloud ? "remote_tag" : status,
    message: "Public guidance was withheld because an exact local tag and digest match could not be verified.", tag: tag ?? null } };
  return { ...local, localModel: true, library: { ...provenance(record), status, tag,
    ...publicModelGuidance(record, capabilities, includeReadme) } };
}

/** Enrich at most four families concurrently and skip known remote entries. */
export async function enrichInstalledModels(models: Array<Record<string, unknown>>, client: OllamaLibraryClient, includeLibrary: boolean,
  loadTags?: (upstream: string | undefined) => Promise<Array<Record<string, unknown>>>) {
  const results = new Array<Record<string, unknown>>(models.length);
  const inventories = new Map<string | undefined, Promise<Array<Record<string, unknown>>>>();
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(4, models.length) }, async () => {
    while (next < models.length) {
      const index = next++;
      const model = models[index];
      const name = String(model.name ?? model.model ?? "");
      let local = model;
      let featureSource: "ollama_api" | "freellama_catalog" = "freellama_catalog";
      let localError: string | undefined;
      if (includeLibrary && loadTags) {
        const upstream = (model.execution as { upstream?: string } | undefined)?.upstream;
        let inventory = inventories.get(upstream);
        if (!inventory) {
          inventory = loadTags(upstream);
          inventories.set(upstream, inventory);
        }
        try {
          const entry = (await inventory).find((entry) => String(entry.name ?? entry.model) === name);
          if (!entry || entry.digest !== model.digest) localError = "Local inventory changed; inspect models again before matching public guidance.";
          else {
            local = { ...model, ...entry };
            if (Array.isArray(entry.capabilities)) featureSource = "ollama_api";
          }
        } catch { localError = "The selected Ollama backend's inventory could not be verified; public guidance was withheld."; }
      }
      const record = includeLibrary && !localError && !local.remote_host && !local.remote_model && !/-cloud$/i.test(name) ? await client.lookup(name) : undefined;
      const knowledge = modelKnowledge(name, local, record, false, featureSource);
      results[index] = { ...model, knowledge: localError ? { ...knowledge, library: { status: "unverified", message: localError } } : knowledge };
    }
  }));
  return results;
}
