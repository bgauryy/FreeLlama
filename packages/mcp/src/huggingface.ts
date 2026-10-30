// Hugging Face Hub as a second public model source. Ollama pulls any GGUF repository on the Hub as
// `hf.co/<owner>/<repo>:<quant>` (the quant is case-insensitive and defaults to Q4_K_M when the repo
// has one), so search lists GGUF repos and a lookup lists each quant with its exact file size.
// Hub metadata is advertising, not routing evidence, like the Ollama library card.
import { DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS } from "./config.js";

const HUB = "https://huggingface.co";
const MAX_JSON_BYTES = 4 * 1024 * 1024;

/** `hf.co/owner/repo[:quant]` or `huggingface.co/...` → repo, Ollama family name and quant. */
export function huggingFaceReference(model: string) {
  const match = model.match(/^(?:https?:\/\/)?(?:hf\.co|huggingface\.co)\/([\w.-]+)\/([\w.-]+?)(?::([\w.-]+))?$/i);
  if (!match || [match[1], match[2]].some((part) => part === "." || part === "..")) return null;
  const repo = `${match[1]}/${match[2]}`;
  return { repo, family: `hf.co/${repo}`, quant: match[3] ?? null, url: `${HUB}/${repo}` };
}

// Quant names as GGUF publishers write them: Q4_K_M, IQ3_XXS, Q8_0, BF16, F16, MXFP4, TQ1_0, and
// unsloth's UD-Q4_K_XL. The tag keeps the publisher's spelling, which is what Ollama resolves.
const QUANT = /^(?:UD-)?(?:I?Q\d(?:_[A-Z0-9]+)*|BF16|F16|F32|MXFP4(?:_MOE)?|TQ\d_\d)$/i;
const SPLIT = /-\d{5}-of-\d{5}$/;

/** The longest "-" or "."-delimited filename suffix that is a quant, else a quant-named folder. */
function quantOf(path: string): string | null {
  const parts = path.split("/");
  const stem = parts.pop()!.replace(/\.gguf$/i, "").replace(SPLIT, "");
  for (let index = 0; index <= stem.length; index++) {
    if (index > 0 && !/[-.]/.test(stem[index - 1])) continue;
    if (QUANT.test(stem.slice(index))) return stem.slice(index);
  }
  return parts.reverse().find((dir) => QUANT.test(dir)) ?? null;
}

const decimalSize = (bytes: number) =>
  bytes >= 1e9 ? `${(bytes / 1e9).toFixed(1)}GB` : `${Math.round(bytes / 1e6)}MB`;

type TreeEntry = { type?: string; path?: string; size?: number; lfs?: { size?: number } };

/**
 * List a repo's pullable GGUF builds: split shards of one file are summed, vision projectors set
 * apart. A build is tagged by its quant only when no other build in the repo shares that quant
 * (Ollama resolves the quant case-insensitively); otherwise, or when the name carries no quant, it
 * is tagged by its full filename, which Ollama also accepts.
 */
export function parseHuggingFaceFiles(tree: TreeEntry[], repo: string) {
  const builds = new Map<string, { path: string; file: string; quant: string | null; bytes: number; files: number }>();
  const projectors: string[] = [];
  for (const entry of tree) {
    if (entry.type !== "file" || !entry.path || !/\.gguf$/i.test(entry.path)) continue;
    const file = entry.path.split("/").pop()!;
    if (/mmproj/i.test(file)) { projectors.push(entry.path); continue; }
    // One build: the same folder and filename once the -0000N-of-0000M shard suffix is removed.
    const key = entry.path.replace(/-\d{5}-of-(\d{5})\.gguf$/i, "-00001-of-$1.gguf");
    const bytes = entry.lfs?.size ?? entry.size ?? 0;
    const build = builds.get(key) ?? { path: key, file: key.split("/").pop()!, quant: quantOf(entry.path), bytes: 0, files: 0 };
    builds.set(key, { ...build, bytes: build.bytes + bytes, files: build.files + 1 });
  }
  const perQuant = new Map<string, number>();
  for (const { quant } of builds.values()) if (quant) perQuant.set(quant.toLowerCase(), (perQuant.get(quant.toLowerCase()) ?? 0) + 1);
  const tags = [...builds.values()].sort((a, b) => a.bytes - b.bytes).map(({ path, file, quant, bytes, files }) => ({
    tag: `hf.co/${repo}:${quant && perQuant.get(quant.toLowerCase()) === 1 ? quant : file}`,
    quant, path, size: bytes ? decimalSize(bytes) : null, sizeBytes: bytes || null, files, cloud: false,
    // Hugging Face's Ollama guide shows single-file builds; say so rather than promise a pull.
    ...(files > 1 ? { note: `Split into ${files} files; the Hub's Ollama guide documents single-file GGUFs, so confirm the pull.` } : {}),
  }));
  return { tags, projectors };
}

