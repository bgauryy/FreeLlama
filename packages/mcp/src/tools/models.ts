// models: installed/resident/detail/raw lists and the two-step public library lookup.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { machine } from "../native.js";
import { listModels } from "../serve-client.js";
import { pageLiveList } from "../paging.js";
import {
  endpointParam, errorResult, isStructuredSuccess, ollamaEndpointParam, ollamaFetch, parsedResult,
  structuredResult,
} from "../helpers.js";
import { libraryTrialCandidate } from "../model-search.js";
import { HUB_EMBEDDING_TASKS, HuggingFaceClient, huggingFaceReference } from "../huggingface.js";
import {
  enrichInstalledModels, isRemoteModel, libraryReference, modelKnowledge, OllamaLibraryClient, publicModelGuidance,
} from "../model-knowledge.js";

const modelsResultSchema = z.object({
  models: z.array(z.record(z.unknown())).optional(), tags: z.array(z.record(z.unknown())).optional(), page: z.unknown().optional(),
  knowledge: z.unknown().optional(), library: z.unknown().optional(),
}).passthrough();

const ollamaLibrary = new OllamaLibraryClient();
const huggingFace = new HuggingFaceClient();

/**
 * Share of host memory one library tag may occupy before it is reported as not fitting. Leaves
 * headroom for resident runners, context/KV caches and the OS; it is a download-size screen, not
 * an accelerator-fit measurement.
 */
const HOST_MEMORY_FIT_FRACTION = 0.6;

// ollama.com filters by any capability word it shows on a card, and adds new ones (audio and
// decision arrived in 2026), so the filter takes any lowercase word instead of a fixed list.
type Capability = string;
// The Hub filters by one task; only these capabilities have one. Others search Ollama alone.
// Hub tasks per capability: embedding models are tagged either way on the Hub.
const HUB_TASKS = new Map<string, string[]>([
  ["embedding", ["feature-extraction", "sentence-similarity"]],
  ["vision", ["image-text-to-text"]],
]);

// Annotate public tags with local installation and a download-size memory screen, then pick a trial tag.
function annotateTags<T extends { tag: string; sizeBytes: number | null; cloud: boolean }>(
  tags: T[], installed: Set<string> | null, memoryBytes: number | null, embedding: boolean,
) {
  const budget = memoryBytes ? memoryBytes * HOST_MEMORY_FIT_FRACTION : null;
  // Ollama keeps a Hugging Face pull under the name it was given; compare case-insensitively.
  const local = installed ? new Set([...installed].map((name) => name.toLowerCase())) : null;
  const annotated = tags.map((t) => ({
    ...t,
    installed: local ? local.has(t.tag.toLowerCase()) : null,
    fitsInMemory: budget && t.sizeBytes ? t.sizeBytes <= budget : null,
    fitScope: "host_memory_budget_only",
  }));
  const sized = annotated.filter((t) => t.fitsInMemory === true && !t.cloud && t.sizeBytes);
  const best = budget
    ? sized.sort((a, b) => embedding ? (a.sizeBytes ?? 0) - (b.sizeBytes ?? 0) : (b.sizeBytes ?? 0) - (a.sizeBytes ?? 0))[0]
    : undefined;
  return { annotated, budget, best };
}

const NO_PROFILE = "No machine profile (freellama serve unreachable), so memory fit could not be checked and no tag is recommended. Start serve, or read the sizes yourself.";

