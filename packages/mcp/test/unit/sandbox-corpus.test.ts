// Frozen differential corpus for the read-only shell sandbox. Every case was recorded from the
// CPython `shell_sandbox` the TypeScript port replaced (see the fixture's `generator`); each
// verdict, normalized command and token stream must stay identical, and each refusal message too
// apart from the listed MESSAGE_CHANGES.
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { ValidationError, isSystemError } from "../../src/research/errors.js";
import { GlobPatternError } from "../../src/research/glob.js";
import { assertCommandConfined, segments, validateCommand } from "../../src/research/sandbox.js";
import { shlexTokens } from "../../src/research/shlex.js";
import { SymlinkLoopError } from "../../src/research/workspace-path.js";

type Outcome = { ok?: unknown; error_type?: string; error?: string };
interface CorpusCase {
  command: string;
  compare: "exact" | "verdict";
  validate: Outcome;
  confined: Outcome;
  segments: Outcome;
  posix_tokens: Outcome;
  plain_tokens: Outcome;
  split: Outcome;
}
interface Corpus {
  generator: string;
  layout: { files: Record<string, string>; outside_files: Record<string, string>; symlinks: Record<string, string> };
  cases: CorpusCase[];
}

const corpus = JSON.parse(
  readFileSync(fileURLToPath(new URL("../fixtures/shell-sandbox-corpus.json", import.meta.url)), "utf8"),
) as Corpus;

let root = "";
let out = "";
let replacements: Array<[string, string]> = [];

beforeAll(() => {
  root = mkdtempSync(path.join(tmpdir(), "freellama-corpus-root-"));
  out = mkdtempSync(path.join(tmpdir(), "freellama-corpus-out-"));
  const write = (base: string, name: string, text: string) => {
    mkdirSync(path.dirname(path.join(base, name)), { recursive: true });
    writeFileSync(path.join(base, name), text);
  };
  for (const [name, text] of Object.entries(corpus.layout.files)) write(root, name, text);
  for (const [name, text] of Object.entries(corpus.layout.outside_files)) write(out, name, text);
  for (const [name, target] of Object.entries(corpus.layout.symlinks)) {
    symlinkSync(target.replaceAll("@OUT@", out), path.join(root, name));
  }
  const pairs = new Map<string, string>([
    [root, "@ROOT@"], [out, "@OUT@"],
    [path.resolve(root), "@ROOT@"], [path.resolve(out), "@OUT@"],
  ]);
  // macOS temp dirs live behind /var -> /private/var.
  pairs.set(realpathSync(root), "@ROOT@");
  pairs.set(realpathSync(out), "@OUT@");
  replacements = [...pairs.entries()].sort((a, b) => b[0].length - a[0].length);
});

afterAll(() => {
  rmSync(root, { recursive: true, force: true });
  rmSync(out, { recursive: true, force: true });
});

function normalize(value: unknown): unknown {
  if (typeof value === "string") {
    let text = value;
    for (const [raw, placeholder] of replacements) text = text.replaceAll(raw, placeholder);
    return text;
  }
  if (Array.isArray(value)) return value.map(normalize);
  return value;
}

/** The corpus `error_type` recorded for each refusal class. */
function errorType(error: unknown): string {
  if (error instanceof GlobPatternError) return "error";
  if (error instanceof SymlinkLoopError) return "RuntimeError";
  if (error instanceof ValidationError) return "ValueError";
  if (isSystemError(error)) return "OSError";
  throw new Error(`unclassified sandbox error: ${String(error)}`, { cause: error });
}

function outcome(fn: () => unknown): Outcome {
  try {
    const value = fn();
    return { ok: value === undefined ? null : normalize(value) };
  } catch (error) {
    return { error_type: errorType(error), error: normalize((error as Error).message) as string };
  }
}