type HubModel = { id?: string; pipeline_tag?: string; downloads?: number; likes?: number; lastModified?: string };

export function parseHuggingFaceSearch(models: HubModel[]) {
  return models.filter((model) => model.id).map((model) => ({
    name: `hf.co/${model.id}`,
    source: "huggingface" as const,
    task: model.pipeline_tag ?? null,
    downloads: model.downloads ?? null,
    likes: model.likes ?? null,
    updated: model.lastModified ?? null,
  }));
}

/** Hub tasks whose GGUF models FreeLlama routes as embeddings. */
export const HUB_EMBEDDING_TASKS = new Set(["feature-extraction", "sentence-similarity"]);

export class HuggingFaceClient {
  constructor(private readonly options: { fetch?: typeof fetch; now?: () => number; timeoutMs?: number } = {}) {}

  private async json<T>(url: string): Promise<T> {
    const response = await (this.options.fetch ?? fetch)(url, {
      method: "GET", headers: { accept: "application/json" },
      signal: AbortSignal.timeout(this.options.timeoutMs ?? Math.min(DEFAULT_OLLAMA_FETCH_TIMEOUT_SECONDS * 1000, 10_000)),
    });
    if (!response.ok) {
      await response.body?.cancel();
      throw new Error(response.status === 404 || response.status === 401
        ? `Hugging Face has no public model at ${url} (HTTP ${response.status}).`
        : `Hugging Face returned HTTP ${response.status}.`);
    }
    const text = await response.text();
    if (text.length > MAX_JSON_BYTES) throw new Error("Hugging Face response exceeds the 4 MiB limit.");
    return JSON.parse(text) as T;
  }

  private stamp() {
    return new Date((this.options.now ?? Date.now)()).toISOString();
  }

  /** GGUF repositories matching `query`, most downloaded (or newest) first. */
  async search(query: string, { limit = 10, order = "popular", task }: { limit?: number; order?: "popular" | "newest"; task?: string } = {}) {
    const params = new URLSearchParams({ search: query, filter: "gguf", sort: order === "newest" ? "lastModified" : "downloads",
      direction: "-1", limit: String(limit) });
    // The list endpoint omits lastModified unless asked; expand names every field read below.
    for (const field of ["downloads", "likes", "pipeline_tag", "lastModified"]) params.append("expand[]", field);
    if (task) params.set("pipeline_tag", task);
    const url = `${HUB}/api/models?${params}`;
    return { url, models: parseHuggingFaceSearch(await this.json<HubModel[]>(url)), fetchedAt: this.stamp() };
  }

  /** One repo's pullable quants, task, architecture and context length. */
  async lookup(model: string) {
    const ref = huggingFaceReference(model);
    if (!ref) throw new Error("Use hf.co/<owner>/<repo>, the name Ollama pulls from Hugging Face.");
    const info = await this.json<HubModel & { sha?: string; gated?: string | boolean; gguf?: { architecture?: string; context_length?: number; total?: number } }>(
      `${HUB}/api/models/${ref.repo}?expand[]=gguf&expand[]=pipeline_tag&expand[]=downloads&expand[]=likes&expand[]=lastModified&expand[]=sha&expand[]=gated`);
    // List the files at the revision the repo reports, not an assumed branch name.
    const tree = await this.json<TreeEntry[]>(`${HUB}/api/models/${ref.repo}/tree/${info.sha ?? "main"}?recursive=true`);
    return {
      ...ref,
      task: info.pipeline_tag ?? null,
      revision: info.sha ?? null,
      // "auto" or "manual": Hugging Face serves the files only after you accept the repo's terms.
      gated: info.gated && info.gated !== "false" ? info.gated : false,
      architecture: info.gguf?.architecture ?? null,
      contextLength: info.gguf?.context_length ?? null,
      parameters: info.gguf?.total ?? null,
      downloads: info.downloads ?? null,
      likes: info.likes ?? null,
      updated: info.lastModified ?? null,
      ...parseHuggingFaceFiles(tree, ref.repo),
      fetchedAt: this.stamp(),
    };
  }
}