// `route` was folded into `run_task { preview: true }` — same NAPI `route()` call, no second
// schema. `search_models` was folded into `models { view: "library" }` — same two-step lookup.
async function libraryLookup({
  model,
  query,
  capabilities,
  order,
  source,
  limit,
  cursor,
  includeReadme,
  endpoint,
  ollamaEndpoint,
}: {
  model?: string;
  query?: string;
  capabilities?: Array<Capability>;
  order?: "popular" | "newest";
  source?: "ollama" | "huggingface";
  limit?: number;
  cursor?: string;
  includeReadme?: boolean;
  endpoint?: string;
  ollamaEndpoint?: string;
}) {
  const localState = async () => {
    let installed: Set<string> | null = null;
    let installedUnavailable: string | undefined;
    let memoryBytes: number | null = null;
    try {
      const tags = (await ollamaFetch(ollamaEndpoint, "/api/tags")) as { models?: Array<{ name?: string }> };
      installed = new Set((tags.models ?? []).map((m) => m.name ?? ""));
    } catch (error) {
      // Unknown is not "not installed": report null plus the reason instead of a false negative.
      installedUnavailable = `Local inventory could not be read (${error instanceof Error ? error.message : String(error)}); installed is null.`;
    }
    try {
      const profile = JSON.parse(await machine(endpoint));
      memoryBytes = profile.memory_bytes ?? profile.unified_memory_bytes ?? null;
    } catch {
      // serve unreachable: recommendationUnavailable below explains the missing fit check.
    }
    return { installed, installedUnavailable, memoryBytes };
  };

  if (!model && cursor !== undefined) {
    return errorResult(new Error("Library step 1 has no `cursor`; it pages step-2 tags. Use `limit` for search, or pass `model`."));
  }
  try {
    if (model && huggingFaceReference(model)) {
      const hub = await huggingFace.lookup(model);
      const { installed, installedUnavailable, memoryBytes } = await localState();
      const embedding = HUB_EMBEDDING_TASKS.has(hub.task ?? "");
      const { annotated, budget, best } = annotateTags(hub.tags, installed, memoryBytes, embedding);
      const tagPage = pageLiveList(annotated, limit, cursor, (tag) => tag.tag);
      return structuredResult({
        family: hub.family,
        source: "huggingface",
        sourcePage: hub.url,
        library: { status: hub.tags.length ? "available" : "no_gguf", fetchedAt: hub.fetchedAt, task: hub.task,
          revision: hub.revision,
          ...(hub.gated ? { gated: hub.gated, gatedNote: "Accept the repo's terms on Hugging Face first; its Ollama guide documents private pulls through your Ollama SSH key added to your Hugging Face account." } : {}),
          architecture: hub.architecture, contextLength: hub.contextLength, parameters: hub.parameters,
          downloads: hub.downloads, likes: hub.likes, updated: hub.updated,
          ...(hub.projectors.length ? { visionProjectors: hub.projectors } : {}) },
        ...(installedUnavailable ? { installedUnavailable } : {}),
        machineMemoryBytes: memoryBytes,
        fitBudgetBytes: budget,
        tags: tagPage.items,
        page: { returned: tagPage.returned, total: tagPage.total, next_cursor: tagPage.next_cursor },
        pull: hub.tags.length
          ? `ollama_manage {action:"pull", model:"<tag>"} after approval; Ollama picks Q4_K_M for a bare ${hub.family} when the repo has it.`
          : "This repository has no GGUF files Ollama can pull.",
        recommendationUnavailable: budget ? undefined : NO_PROFILE,
        recommendation: best ? libraryTrialCandidate(best.tag, embedding) : null,
      });
    }
    if (model) {
      const ref = libraryReference(model);
      if (!ref) throw new Error("Use an exact Ollama family or namespace/model name, not a URL or path.");
      const { family } = ref;
      const library = await ollamaLibrary.lookup(model);
      if (library.status !== "available") return structuredResult({ family, sourcePage: ref.url, tags: [],
        tagsUnavailable: library.message, library, recommendation: null });
      // An available record always carries at least one tag; the client reports none as parse_error.
      const tags = library.tags!;
      const { installed, installedUnavailable, memoryBytes } = await localState();
      const isEmbeddingFamily = library.card!.capabilities.includes("embedding");
      const { annotated, budget, best } = annotateTags(tags, installed, memoryBytes, isEmbeddingFamily);
      const tagPage = pageLiveList(annotated, limit, cursor, (tag) => tag.tag);
      return structuredResult({
        family,
        source: "ollama",
        sourcePage: ref.url,
        library: { status: library.status, sourceUrl: library.sourceUrl, tagsSourceUrl: library.tagsSourceUrl,
          fetchedAt: library.fetchedAt, expiresAt: library.expiresAt, cached: library.cached,
          ...publicModelGuidance(library, undefined, includeReadme) },
        ...(installedUnavailable ? { installedUnavailable } : {}),
        machineMemoryBytes: memoryBytes,
        fitBudgetBytes: budget,
        tags: tagPage.items,
        page: { returned: tagPage.returned, total: tagPage.total, next_cursor: tagPage.next_cursor },
        recommendationUnavailable: budget ? undefined : NO_PROFILE,
        recommendation: best ? libraryTrialCandidate(best.tag, isEmbeddingFamily) : null,
      });
    }

    // Step 1 searches ollama.com and, for a text query, the Hugging Face Hub's GGUF repositories.
    const params = new URLSearchParams();
    for (const c of capabilities ?? []) params.append("c", c);
    if (query) params.set("q", query);
    if (order === "newest") params.set("o", "newest");
    const hubTasks = (capabilities ?? []).map((c) => HUB_TASKS.get(c));
    const hubSkipped = source === "ollama" ? "source is ollama"
      : !query ? "the Hub is searched only with a query"
      : hubTasks.length > 1 || hubTasks.some((tasks) => !tasks)
        ? `the Hub has no filter for ${(capabilities ?? []).filter((c) => !HUB_TASKS.has(c)).join(", ") || "several capabilities at once"}`
        : null;
    if (source === "huggingface" && hubSkipped) throw new Error(`source "huggingface" cannot be searched: ${hubSkipped}.`);
    const searchHub = async () => {
      const results = await Promise.all((hubTasks[0] ?? [undefined]).map((task) =>
        huggingFace.search(query!, { limit: limit ?? 10, order, task })));
      // One task filter per Hub request: merge, keep each repo once, best first.
      const seen = new Set<string>();
      const models = results.flatMap((r) => r.models).filter((m) => !seen.has(m.name) && seen.add(m.name))
        .sort((a, b) => order === "newest" ? String(b.updated).localeCompare(String(a.updated)) : (b.downloads ?? 0) - (a.downloads ?? 0))
        .slice(0, limit ?? 10);
      return { url: results[0].url, models, fetchedAt: results[0].fetchedAt };
    };
    const [ollamaSearch, hubSearch] = await Promise.allSettled([
      source === "huggingface" ? Promise.resolve(null) : ollamaLibrary.search(params),
      hubSkipped ? Promise.resolve(null) : searchHub(),
    ]);
    // Every source that was tried failed: that is the answer, not an empty success.
    const tried = [source === "huggingface" ? null : ollamaSearch, hubSkipped ? null : hubSearch].filter(Boolean) as PromiseSettledResult<unknown>[];
    if (tried.every((result) => result.status === "rejected")) {
      throw new Error(tried.map((result) => (result as PromiseRejectedResult).reason instanceof Error
        ? (result as PromiseRejectedResult).reason.message : String((result as PromiseRejectedResult).reason)).join("; "));
    }
    const ollama = ollamaSearch.status === "fulfilled" ? ollamaSearch.value : null;
    const hub = hubSearch.status === "fulfilled" ? hubSearch.value : null;
    const ollamaRows = (ollama?.models ?? []).slice(0, limit ?? 10).map((m) => ({ ...m, source: "ollama" as const }));
    const hubRows = hub?.models ?? [];
    const { installed, installedUnavailable } = await localState();
    const local = installed ? [...installed].map((n) => n.toLowerCase()) : null;
    const isInstalled = (name: string) => local ? local.some((n) => n.split(":")[0] === name.toLowerCase()) : null;
    const reason = (result: PromiseSettledResult<unknown>) =>
      result.status === "rejected" ? (result.reason instanceof Error ? result.reason.message : String(result.reason)) : undefined;
    return structuredResult({
      query: ollama?.url ?? hub?.url ?? null,
      ...(ollama && hub ? { hubQuery: hub.url } : {}),
      fetchedAt: ollama?.fetchedAt ?? hub?.fetchedAt ?? null,
      ...(ollama && ollamaRows.length === 0 ? { searchUnavailable: "No ollama.com result cards were recognized. The query may have no matches or Ollama's markup may have changed; inspect the source URL." } : {}),
      ...(reason(ollamaSearch) ? { ollamaUnavailable: reason(ollamaSearch) } : {}),
      ...(reason(hubSearch) ? { huggingFaceUnavailable: reason(hubSearch) } : {}),
      ...(hubSkipped && source !== "ollama" ? { huggingFaceSkipped: hubSkipped } : {}),
      order: order ?? "popular",
      count: ollamaRows.length + hubRows.length,
      ...(installedUnavailable ? { installedUnavailable } : {}),
      nextStep: 'Not pullable yet. Call again with view:"library", model:"<name>" (an Ollama family or hf.co/<owner>/<repo>) to get tags, sizes and memory fit.',
      models: [...ollamaRows, ...hubRows].map((m) => ({ ...m, installed: isInstalled(m.name) })),
    });
  } catch (error) {
    return errorResult(error);
  }
}

