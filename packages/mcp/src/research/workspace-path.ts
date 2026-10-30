// Path resolution for workspace confinement. `resolvePath` resolves each symlink and `..` component
// physically (a `..` after a symlink leaves the symlink's target, not the link's directory). A
// component that does not exist, or that cannot be read as a link, is kept verbatim, and a later
// `..` removes it. Confinement compares the resolved path with the resolved workspace root.
import { lstatSync, readlinkSync, statSync } from "node:fs";
import path from "node:path";
import { ValidationError } from "./errors.js";

/** A symlink chain that leads back to itself. */
export class SymlinkLoopError extends ValidationError {
  override name = "SymlinkLoopError";
}

const hasNul = (text: string) => text.includes("\0");

/** `text` with repeated slashes, trailing slashes and `.` components removed; `..` is kept. */
export function cleanPath(text: string): string {
  const parts = text.split("/").filter((part) => part !== "" && part !== ".");
  if (text.startsWith("/")) return `/${parts.join("/")}`;
  return parts.join("/") || ".";
}

/** `child` relative to `base`; an absolute `child` replaces `base`. */
export function joinPath(base: string, child: string): string {
  return cleanPath(child.startsWith("/") ? child : `${base}/${child}`);
}

/** Whether the resolved path `resolved` lies outside the resolved directory `root`. */
export function escapesRoot(resolved: string, root: string): boolean {
  const a = cleanPath(resolved);
  const b = cleanPath(root);
  return a !== b && !a.startsWith(b === "/" ? "/" : `${b}/`);
}

/** Missing paths and broken links do not exist; other stat failures are thrown. */
const IGNORED = new Set(["ENOENT", "ENOTDIR", "EBADF", "ELOOP"]);

function probe<T>(read: () => T, fallback: T): T {
  try {
    return read();
  } catch (error) {
    if (IGNORED.has((error as NodeJS.ErrnoException).code ?? "")) return fallback;
    throw error;
  }
}

/** Whether `target` exists, following symlinks. */
export function exists(target: string): boolean {
  return !hasNul(target) && probe(() => (statSync(target), true), false);
}

/** Whether `target` is a symlink. */
export function isSymlink(target: string): boolean {
  return !hasNul(target) && probe(() => lstatSync(target).isSymbolicLink(), false);
}

/** Whether `target` exists without following a final symlink; any failure counts as missing. */
export function lexists(target: string): boolean {
  if (hasNul(target)) return false;
  try {
    lstatSync(target);
    return true;
  } catch {
    return false;
  }
}

/** Whether `target` is a directory, following symlinks; any failure counts as not a directory. */
export function isDir(target: string): boolean {
  if (hasNul(target)) return false;
  try {
    return statSync(target).isDirectory();
  } catch {
    return false;
  }
}

function readlink(target: string): string | null {
  if (hasNul(target)) throw new ValidationError("embedded null byte");
  try {
    return readlinkSync(target);
  } catch {
    return null;
  }
}

/** The absolute physical form of `target`, which need not exist. */
export function resolvePath(target: string, cwd: string = process.cwd()): string {
  const seen = new Map<string, string | null>();
  const walk = (start: string, rest: string): string => {
    let current = rest.startsWith("/") ? "" : start;
    for (const name of rest.split("/")) {
      if (!name || name === ".") continue;
      if (name === "..") {
        current = current.slice(0, Math.max(current.lastIndexOf("/"), 0));
        continue;
      }
      const next = `${current}/${name}`;
      if (seen.has(next)) {
        const cached = seen.get(next);
        if (cached === null || cached === undefined) throw new SymlinkLoopError(`symlink loop at ${JSON.stringify(next)}`);
        current = cached;
        continue;
      }
      const link = readlink(next);
      if (link === null) {
        current = next;
      } else {
        seen.set(next, null);
        current = walk(current, link);
        seen.set(next, current);
      }
    }
    return current;
  };
  const text = cleanPath(target);
  return path.posix.normalize(walk(text.startsWith("/") ? "" : cwd, text) || "/");
}
