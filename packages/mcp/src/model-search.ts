// Parsers for the public ollama.com library pages (no JSON API exists, so this reads the markup).
import { clipText } from "./helpers.js";

/**
 * Parse ollama.com/search result cards.
 *
 * There is no JSON API — `Accept: application/json` still returns HTML, and `/api/search`,
 * `/search.json`, and the registry `_catalog` endpoint all 404. So this parses the rendered page,
 * which means it is inherently coupled to Ollama's markup and can break on a redesign. Failures
 * surface as "0 results" rather than an exception, so the tool degrades to unhelpful instead of
 * broken; the shape it depends on is one <li> per model, each containing a /library/<name> link.
 */
export function parseModelSearch(html: string) {
  const results = [];
  for (const block of html.split(/<li\s/).slice(1)) {
    const name = block.match(/href="\/library\/([^"]+)"/)?.[1];
    if (!name) continue;
    const description =
      block
        .match(/<p class="max-w-lg[^"]*">([\s\S]*?)<\/p>/)?.[1]
        ?.replace(/<[^>]+>/g, "")
        .replace(/&#39;/g, "'")
        .replace(/&amp;/g, "&")
        .replace(/&quot;/g, '"')
        .replace(/\s+/g, " ")
        .trim() ?? "";
    // Indigo chips are runtime capabilities; the cyan "cloud" chip means the model runs on
    // Ollama's hosted service, NOT on this machine — the distinction that matters most here.
    const capabilities = [...block.matchAll(/text-(?:indigo-600|cyan-500)[^>]*>([a-z]+)<\/span>/g)].map(
      (m) => m[1],
    );
    const stat = (label: string) =>
      block.match(
        new RegExp(`<span >([\\d.,KMB]+)<\\/span>\\s*<span class="hidden sm:flex">&nbsp;${label}`),
      )?.[1] ?? null;
    results.push({
      name,
      description: clipText(description, 160),
      capabilities,
      pulls: stat("Pulls"),
      tags: stat("Tag"),
      cloudOnly: capabilities.includes("cloud"),
    });
  }
  return results;
}

/**
 * Parse the tag table on ollama.com/library/<name>.
 *
 * This is the step search cannot cover: search returns FAMILY names (`gemma4`), and a family is
 * not pullable — you pull a tag (`gemma4:12b`), and only the tag carries the size that decides
 * whether it fits in memory. The page renders each tag twice (a mobile row and a desktop grid);
 * the mobile row is the one with everything on a single line, so it is what gets parsed.
 */
export function parseModelTags(html: string, family: string) {
  const tags = [];
  const seen = new Set<string>();
  const row = /<a href="\/library\/([^"]+)" class="sm:hidden[\s\S]*?<p class="flex text-neutral-500">([^<]*)<\/p>/g;
  for (const m of html.matchAll(row)) {
    const tag = m[1];
    if (seen.has(tag)) continue;
    seen.add(tag);
    const meta = m[2].replace(/&middot;/g, "·").split("·").map((x) => x.trim());
    const sizeText = meta.find((x) => /^[\d.]+\s?[MG]B$/i.test(x)) ?? null;
    const bytes = sizeText
      ? Number.parseFloat(sizeText) * (/GB/i.test(sizeText) ? 1e9 : 1e6)
      : null;
    tags.push({
      tag,
      size: sizeText,
      sizeBytes: bytes,
      context: meta.find((x) => /context window/i.test(x))?.replace(/\s*context window\s*/i, "") ?? null,
      modalities: meta.find((x) => /^(Text|Image|Audio)/i.test(x)) ?? null,
      updated: meta.at(-1) ?? null,
    });
  }
  return { family, tags };
}
