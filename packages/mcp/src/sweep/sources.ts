// What a sweep reads: files under the readable roots, or a bounded, polite crawl of public pages.
// Both end as labelled text chunks small enough for a local model to judge one at a time.
import { lookup } from "node:dns/promises";
import { isIP } from "node:net";
import path from "node:path";
import { chunkLines, expandFileEntries, MAX_TEXT_FILE_BYTES, readAllowedFile } from "../task-files.js";

export type CollectedChunk = { label: string; source: string; lines?: [number, number]; text: string };

/** Most chunks one sweep holds; a larger job is several sweeps the caller splits. */
export const MAX_SWEEP_CHUNKS = 5_000;
const MAX_PAGE_BYTES = 2 * 1024 * 1024;
const MAX_PAGE_TEXT = 400_000;
const FETCH_TIMEOUT_MS = 15_000;
/** Pause between requests to one site, so a crawl stays polite. */
const CRAWL_DELAY_MS = 500;
const USER_AGENT = "FreeLlama-sweep/0.9 (+https://github.com/bgauryy/FreeLlama)";

/** Chunks of text files, labelled `path:Lstart-Lend` (relative to the working directory when inside it). */
export function collectFiles(entries: string[], chunkBytes: number, cwd = process.cwd()): { chunks: CollectedChunk[]; skipped: { source: string; reason: string }[] } {
  const chunks: CollectedChunk[] = [];
  const skipped: { source: string; reason: string }[] = [];
  for (const file of expandFileEntries(entries, cwd)) {
    const relative = path.relative(cwd, file);
    const shown = relative && !relative.startsWith("..") && !path.isAbsolute(relative) ? relative : file;
    let bytes: Buffer;
    try {
      bytes = readAllowedFile(file, MAX_TEXT_FILE_BYTES);
    } catch (error) {
      skipped.push({ source: shown, reason: error instanceof Error ? error.message : String(error) });
      continue;
    }
    if (bytes.includes(0)) {
      skipped.push({ source: shown, reason: "binary file" });
      continue;
    }
    const text = bytes.toString("utf8");
    if (!text.trim()) continue;
    for (const chunk of chunkLines(text, chunkBytes)) {
      if (!chunk.text.trim()) continue;
      chunks.push({ label: `${shown}:L${chunk.lines[0]}-L${chunk.lines[1]}`, source: shown, lines: chunk.lines, text: chunk.text });
      if (chunks.length > MAX_SWEEP_CHUNKS) {
        throw new Error(`more than ${MAX_SWEEP_CHUNKS} chunks; split the sweep by directory or raise chunkBytes`);
      }
    }
  }
  return { chunks, skipped };
}

const BLOCK_END = /<\/(p|div|section|article|li|ul|ol|h[1-6]|tr|table|pre|blockquote|header|footer|main|aside|dd|dt)>/gi;
const ENTITIES: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: "\"", apos: "'", nbsp: " ", "#39": "'" };

/** Readable text of an HTML page: no scripts, styles or markup; blocks become lines. */
export function htmlToText(html: string): { title?: string; text: string } {
  const title = /<title[^>]*>([\s\S]*?)<\/title>/i.exec(html)?.[1];
  const text = html
    .replace(/<!--[\s\S]*?-->/g, "")
    // Site chrome (menus, sidebars, footers) repeats on every page and says nothing about it.
    .replace(/<(script|style|noscript|svg|template|iframe|head|nav|aside|footer)\b[\s\S]*?<\/\1>/gi, "")
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<li[^>]*>/gi, "\n- ")
    .replace(BLOCK_END, "\n")
    .replace(/<[^>]+>/g, " ")
    .replace(/&(#x[0-9a-f]+|#\d+|[a-z0-9]+);/gi, (match, name: string) => {
      if (name.startsWith("#x")) return String.fromCodePoint(Number.parseInt(name.slice(2), 16));
      if (name.startsWith("#") && name !== "#39") return String.fromCodePoint(Number.parseInt(name.slice(1), 10));
      return ENTITIES[name.toLowerCase()] ?? match;
    })
    .split("\n")
    .map((line) => line.replace(/[ \t\f\v\u00a0]+/g, " ").trim())
    .map((line) => (/^-\s*$/.test(line) ? "" : line))
    .filter((line, index, lines) => line !== "" || (index > 0 && lines[index - 1] !== ""))
    .join("\n")
    .trim();
  return { title: title?.replace(/\s+/g, " ").trim(), text };
}

