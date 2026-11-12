// `files` on run_task: the server reads the paths an agent names, so the agent does not re-emit a
// log, a screenshot or a source file as its own output tokens. Each task gives files one role.
import { globSync, realpathSync, statSync, readFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import path from "node:path";

/** Largest file read as text; larger inputs are the caller's to cut where meaning allows. */
export const MAX_TEXT_FILE_BYTES = 4 * 1024 * 1024;
/** Largest image read for a vision or decision task. */
export const MAX_IMAGE_FILE_BYTES = 8 * 1024 * 1024;
/** Total bytes one call may read across its files. */
export const MAX_TOTAL_FILE_BYTES = 32 * 1024 * 1024;
/** Most files one call may name. */
export const MAX_FILES = 2_000;

const IMAGE_EXTENSIONS = new Set([".png", ".jpg", ".jpeg", ".webp", ".gif"]);
/** Decision models with audio input take WAV clips in `images` too (Ollama `/v1/systemone`). */
const AUDIO_EXTENSIONS = new Set([".wav"]);

type EmbeddingFileItem = string | { text?: string; image?: string; audio?: string };

export type TaskFileInput = {
  task?: string;
  /** Embedding: prepended to each file chunk, e.g. nomic's `search_document: `. */
  documentPrefix?: string;
  prompt?: string;
  images?: string[];
  /** Text items, or `{image}`/`{audio}` maps (base64) for a multimodal embedding model. */
  input?: EmbeddingFileItem | EmbeddingFileItem[];
  state?: unknown;
};

/** What `files` added, without the content: the receipt an agent sees. An embedding file split into
 * chunks has one receipt per chunk, with its 1-based line range. */
export type FileReceipt = {
  path: string;
  bytes: number;
  role: "prompt" | "image" | "audio" | "input" | "state";
  lines?: [number, number];
};

/**
 * Largest embedding chunk. Ollama embeds an item whole and fails one longer than the model's
 * window; 1.5 KB of code stays near 450 tokens, inside the 2,048 of common embedding models, and a
 * small chunk ranks the function a question is about rather than the file around it (measured: the
 * decision request builder ranked first at 1.5 KB, its surrounding 100 lines at 4 KB).
 */
export const EMBEDDING_CHUNK_BYTES = 1_536;

/** Lines a chunk repeats from the end of the one before, so a match is never cut at a boundary. */
export const CHUNK_OVERLAP_LINES = 8;

/** Split text at line boundaries into chunks of at most `limit` bytes (a longer line is cut). */
export function chunkLines(text: string, limit = EMBEDDING_CHUNK_BYTES): Array<{ text: string; lines: [number, number] }> {
  const lines = text.split("\n");
  const chunks: Array<{ text: string; lines: [number, number] }> = [];
  let current: string[] = [];
  let bytes = 0;
  let start = 1;
  const flush = (end: number, overlap = 0) => {
    if (current.length > 0 && current.join("\n").trim()) chunks.push({ text: current.join("\n"), lines: [start, end] });
    // Carry the last lines into the next chunk while they stay well under the limit.
    const carried = overlap > 0 ? current.slice(-overlap) : [];
    const carriedBytes = carried.reduce((sum, line) => sum + Buffer.byteLength(line) + 1, 0);
    current = carriedBytes < limit / 4 ? carried : [];
    bytes = current === carried ? carriedBytes : 0;
    start = end + 1 - current.length;
  };
  lines.forEach((line, index) => {
    const size = Buffer.byteLength(line) + 1;
    if (bytes + size > limit && current.length > 0) flush(index, CHUNK_OVERLAP_LINES);
    if (size > limit) {
      // One very long line (minified code, a data blob): cut it by characters.
      for (let at = 0; at < line.length; at += Math.floor(limit / 2)) {
        chunks.push({ text: line.slice(at, at + Math.floor(limit / 2)), lines: [index + 1, index + 1] });
      }
      start = index + 2;
      return;
    }
    current.push(line);
    bytes += size;
  });
  flush(lines.length);
  return chunks;
}

/**
 * Directories `files` may read under: the server's working directory (the agent's project when a
 * client starts the server there), the OS temp directory (where screenshots and tool output land),
 * `FREELLAMA_FILE_ROOTS` (path-delimited), and the handoffs a task wrote with `saveAs`
 * (`${FREELLAMA_HOME}/handoffs`) and the answers written to `${FREELLAMA_HOME}/tmp`. Reading stays inside what the agent could already reach in its
 * project, so a file the host would not show the agent is not read here either.
 */
export function fileRoots(env: NodeJS.ProcessEnv = process.env, cwd = process.cwd(), home = homedir()): string[] {
  const configured = (env.FREELLAMA_FILE_ROOTS ?? "").split(path.delimiter).filter(Boolean);
  // A host that starts the server from `/` or the home directory would otherwise expose every
  // file there; such a working directory is not a project, so only configured roots apply.
  const broad = new Set([path.parse(path.resolve(cwd)).root, resolvedRoot(home)]);
  const project = broad.has(resolvedRoot(cwd)) ? [] : [cwd];
  // The same directory as handoffs.ts HANDOFF_DIR, derived from `env` so callers can pass their own.
  const handoffs = path.join(env.FREELLAMA_HOME || path.join(home, ".octocode", "freellama"), "handoffs");
  // And handoffs.ts OUTPUT_TMP_DIR: where shortened answers are written when no `outputPath` was given.
  const outputs = path.join(env.FREELLAMA_HOME || path.join(home, ".octocode", "freellama"), "tmp");
  return [...project, tmpdir(), ...configured, handoffs, outputs].map(resolvedRoot);
}

/**
 * A root as files resolve: through symlinks (macOS `/var` is `/private/var`). A root that does not
 * exist yet (the handoffs directory before the first saveAs) resolves through its nearest parent.
 */
function resolvedRoot(root: string): string {
  try {
    return realpathSync(root);
  } catch {
    const absolute = path.resolve(root);
    const parent = path.dirname(absolute);
    return parent === absolute ? absolute : path.join(resolvedRoot(parent), path.basename(absolute));
  }
}

export function within(file: string, root: string): boolean {
  const relative = path.relative(root, file);
  return relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative));
}

