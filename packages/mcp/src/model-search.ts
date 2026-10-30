// Parsers for Ollama's public HTML library. Website claims are not routing evidence.
import { clipText } from "./helpers.js";

function decodeHtml(text: string): string {
  const entities: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " ", middot: "·", bull: "•" };
  return text.replace(/&(#x[\da-f]+|#\d+|[a-z]+);/gi, (raw, entity: string) => {
    if (!entity.startsWith("#")) return entities[entity.toLowerCase()] ?? raw;
    const code = entity[1].toLowerCase() === "x" ? Number.parseInt(entity.slice(2), 16) : Number(entity.slice(1));
    return code > 0 && code <= 0x10ffff && !(code >= 0xd800 && code <= 0xdfff) ? String.fromCodePoint(code) : raw;
  });
}

function attribute(tag: string, name: string): string | undefined {
  return tag.match(new RegExp(`(?:^|\\s)${name}\\s*=\\s*(["'])([\\s\\S]*?)\\1`, "i"))?.[2];
}

function htmlText(html: string): string {
  return decodeHtml(html.replace(/<(script|style|svg)\b[^>]*>[\s\S]*?<\/\1>/gi, "")
    .replace(/<\/(?:p|li|div|h[1-6]|pre|tr)>|<br\s*\/?\s*>/gi, "\n").replace(/<[^>]*>/g, " "))
    .split("\n").map((line) => line.replace(/\s+/g, " ").trim()).filter(Boolean).join("\n");
}

/** Read one nested div without coupling extraction to the surrounding Tailwind classes. */
function divContent(html: string, id: string): string | undefined {
  const tags = /<\/?div\b[^>]*>/gi;
  let start: number | undefined;
  let depth = 0;
  for (const match of html.matchAll(tags)) {
    if (start === undefined) {
      if (attribute(match[0], "id") !== id) continue;
      start = match.index! + match[0].length;
      depth = 1;
    } else {
      depth += /^<\//.test(match[0]) ? -1 : 1;
      if (depth === 0) return html.slice(start, match.index);
    }
  }
  return undefined;
}