/** Absolute http(s) links of a page, without fragments, in document order and unique. */
export function pageLinks(html: string, base: string): string[] {
  const seen = new Set<string>();
  for (const match of html.matchAll(/<a\b[^>]*\bhref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))/gi)) {
    const href = match[1] ?? match[2] ?? match[3] ?? "";
    try {
      const url = new URL(href, base);
      if (url.protocol !== "http:" && url.protocol !== "https:") continue;
      url.hash = "";
      seen.add(url.toString());
    } catch {
      // Not a URL; skip it.
    }
  }
  return [...seen];
}

/** Disallowed path prefixes for `*` (and FreeLlama) in a robots.txt. */
export function robotsDisallow(robots: string): string[] {
  const rules: string[] = [];
  let applies = false;
  let inAgents = false;
  for (const raw of robots.split("\n")) {
    const line = raw.replace(/#.*$/, "").trim();
    const [key, ...rest] = line.split(":");
    const field = key?.trim().toLowerCase();
    const value = rest.join(":").trim();
    if (field === "user-agent") {
      if (!inAgents) applies = false;
      inAgents = true;
      if (value === "*" || /freellama/i.test(value)) applies = true;
    } else if (field) {
      inAgents = false;
      if (applies && field === "disallow" && value) rules.push(value);
    }
  }
  return rules;
}

const PRIVATE_V4 = [/^10\./, /^127\./, /^169\.254\./, /^192\.168\./, /^172\.(1[6-9]|2\d|3[01])\./, /^0\./, /^100\.(6[4-9]|[7-9]\d|1[01]\d|12[0-7])\./];

function privateAddress(address: string): boolean {
  if (isIP(address) === 4) return PRIVATE_V4.some((pattern) => pattern.test(address));
  const lower = address.toLowerCase();
  return lower === "::1" || lower === "::" || lower.startsWith("fe80:") || lower.startsWith("fc") || lower.startsWith("fd")
    || (lower.startsWith("::ffff:") && privateAddress(lower.slice(7)));
}

/**
 * Crawls stay on the public internet: a page on loopback or a private network (this machine's own
 * services, a router) is refused even when a public page links to it.
 */
export async function assertPublicHost(url: URL, resolve: typeof lookup = lookup): Promise<void> {
  const host = url.hostname.replace(/^\[|\]$/g, "");
  if (host === "localhost" || host.endsWith(".localhost") || host.endsWith(".local") || host.endsWith(".internal")) {
    throw new Error(`${url.host} is a local host; sweeps crawl public sites only`);
  }
  const addresses = isIP(host) ? [{ address: host }] : await resolve(host, { all: true });
  if (addresses.some(({ address }) => privateAddress(address))) {
    throw new Error(`${url.host} resolves to a private address; sweeps crawl public sites only`);
  }
}

type Fetcher = (url: string, init: RequestInit) => Promise<Response>;

export type CrawlOptions = {
  sameOrigin: boolean;
  maxPages: number;
  maxDepth: number;
  include?: string;
  chunkBytes: number;
  signal?: AbortSignal;
  fetcher?: Fetcher;
  resolve?: typeof lookup;
  delayMs?: number;
  onPage?: (url: string, chunks: CollectedChunk[]) => void;
};

/**
 * Breadth-first crawl from `urls`: GET only, robots.txt honored, one request at a time per crawl
 * with a pause between them, text pages only. Each page becomes chunks labelled `url#n`.
 */
export async function crawl(urls: string[], options: CrawlOptions): Promise<{ pages: number; skipped: { source: string; reason: string }[] }> {
  const fetcher = options.fetcher ?? ((url, init) => fetch(url, init));
  const delay = options.delayMs ?? CRAWL_DELAY_MS;
  const include = options.include ? new RegExp(options.include) : undefined;
  const origins = new Set(urls.map((url) => new URL(url).origin));
  const robots = new Map<string, string[]>();
  const queue = urls.map((url) => ({ url: new URL(url).toString(), depth: 0 }));
  const seen = new Set(queue.map((item) => item.url));
  const skipped: { source: string; reason: string }[] = [];
  let pages = 0;
  let total = 0;
  // Redirects are followed by hand so every hop passes the public-host check.
  const get = async (start: string): Promise<{ response: Response; url: string }> => {
    let url = start;
    for (let hop = 0; hop <= 5; hop++) {
      await assertPublicHost(new URL(url), options.resolve);
      const response = await fetcher(url, {
        headers: { "user-agent": USER_AGENT, accept: "text/html,text/plain,text/markdown,application/json;q=0.8" },
        redirect: "manual",
        signal: options.signal ? AbortSignal.any([options.signal, AbortSignal.timeout(FETCH_TIMEOUT_MS)]) : AbortSignal.timeout(FETCH_TIMEOUT_MS),
      });
      const location = response.headers.get("location");
      if (response.status < 300 || response.status >= 400 || !location) return { response, url };
      url = new URL(location, url).toString();
    }
    throw new Error("more than 5 redirects");
  };
  while (queue.length > 0 && pages < options.maxPages) {
    if (options.signal?.aborted) break;
    const { url, depth } = queue.shift()!;
    const parsed = new URL(url);
    try {
      if (!robots.has(parsed.origin)) {
        let rules: string[] = [];
        try {
          const { response } = await get(`${parsed.origin}/robots.txt`);
          if (response.ok) rules = robotsDisallow(await response.text());
        } catch {
          // No robots.txt reachable: nothing disallowed.
        }
        robots.set(parsed.origin, rules);
      }
      if (robots.get(parsed.origin)!.some((rule) => parsed.pathname.startsWith(rule))) {
        skipped.push({ source: url, reason: "disallowed by robots.txt" });
        continue;
      }
      if (pages > 0) await new Promise((resolve) => setTimeout(resolve, delay));
      const { response, url: final } = await get(url);
      if (!response.ok) {
        skipped.push({ source: url, reason: `HTTP ${response.status}` });
        continue;
      }
      const type = response.headers.get("content-type") ?? "";
      if (!/text\/|json|xml/.test(type)) {
        skipped.push({ source: url, reason: `not text (${type || "unknown type"})` });
        continue;
      }
      const length = Number(response.headers.get("content-length") ?? 0);
      if (length > MAX_PAGE_BYTES) {
        skipped.push({ source: url, reason: `page is ${length} bytes; the limit is ${MAX_PAGE_BYTES}` });
        continue;
      }
      const body = (await response.text()).slice(0, MAX_PAGE_BYTES);
      const html = /html/.test(type);
      const { title, text } = html ? htmlToText(body) : { title: undefined, text: body };
      pages += 1;
      const pageText = text.slice(0, MAX_PAGE_TEXT);
      const chunks = chunkLines(pageText, options.chunkBytes)
        .filter((chunk) => chunk.text.trim())
        .map((chunk, index) => ({
          label: `${final}#${index + 1}`,
          source: final,
          lines: chunk.lines,
          text: `${title ? `Page: ${title}\n` : ""}URL: ${final}\n\n${chunk.text}`,
        }));
      total += chunks.length;
      if (total > MAX_SWEEP_CHUNKS) throw new Error(`more than ${MAX_SWEEP_CHUNKS} chunks; lower maxPages or raise chunkBytes`);
      options.onPage?.(final, chunks);
      if (html && depth < options.maxDepth) {
        for (const link of pageLinks(body, final)) {
          if (seen.has(link)) continue;
          if (options.sameOrigin && !origins.has(new URL(link).origin)) continue;
          if (include && !include.test(link)) continue;
          seen.add(link);
          queue.push({ url: link, depth: depth + 1 });
        }
      }
    } catch (error) {
      if (error instanceof Error && /more than \d+ chunks/.test(error.message)) throw error;
      skipped.push({ source: url, reason: error instanceof Error ? error.message : String(error) });
    }
  }
  return { pages, skipped };
}