export function registerModelsTool(server: McpServer): void {
  server.registerTool(
    "models",
    {
      description:
        "Use when: listing installed or loaded models with capabilities, policy rank and memory fit, inspecting one model, or searching ollama.com and Hugging Face GGUF repos (view:library). " +
        "Do not use when: running work (run_task) or pulling (ollama_manage). Returns: models; library lists families first, then one family's tags.",
      inputSchema: z.object({
        view: z
          .enum(["installed", "resident", "detail", "raw", "library"])
          .optional(),
        model: z.string().min(1).optional(),
        includeVerbose: z
          .boolean()
          .optional(),
        includeLibrary: z.boolean().optional(),
        includeReadme: z.boolean().optional(),
        query: z.string().min(1).optional(),
        capabilities: z
          .array(z.string().regex(/^[a-z][a-z-]*$/).describe("ollama.com capability word, e.g. vision, tools, thinking, embedding, audio, decision, cloud"))
          .min(1)
          .max(8)
          .optional(),
        order: z
          .enum(["popular", "newest"])
          .optional(),
        source: z.enum(["ollama", "huggingface"]).optional(),
        limit: z.number().int().positive().max(50).optional(),
        cursor: z.string().min(1).optional(),
        endpoint: endpointParam,
        ollamaEndpoint: ollamaEndpointParam,
      }).strict(),
      outputSchema: modelsResultSchema,
      annotations: { readOnlyHint: true },
    },
    async ({ view, model, includeVerbose, includeLibrary, includeReadme, query, capabilities, order, source, limit, cursor, endpoint, ollamaEndpoint }) => {
      try {
        const selectedView = view ?? "installed";
        if (includeLibrary !== undefined && selectedView !== "installed" && selectedView !== "detail") {
          return errorResult(new Error('`includeLibrary` is valid only for views "installed" and "detail".'));
        }
        if (includeReadme !== undefined && !(selectedView === "library" && model || selectedView === "detail" && includeLibrary === true)) {
          return errorResult(new Error('`includeReadme` needs library step 2, or view "detail" with `includeLibrary:true`.'));
        }
        if (selectedView !== "library" && [query, capabilities, order, source].some((value) => value !== undefined)) {
          return errorResult(new Error(`view "${selectedView}" does not accept library search fields.`));
        }
        if (selectedView !== "library" && selectedView !== "raw" && [limit, cursor].some((value) => value !== undefined)) {
          return errorResult(new Error(`view "${selectedView}" accepts neither pagination nor library search fields.`));
        }
        if (selectedView !== "detail" && includeVerbose !== undefined) {
          return errorResult(new Error('`includeVerbose` is valid only for view "detail".'));
        }
        if (selectedView !== "detail" && selectedView !== "library" && model !== undefined) {
          return errorResult(new Error('`model` is valid only for views "detail" and "library".'));
        }
        if (
          selectedView === "library" &&
          model &&
          [query, capabilities, order, source].some((value) => value !== undefined)
        ) {
          return errorResult(
            new Error('Library step 2 accepts only `model` plus endpoint overrides; omit step-1 search fields.'),
          );
        }

        switch (selectedView) {
          case "raw": {
            const raw = (await ollamaFetch(ollamaEndpoint, "/api/tags")) as Record<string, unknown>;
            const models = Array.isArray(raw.models) ? raw.models : [];
            const page = pageLiveList(models, limit, cursor, (model) => String((model as Record<string, unknown>).name ?? JSON.stringify(model)));
            return structuredResult({ ...raw, models: page.items, page: { returned: page.returned, total: page.total, next_cursor: page.next_cursor } });
          }

          case "resident": {
            const managed = parsedResult(await listModels(endpoint));
            if (!isStructuredSuccess(managed)) return managed;
            const data = managed.structuredContent as {
              models?: Array<Record<string, unknown>>;
            };
            // Ollama's /api/ps exposes only raw `size`/`size_vram` bytes; the Rust inventory derives
            // the GPU/CPU split as `resident_view`. Entries without valid resident bytes carry none.
            const models = (data.models ?? []).filter((entry) => entry.resident === true).map((entry) => {
              const { resident_view: view, ...rest } = entry as Record<string, unknown> & {
                resident_view?: { gpu_percent: number; processor_label: string; assigned: boolean; warning?: string };
              };
              if (!view) return rest;
              return {
                ...rest,
                placement: {
                  gpu_percent: view.gpu_percent,
                  assigned: view.assigned,
                  processor: view.processor_label,
                  ...(view.warning !== undefined ? { warning: view.warning } : {}),
                },
              };
            });
            return structuredResult({ ...data, models });
          }

          case "detail": {
            if (!model) {
              return errorResult(new Error('view "detail" needs `model` set to an installed tag.'));
            }
            const data = (await ollamaFetch(ollamaEndpoint, "/api/show", {
              method: "POST",
              body: { model },
            })) as Record<string, unknown>;
            let local = data;
            if (includeLibrary && !isRemoteModel(data, model)) {
              // /show does not guarantee a manifest digest. Use the exact /tags entry, never a name guess.
              try {
                const inventory = await ollamaFetch(ollamaEndpoint, "/api/tags") as { models?: Array<Record<string, unknown>> };
                const canonical = libraryReference(model)?.tag;
                const entry = canonical && inventory.models?.find((entry) => libraryReference(String(entry.name ?? entry.model))?.tag === canonical);
                if (entry) local = { ...entry, ...data, digest: entry.digest };
              } catch { /* Without a digest, enrichment reports unverified and withholds public guidance. */ }
            }
            const library = includeLibrary && !isRemoteModel(local, model)
              ? await ollamaLibrary.lookup(model) : undefined;
            const { license, modelfile, ...rest } = data;
            // The real ceiling hides under a per-architecture key (`qwen3_5.context_length`,
            // `llama.context_length`, ...), so it cannot be read by a fixed path.
            const modelInfo = (rest.model_info ?? {}) as Record<string, unknown>;
            const contextEntry = Object.entries(modelInfo).find(
              ([key, value]) => key.endsWith(".context_length") && typeof value === "number",
            );
            return structuredResult({
              ...rest,
              max_context_length: (contextEntry?.[1] as number) ?? null,
              knowledge: modelKnowledge(model, local, library, includeReadme),
              ...(includeVerbose ? { license, modelfile } : {}),
            });
          }

          case "library":
            return await libraryLookup({ model, query, capabilities, order, source, limit, cursor, includeReadme, endpoint, ollamaEndpoint });

          default: {
            const result = parsedResult(await listModels(endpoint));
            if (!isStructuredSuccess(result)) return result;
            const data = result.structuredContent as { models?: Array<Record<string, unknown>> };
            return structuredResult({ ...data, models: await enrichInstalledModels(data.models ?? [], ollamaLibrary, includeLibrary ?? false,
              async (upstream) => (await ollamaFetch(upstream ?? ollamaEndpoint, "/api/tags") as { models?: Array<Record<string, unknown>> }).models ?? []) });
          }
        }
      } catch (error) {
        return errorResult(error);
      }
    },
  );
}