/** Header classification only; Ollama remains responsible for decoding the complete WAV. */
export function isWav(bytes: Buffer): boolean {
  return bytes.length >= 12 && bytes.subarray(0, 4).toString() === "RIFF" && bytes.subarray(8, 12).toString() === "WAVE";
}

type ReadFile = { path: string; bytes: Buffer; media?: "image" | "audio" };

/** Entries with `*`, `?` or `[` are globs, matched from the server's working directory, sorted. */
export function expandFileEntries(entries: string[], cwd = process.cwd()): string[] {
  const out: string[] = [];
  for (const entry of entries) {
    if (!/[*?[]/.test(entry)) {
      out.push(entry);
      continue;
    }
    const matches = globSync(entry, { cwd })
      .map((match) => path.resolve(cwd, match))
      .filter((match) => statSync(match).isFile())
      .sort();
    if (matches.length === 0) throw new Error(`no files match ${entry} (globs resolve from ${cwd})`);
    out.push(...matches);
    if (out.length > MAX_FILES) break;
  }
  return out;
}

function readFiles(entries: string[], givenRoots: string[]): ReadFile[] {
  const roots = givenRoots.map(resolvedRoot);
  const paths = expandFileEntries(entries);
  if (paths.length > MAX_FILES) throw new Error(`files names ${paths.length} paths; at most ${MAX_FILES} per call`);
  let total = 0;
  return paths.map((name) => {
    const absolute = path.resolve(name);
    let real: string;
    try {
      real = realpathSync(absolute);
    } catch {
      throw new Error(`file not found: ${name}`);
    }
    if (!roots.some((root) => within(real, root))) {
      throw new Error(
        `file ${name} is outside the readable roots (${roots.join(", ")}); copy it into the project or temp directory, or add its directory to FREELLAMA_FILE_ROOTS`,
      );
    }
    const stat = statSync(real);
    if (!stat.isFile()) throw new Error(`not a regular file: ${name}`);
    const extension = path.extname(real).toLowerCase();
    let media: "audio" | "image" | undefined = AUDIO_EXTENSIONS.has(extension) ? "audio" : IMAGE_EXTENSIONS.has(extension) ? "image" : undefined;
    const limit = media ? MAX_IMAGE_FILE_BYTES : MAX_TEXT_FILE_BYTES;
    if (stat.size > limit) {
      throw new Error(`file ${name} is ${stat.size} bytes; the limit is ${limit} for ${media ?? "text"}; split it or send the part that matters`);
    }
    total += stat.size;
    if (total > MAX_TOTAL_FILE_BYTES) throw new Error(`files total more than ${MAX_TOTAL_FILE_BYTES} bytes; split the call`);
    const bytes = readFileSync(real);
    if (media === "audio" && !isWav(bytes)) {
      throw new Error(`audio file ${name} must contain a WAV RIFF/WAVE header`);
    }
    if (isWav(bytes)) media = "audio";
    return { path: name, bytes, media };
  });
}

/** The project-relative path when the file is under the working directory. */
function shortPath(name: string): string {
  const relative = path.relative(process.cwd(), path.resolve(name));
  return relative && !relative.startsWith("..") && !path.isAbsolute(relative) ? relative : name;
}

/** The file's opening comment block (`//!`, `///`, `//`, `#`, `/*`, `*`, `"""`), at most 400 bytes. */
export function leadingComment(body: string): string {
  const lines: string[] = [];
  for (const line of body.split("\n").slice(0, 20)) {
    const trimmed = line.trim();
    if (!/^(\/\/|#(?!include)|\/\*|\*|"""|\s*$)/.test(trimmed) || trimmed.startsWith("#!")) break;
    if (trimmed) lines.push(trimmed.replace(/^(\/\/[!/]?|#+|\/\*+|\*\/?|""")\s?/, ""));
  }
  return lines.join(" ").slice(0, 400);
}

/** Largest audio file `audio` reads; Ollama itself takes up to 512 MB and 2 hours. */
export const MAX_AUDIO_FILE_BYTES = 256 * 1024 * 1024;

/** Read one file under the readable roots with its own size limit, for tools outside run_task. */
export function readAllowedFile(name: string, limit: number, roots: string[] = fileRoots()): Buffer {
  const allowed = roots.map(resolvedRoot);
  let real: string;
  try {
    real = realpathSync(path.resolve(name));
  } catch {
    throw new Error(`file not found: ${name}`);
  }
  if (!allowed.some((root) => within(real, root))) {
    throw new Error(`file ${name} is outside the readable roots (${allowed.join(", ")}); copy it into the project or temp directory, or add its directory to FREELLAMA_FILE_ROOTS`);
  }
  const stat = statSync(real);
  if (!stat.isFile()) throw new Error(`not a regular file: ${name}`);
  if (stat.size > limit) throw new Error(`file ${name} is ${stat.size} bytes; the limit is ${limit}`);
  return readFileSync(real);
}

function text(file: ReadFile): string {
  if (file.bytes.includes(0)) throw new Error(`file ${file.path} is binary; only images (${[...IMAGE_EXTENSIONS].join(", ")}) and text files can be read`);
  return file.bytes.toString("utf8");
}

/**
 * Fill a task's payload from `files`, by task:
 * - embedding: each text file is one more `input` item, after any `input` given;
 * - decision: one file is the state (JSON stays JSON); several make a state keyed by path;
 *   images join `images`;
 * - vision and the chat tasks: images join `images`; text files are appended to `prompt`,
 *   each under a `--- file: <path> ---` line.
 * Returns the filled payload and a receipt per file; content never enters the receipt.
 */
export function applyTaskFiles<T extends TaskFileInput>(
  payload: T,
  paths: string[] | undefined,
  roots: string[] = fileRoots(),
): { payload: T & TaskFileInput; files: FileReceipt[] } {
  if (paths === undefined || paths.length === 0) return { payload, files: [] };
  const read = readFiles(paths, roots);
  const receipts: FileReceipt[] = [];
  const media = read.filter((file) => file.media !== undefined);
  const texts = read.filter((file) => file.media === undefined);
  const next: T = { ...payload };
  const task = payload.task ?? "completion";
  if (task === "embedding") {
    const given = payload.input === undefined ? [] : Array.isArray(payload.input) ? payload.input : [payload.input];
    const prefix = payload.documentPrefix ?? "";
    const items: EmbeddingFileItem[] = [...given];
    // In the order named: text in line chunks; an image or WAV is one `{image}`/`{audio}` item,
    // which only a multimodal embedding model (such as embeddinggemma-2) accepts.
    for (const file of read) {
      if (file.media) {
        const encoded = file.bytes.toString("base64");
        items.push(file.media === "audio" ? { audio: encoded } : { image: encoded });
        receipts.push({ path: file.path, bytes: file.bytes.length, role: "input" });
        continue;
      }
      const body = text(file);
      // Each chunk carries its file's path and opening comment: code alone embeds poorly, and
      // the path and module doc are what a question about a file usually matches.
      const header = `${shortPath(file.path)}\n${leadingComment(body)}`.trimEnd();
      for (const chunk of chunkLines(body)) {
        items.push(`${prefix}${header}\n${chunk.text}`);
        receipts.push({ path: file.path, bytes: Buffer.byteLength(chunk.text), role: "input", lines: chunk.lines });
      }
    }
    next.input = items;
    return { payload: next, files: receipts };
  }
  const audio = media.find((file) => file.media === "audio");
  if (audio && task !== "decision") throw new Error(`audio file ${audio.path} is read only by tasks "decision" and "embedding" (models with audio input)`);
  if (media.length > 0) {
    // Ollama's decision transport calls the field images for both images and audio; preserve the
    // bytes and expose the actual modality in receipts so routing can require Audio for WAV.
    next.images = [...(payload.images ?? []), ...media.map((file) => file.bytes.toString("base64"))];
    for (const file of media) receipts.push({ path: file.path, bytes: file.bytes.length, role: file.media! });
  }
  if (task === "decision") {
    if (texts.length > 0 && payload.state !== undefined) throw new Error("give the decision state either as `state` or as text `files`, not both");
    if (texts.length === 1) {
      const body = text(texts[0]!);
      next.state = jsonOrText(body);
    } else if (texts.length > 1) {
      next.state = Object.fromEntries(texts.map((file) => [file.path, text(file)]));
    }
    for (const file of texts) receipts.push({ path: file.path, bytes: file.bytes.length, role: "state" });
    return { payload: next, files: receipts };
  }
  if (texts.length > 0) {
    if (payload.prompt === undefined) throw new Error("text `files` are appended to `prompt`; give the instructions in `prompt`");
    const sections = texts.map((file) => `--- file: ${file.path} ---\n${text(file)}`);
    next.prompt = [payload.prompt, ...sections].join("\n\n");
    for (const file of texts) receipts.push({ path: file.path, bytes: file.bytes.length, role: "prompt" });
  }
  return { payload: next, files: receipts };
}

function jsonOrText(body: string): unknown {
  try {
    const value: unknown = JSON.parse(body);
    if (value !== null && typeof value === "object") return value;
  } catch {
    // Text state.
  }
  return body;
}

/**
 * The label of embedding item `index`: `path:Lstart-Lend` for a file chunk, or `input[index]`,
 * followed by the start of the item's text when `texts` gives it, so a match is readable without
 * looking the index up (`input[2] "Uploading big PNG images makes…"`). `prefix` (an embedding
 * family's document prefix FreeLlama added) is left out of the snippet.
 */
export function inputLabels(inputCount: number, files: FileReceipt[], texts?: readonly unknown[], prefix?: string): string[] {
  // Project-relative where possible: a label is read once per match, so short paths save tokens.
  const fromFiles = files.filter((file) => file.role === "input")
    .map((file) => (file.lines ? `${shortPath(file.path)}:L${file.lines[0]}-${file.lines[1]}` : shortPath(file.path)));
  const given = inputCount - fromFiles.length;
  const snippet = (index: number) => {
    const raw = texts?.[index];
    if (typeof raw !== "string") return "";
    const text = (prefix && raw.startsWith(prefix) ? raw.slice(prefix.length) : raw).replace(/\s+/g, " ").trim();
    return text === "" ? "" : ` ${JSON.stringify(text.length > 48 ? `${text.slice(0, 47)}…` : text)}`;
  };
  return [...Array.from({ length: given }, (_, index) => `input[${index}]${snippet(index)}`), ...fromFiles];
}