export function parseModelCard(html: string) {
  const meta = [...html.matchAll(/<meta\b[^>]*>/gi)].find((m) => attribute(m[0], "name") === "description");
  const description = decodeHtml(meta ? attribute(meta[0], "content") ?? "" : "").trim();
  const editor = [...html.matchAll(/<textarea\b([^>]*)>([\s\S]*?)<\/textarea>/gi)]
    .find((m) => attribute(m[1], "id") === "editor");
  const rendered = divContent(html, "display");
  const readme = editor ? decodeHtml(editor[2]).trim() : rendered === undefined ? "" : htmlText(rendered);
  if (!description && !editor && rendered === undefined) throw new Error("Ollama library markup was not recognized.");
  // Only the header chips describe the family; README prose/code can contain arbitrary spans.
  const header = html.split(/<div[^>]*\bid=["']readme["']/i)[0];
  const capabilities = [...new Set([...header.matchAll(/<span\b[^>]*class=["'][^"']*text-(?:indigo-600|cyan-500)[^"']*["'][^>]*>\s*([a-z]+)\s*<\/span>/g)].map((m) => m[1]))];
  return { description, readme, readmeFormat: editor ? "markdown" : "text", capabilities };
}

/** A size-only trial candidate. Library metadata supplies no task-quality measurement. */
export function libraryTrialCandidate(tag: string, embedding: boolean) {
  return {
    tag,
    evidence: "download_size_preflight_only",
    taskQuality: "unmeasured",
    why: embedding
      ? "Smallest listed download inside the conservative host-memory budget. This is a trial candidate; smaller size does not establish better embedding quality."
      : "Largest listed download inside the conservative host-memory budget. This is a trial candidate; download size establishes neither accelerator fit nor task quality.",
    configure: "Set contextTokens and output limits for the workload, then inspect preview and execution receipts. Ollama defaults depend on its configuration and hardware. Use keepAlive:\"0\" for one-off work; keep related repeated work warm when resources permit.",
    caution: "Measure this exact tag on representative tasks before choosing it. Include resident runners, context/KV caches and OS headroom in memory fit. Discovery does not authorize a pull.",
  };
}

// A chip like "27b", "1.5b", "e4b" or "8x7b" names a parameter size; any other lowercase chip is a
// capability ("vision", "tools", "audio", "cloud", ...), so a capability Ollama adds later is kept.
const SIZE_CHIP = /^(?:\d+x)?(?:e)?\d+(?:\.\d+)?[kmbt]$/i;

/**
 * Parse ollama.com/search result cards.
 *
 * There is no JSON API — `Accept: application/json` still returns HTML, and `/api/search`,
 * `/search.json`, and the registry `_catalog` endpoint all 404. So this parses the rendered page by
 * meaning rather than by CSS class: one <li> per model with a /library/<name> (or namespace) link,
 * the first paragraph as the description, short chip spans as capabilities or sizes, and the
 * download and tag counts by their text. Both the 2025 and the October 2026 card layouts parse;
 * a redesign that breaks them surfaces as "0 results" with the source URL, not an exception.
 */
export function parseModelSearch(html: string) {
  const results = [];
  for (const block of html.split(/<li\b/).slice(1)) {
    const link = block.match(/href="\/(?:library\/([\w.-]+)|([\w.-]+\/[\w.-]+))"/);
    const name = link?.[1] ?? link?.[2];
    if (!name) continue;
    const rawDescription = block.match(/<p\b[^>]*>([\s\S]*?)<\/p>/)?.[1];
    if (rawDescription === undefined) continue;
    const description = decodeHtml(rawDescription.replace(/<[^>]+>/g, "")).replace(/\s+/g, " ").trim();
    const chips = [...block.matchAll(/<span\b[^>]*class="[^"]*\b(?:rounded-md|text-(?:indigo|cyan|blue)-\d+)\b[^"]*"[^>]*>\s*([\w.-]+)\s*<\/span>/g)]
      .map((m) => ({ text: m[1].toLowerCase(), at: m.index! }));
    // Capability chips share one container; a later row holds counts, sizes and other badges such
    // as "free". Without containers (older cards) every chip is in the first group.
    const first = chips.find((chip) => !SIZE_CHIP.test(chip.text));
    const groupStart = first ? block.lastIndexOf("<div", first.at) : -1;
    const groupEnd = groupStart >= 0 ? block.indexOf("</div>", first!.at) : -1;
    const inGroup = (at: number) => groupStart < 0 || groupEnd < 0 || (at > groupStart && at < groupEnd);
    const words = chips.filter((chip) => !SIZE_CHIP.test(chip.text) && /^[a-z]+$/.test(chip.text));
    const sizes = chips.filter((chip) => SIZE_CHIP.test(chip.text)).map((chip) => chip.text);
    // Family chips advertise features. A cloud chip does not prove the absence of local tags.
    const capabilities = [...new Set(words.filter((chip) => inGroup(chip.at)).map((chip) => chip.text))];
    const labels = [...new Set(words.filter((chip) => !inGroup(chip.at)).map((chip) => chip.text))];
    const legacyStat = (label: string) =>
      block.match(new RegExp(`<span >([\\d.,KMB]+)<\\/span>\\s*<span class="hidden sm:flex">&nbsp;${label}`))?.[1] ?? null;
    const downloads = block.match(/title="([\d,]+) downloads"/)?.[1];
    const pullsShort = block.match(/title="[\d,]+ downloads"[\s\S]*?<span\s*>([\d.,]+[KMB]?)<\/span>/)?.[1];
    const tagCount = block.match(/>\s*(\d+)\s+tags?\s*</)?.[1];
    results.push({
      name,
      description: clipText(description, 160),
      capabilities,
      ...(labels.length ? { labels } : {}),
      ...(sizes.length ? { sizes } : {}),
      pulls: pullsShort ?? legacyStat("Pulls"),
      ...(downloads ? { downloads: Number(downloads.replace(/,/g, "")) } : {}),
      tags: tagCount ?? legacyStat("Tag"),
      cloudAvailable: capabilities.includes("cloud"),
    });
  }
  return results;
}

/**
 * Parse the tag table on ollama.com/library/<name>.
 *
 * The /tags page contains all variants; the family page contains only featured variants.
 * Read rows carrying size/context metadata, independent of responsive CSS, and deduplicate links.
 * An omitted tag defaults to latest in Ollama; exact tags are needed for provenance and sizing.
 */
export function parseModelTags(html: string, family: string) {
  const tags = [];
  const seen = new Set<string>();
  const row = /<a\b([^>]*)>([\s\S]*?)<\/a>/gi;
  for (const m of html.matchAll(row)) {
    let tag: string;
    try { tag = decodeURIComponent(attribute(m[1], "href") ?? "").replace(/^\/(?:library\/)?/, ""); }
    catch { continue; }
    if (!tag.startsWith(`${family}:`)) continue;
    const text = htmlText(m[2]).replace(/\n/g, " ");
    if (!/context window|[\d.]\s*[MG]B\b|\bcloud\b/i.test(text)) continue;
    if (seen.has(tag)) continue;
    seen.add(tag);
    const meta = text.split(/[·•]/).map((x) => x.trim());
    const sizeText = meta.find((x) => /^[\d.]+\s?[MG]B$/i.test(x)) ?? null;
    const bytes = sizeText
      ? Number.parseFloat(sizeText) * (/GB/i.test(sizeText) ? 1e9 : 1e6)
      : null;
    tags.push({
      tag,
      digest: text.replace(tag, "").match(/\b[\da-f]{12,64}\b/i)?.[0]?.toLowerCase() ?? null,
      cloud: /-cloud$/i.test(tag) || meta.some((part) => /^cloud$/i.test(part)),
      size: sizeText,
      sizeBytes: bytes,
      context: meta.find((x) => /context window/i.test(x))?.replace(/\s*context window\s*/i, "") ?? null,
      modalities: meta.find((x) => /^(Text|Image|Audio)/i.test(x))?.replace(/\s+input.*$/i, "") ?? null,
      updated: meta.at(-1) ?? null,
    });
  }
  return { family, tags };
}
