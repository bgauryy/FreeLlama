// Non-recursive shell-style globbing for command confinement. In each path component `*` matches
// any run of characters, `?` one character, and `[...]` one character of a class (`!` negates, a
// leading `]` is literal, `a-z` is a range). A name that begins with `.` matches only a pattern
// that begins with `.`. Directory components match only directories, following symlinks. Results
// are joined paths in directory order.
import { readdirSync } from "node:fs";
import { ValidationError } from "./errors.js";
import { isDir, lexists } from "./workspace-path.js";

/** A glob pattern that cannot be compiled. */
export class GlobPatternError extends ValidationError {
  override name = "GlobPatternError";
}

export const hasGlobMagic = (text: string) => /[*?[]/.test(text);
const isHidden = (name: string) => name.startsWith(".");

/** Paths that match `pattern`. */
export function globPaths(pattern: string): string[] {
  const split = pattern.lastIndexOf("/") + 1;
  const basename = pattern.slice(split);
  let dirname = pattern.slice(0, split);
  if (/[^/]/.test(dirname)) dirname = dirname.replace(/\/+$/, "");
  if (!hasGlobMagic(pattern)) return (basename ? lexists(pattern) : isDir(dirname)) ? [pattern] : [];
  if (!dirname) return matchNames("", basename);
  const directories = dirname !== pattern && hasGlobMagic(dirname) ? globDirectories(dirname) : [dirname];
  const results: string[] = [];
  for (const directory of directories) {
    const names = hasGlobMagic(basename)
      ? matchNames(directory, basename)
      : (basename ? lexists(join(directory, basename)) : isDir(directory)) ? [basename] : [];
    for (const name of names) results.push(join(directory, name));
  }
  return results;
}

function globDirectories(pattern: string): string[] {
  return globPaths(pattern).filter((match) => isDir(match));
}

function join(directory: string, name: string): string {
  if (name.startsWith("/")) return name;
  return !directory || directory.endsWith("/") ? directory + name : `${directory}/${name}`;
}

function matchNames(directory: string, pattern: string): string[] {
  const where = directory || ".";
  if (where.includes("\0")) throw new ValidationError("embedded null byte");
  let names: string[];
  try {
    names = readdirSync(where);
  } catch {
    names = [];
  }
  if (!isHidden(pattern)) names = names.filter((name) => !isHidden(name));
  const matches = compileGlob(pattern);
  return names.filter(matches);
}

type Item = { kind: "star" } | { kind: "any" } | { kind: "char"; char: number } | { kind: "class"; negate: boolean; ranges: Array<[number, number]> };

/** A matcher for one path component of a glob pattern. */
export function compileGlob(pattern: string): (name: string) => boolean {
  const chars = Array.from(pattern);
  const items: Item[] = [];
  for (let index = 0; index < chars.length;) {
    const char = chars[index++]!;
    if (char === "*") {
      if (items.at(-1)?.kind !== "star") items.push({ kind: "star" });
    } else if (char === "?") {
      items.push({ kind: "any" });
    } else if (char === "[") {
      const parsed = parseClass(chars, index, pattern);
      if (parsed) {
        items.push(parsed.item);
        index = parsed.end;
      } else {
        items.push({ kind: "char", char: char.codePointAt(0)! });
      }
    } else {
      items.push({ kind: "char", char: char.codePointAt(0)! });
    }
  }
  return (name) => matchItems(items, Array.from(name, (char) => char.codePointAt(0)!));
}

/**
 * The class that starts after `[` at `start`, or null when it has no closing `]` (then `[` is
 * literal). Every `-` between two characters is a range, except in a class that contains `--`:
 * there the range hyphens are chosen left to right, each skipping the character after its range
 * end, and every other `-` is literal.
 */
function parseClass(chars: string[], start: number, pattern: string): { item: Item; end: number } | null {
  let close = start;
  if (chars[close] === "!") close += 1;
  if (chars[close] === "]") close += 1;
  while (close < chars.length && chars[close] !== "]") close += 1;
  if (close >= chars.length) return null;
  const body = chars.slice(start, close);
  const negate = body[0] === "!";
  const first = negate ? 1 : 0;
  const tokens = body.map((char) => ({ char, operator: char === "-" }));
  if (body.join("").includes("--")) {
    for (const token of tokens) token.operator = false;
    for (let hyphen = body.indexOf("-", first + 1); hyphen >= 0; hyphen = body.indexOf("-", hyphen + 3)) {
      tokens[hyphen]!.operator = true;
    }
  }
  const ranges: Array<[number, number]> = [];
  const point = (char: string) => char.codePointAt(0)!;
  for (let index = first; index < tokens.length;) {
    const low = tokens[index++]!.char;
    if (!tokens[index]?.operator || index + 1 >= tokens.length) {
      ranges.push([point(low), point(low)]);
      continue;
    }
    const high = tokens[index + 1]!.char;
    index += 2;
    if (point(high) < point(low)) {
      throw new GlobPatternError(`bad character range ${low}-${high} in glob pattern ${JSON.stringify(pattern)}`);
    }
    ranges.push([point(low), point(high)]);
  }
  return { item: { kind: "class", negate, ranges }, end: close + 1 };
}

function matchOne(item: Item, char: number): boolean {
  if (item.kind === "any") return true;
  if (item.kind === "char") return item.char === char;
  if (item.kind === "class") return item.ranges.some(([low, high]) => char >= low && char <= high) !== item.negate;
  return false;
}

/** Match with single-star backtracking; every non-star item consumes exactly one character. */
function matchItems(items: Item[], name: number[]): boolean {
  let item = 0;
  let char = 0;
  let starItem = -1;
  let starChar = 0;
  while (char < name.length) {
    if (item < items.length && items[item]!.kind !== "star" && matchOne(items[item]!, name[char]!)) {
      item += 1;
      char += 1;
    } else if (item < items.length && items[item]!.kind === "star") {
      starItem = item;
      starChar = char;
      item += 1;
    } else if (starItem >= 0) {
      item = starItem + 1;
      starChar += 1;
      char = starChar;
    } else {
      return false;
    }
  }
  while (item < items.length && items[item]!.kind === "star") item += 1;
  return item === items.length;
}