// Refusal messages the corpus recorded with Python repr or OSError formatting, mapped to the text the
// TypeScript sandbox reports. Verdicts and error types are unchanged; only these messages differ.
const MESSAGE_CHANGES: Array<[RegExp, string]> = [
  // repr escaped invisible characters in a tool name; the message now quotes the name verbatim.
  [/^command blocked: 'cat\\xa0readme\.md'/, "command blocked: 'cat\u00a0readme.md'"],
  [/^command blocked: 'cat\\u3000readme\.md'/, "command blocked: 'cat\u3000readme.md'"],
  [/^command blocked: 'cat\\x1creadme\.md'/, "command blocked: 'cat\u001creadme.md'"],
  // repr of a missing subcommand (`None`).
  [/^command blocked: `git None` is not read-only/, "command blocked: `git` needs a read-only subcommand"],
  // Python's regex compiler error position became the glob pattern.
  [/^bad character range z-a at position 5$/, 'bad character range z-a in glob pattern "[z-a]x"'],
  // pathlib's RuntimeError text and OSError `[Errno N] strerror: 'path'` became Node's forms.
  [/^Symlink loop from '(.*)'$/, 'symlink loop at "$1"'],
  [/^\[Errno N\] File name too long: '(.*)'$/, "ENAMETOOLONG: name too long, stat '$1'"],
];
const changedMessage = (result: Outcome): Outcome => {
  if (!result.error) return result;
  const rule = MESSAGE_CHANGES.find(([pattern]) => pattern.test(result.error!));
  return rule ? { ...result, error: result.error.replace(rule[0], rule[1]) } : result;
};

// errno values differ by OS (ENAMETOOLONG is 63 on macOS, 36 on Linux).
const maskErrno = (result: Outcome): Outcome => (
  result.error ? { ...result, error: result.error.replace(/^\[Errno \d+\]/, "[Errno N]") } : result
);
const verdictOnly = (result: Outcome): Outcome => (
  "ok" in result ? { ok: "<allowed>" } : { error_type: result.error_type }
);

const concrete = (command: string) => command.replaceAll("@ROOT@", root).replaceAll("@OUT@", out);

describe("shell sandbox differential corpus", () => {
  it("covers the confinement tests plus at least 150 adversarial and benign commands", () => {
    expect(corpus.generator).toMatch(/^CPython /);
    expect(corpus.cases.length).toBeGreaterThanOrEqual(78 + 150);
    expect(new Set(corpus.cases.map((item) => item.command)).size).toBe(corpus.cases.length);
  });

  it.each(corpus.cases.map((item, index) => [index, JSON.stringify(item.command).slice(0, 80), item] as const))(
    "case %i %s",
    (_index, _label, item) => {
      const text = concrete(item.command);
      const actual = {
        validate: outcome(() => validateCommand(root, text)),
        confined: outcome(() => assertCommandConfined(root, text)),
        segments: outcome(() => segments(text)),
        posix_tokens: outcome(() => shlexTokens(text, { posix: true, punctuationChars: ";&|()<>", whitespaceSplit: true, commenters: "" })),
        plain_tokens: outcome(() => shlexTokens(text, { posix: false, whitespaceSplit: true, commenters: "" })),
        split: outcome(() => shlexTokens(text, { posix: true, whitespaceSplit: true, commenters: "" })),
      };
      const expected = {
        validate: item.validate, confined: item.confined, segments: item.segments,
        posix_tokens: item.posix_tokens, plain_tokens: item.plain_tokens, split: item.split,
      };
      const changed = Object.fromEntries(Object.entries(expected).map(([key, value]) => [key, changedMessage(maskErrno(value))]));
      const shape = item.compare === "verdict" ? verdictOnly : maskErrno;
      const project = (record: Record<string, Outcome>) => Object.fromEntries(
        Object.entries(record).map(([key, value]) => [key, key === "validate" || key === "confined" ? shape(value) : maskErrno(value)]),
      );
      expect(project(actual)).toEqual(project(changed));
    },
  );
});
