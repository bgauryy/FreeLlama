// Read-only command validation for the research shell (layer 1 of sandbox.ts). A command passes only
// when every pipeline segment runs an allowlisted read-only tool with no writing or executing flag,
// uses no hidden expansion, and names no path that resolves outside the workspace. The
// differential corpus in test/fixtures/shell-sandbox-corpus.json pins every verdict.
import { ValidationError } from "./errors.js";
import { globPaths, hasGlobMagic } from "./glob.js";
import { checkSed } from "./sandbox-sed.js";
import { SPACE_CLASS, trimSpace } from "./text.js";
import { shlexIter } from "./shlex.js";
import { cleanPath, escapesRoot, exists, isSymlink, joinPath, resolvePath } from "./workspace-path.js";

// Tools that only read. `awk` is absent on purpose: `system()`, `print >` and `cmd | getline` make
// any awk program a general executor, and mawk/BSD awk have no sandbox flag.
export const READ_ONLY_TOOLS = [
  "ls", "find", "grep", "egrep", "fgrep", "rg", "cat", "head", "tail", "wc", "sort", "uniq",
  "cut", "tr", "nl", "file", "stat", "du", "tree", "basename", "dirname", "realpath", "readlink",
  "sed", "jq", "diff", "cmp", "column", "xargs", "git", "tac", "fold", "paste", "comm", "od",
  "strings", "seq", "expr",
] as const;
export const SAFE_BUILTINS: ReadonlySet<string> = new Set(["echo", "printf", "true", "false", "test", "[", "pwd"]);
export const READ_ONLY_GIT: ReadonlySet<string> = new Set([
  "log", "show", "grep", "ls-files", "ls-tree", "diff", "blame", "status", "rev-parse",
  "cat-file", "shortlog", "describe", "rev-list", "name-rev", "annotate",
]);
const READ_ONLY_TOOL_SET: ReadonlySet<string> = new Set(READ_ONLY_TOOLS);

// Per-tool flags that write a file or run another program.
const FORBIDDEN_FLAGS: Readonly<Record<string, readonly string[]>> = {
  find: ["-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls"],
  sort: ["-o", "--output", "--compress-program", "--files0-from"],
  wc: ["--files0-from"],
  grep: ["-f", "--file"],
  egrep: ["-f", "--file"],
  fgrep: ["-f", "--file"],
  rg: ["--pre", "--pre-glob", "--search-zip", "-z", "-f", "--file"],
  tree: ["-o"],
  file: ["-C", "--compile", "-f", "--files-from"],
  jq: ["-f", "--from-file", "-L", "--library-path"],
  git: ["-C", "-c", "--config-env", "--exec-path", "--output", "-o", "--ext-diff", "--textconv", "--open-files-in-pager", "-O"],
  diff: ["--to-file"],
};
const SED_IN_PLACE = /^(?:-[a-zA-Z]*i|--in-place)/;

// Unicode whitespace and word characters.
const S = SPACE_CLASS;
const W = "\\p{L}\\p{N}_";
// Shell syntax that would hide a command or a path from validation.
const HIDDEN_EXPANSION = /\$\(|`|<\(|>\(|\$'|\$\{|\$[A-Za-z_0-9@*#?!$-]/;
const ALLOWED_STDERR_REDIRECT = new RegExp(`(?<![${W}>&])2>[${S}]*/dev/null|(?<![${W}>&])2>&1`, "gu");
const HOME_ESCAPE = new RegExp(`(?:^|[${S}="'])(?:~|\\$HOME(?![${W}])|\\$\\{HOME\\})`, "u");
const ABS_PATH = new RegExp(`(?:^|[${S}="'(:,])(/[^${S}"';|&)]*)`, "gu");
const DOTDOT_PATH = new RegExp(`(?:^|[${S}="'/])\\.\\.(?:/|[${S}"']|$)`, "u");
const FS_ABS_PREFIX = /^\/(?:etc|usr|home|Users|var|tmp|private|opt|root|System|Library|bin|sbin|dev|proc|sys|Applications|Volumes|mnt|media|srv|run|nix|snap)(?:\/|$)/;

/** The command with single-quoted spans blanked, since bash expands nothing inside them. */
function outsideSingleQuotes(command: string): string {
  const out: string[] = [];
  let quoted = false;
  let escaped = false;
  for (const char of command) {
    if (quoted) {
      quoted = char !== "'";
      out.push(" ");
      continue;
    }
    if (escaped) {
      escaped = false;
      out.push(" ");
      continue;
    }
    if (char === "\\") {
      escaped = true;
      out.push(" ");
      continue;
    }
    if (char === "'") {
      quoted = true;
      // Keep the quote after an unquoted `$` so `$'\057etc'` (ANSI-C quoting) stays visible.
      out.push(out.length > 0 && out[out.length - 1] === "$" ? "'" : " ");
      continue;
    }
    out.push(char);
  }
  return out.join("");
}

/** Reject command text that names a path outside the workspace. */
export function assertCommandConfined(root: string, commandText: string): void {
  if (HOME_ESCAPE.test(commandText)) {
    throw new ValidationError("command blocked: home-directory path is outside the workspace");
  }
  if (DOTDOT_PATH.test(commandText)) {
    throw new ValidationError("command blocked: '..' walks outside the workspace");
  }
  const resolvedRoot = resolvePath(root);
  for (const match of commandText.matchAll(ABS_PATH)) {
    const raw = match[1]!;
    let candidates: string[];
    if (hasGlobMagic(raw)) {
      // `/et?/hostname` exists only after expansion; judge every match, and treat a pattern that
      // matches nothing inside the workspace as an escape.
      const matches = globPaths(raw).map(cleanPath);
      if (matches.length === 0 && !raw.startsWith(resolvedRoot)) {
        throw new ValidationError(`command blocked: path pattern escapes workspace: ${raw}`);
      }
      candidates = matches;
    } else {
      const candidate = cleanPath(raw);
      if (FS_ABS_PREFIX.exec(raw) === null && !exists(candidate)) continue; // e.g. a URL path in a grep pattern
      candidates = [candidate];
    }
    for (const candidate of candidates) {
      if (escapesRoot(resolvePath(candidate), resolvedRoot)) {
        throw new ValidationError(`command blocked: path escapes workspace: ${raw}`);
      }
    }
  }
}

const OPERATOR_CHARS = new Set(";&|()<>");
const SEPARATORS = new Set(["|", "||", "&&", ";"]);

/** The pipeline segments of a command; an input redirect `<` stays in its segment as a word. */
export function segments(commandText: string): string[][] {
  const tokens = shlexIter(commandText, { posix: true, punctuationChars: ";&|()<>", whitespaceSplit: true, commenters: "" });
  const result: string[][] = [[]];
  for (const token of tokens) {
    if (SEPARATORS.has(token)) {
      result.push([]);
    } else if (token && [...token].every((char) => OPERATOR_CHARS.has(char))) {
      if (token === "<") {
        result[result.length - 1]!.push(token);
        continue;
      }
      throw new ValidationError(
        `command blocked: shell operator '${token}' is not allowed (read-only research: `
          + "no output redirection, background jobs or subshells)",
      );
    } else {
      result[result.length - 1]!.push(token);
    }
  }
  return result.filter((segment) => segment.length > 0);
}

/** Whether a short-option cluster such as `-rnL` contains `letters`. */
const inShortCluster = (arg: string, letters: string) => arg.startsWith("-") && !arg.startsWith("--") && arg.slice(1).includes(letters);

function checkTool(words: string[]): void {
  if (words.length === 0) return;
  const name = words[0]!;
  const args = words.slice(1);
  if (name.includes("/")) throw new ValidationError(`command blocked: run tools by name, not path: ${name}`);
  if (SAFE_BUILTINS.has(name)) return;
  if (!READ_ONLY_TOOL_SET.has(name)) {
    throw new ValidationError(
      `command blocked: '${name}' is not a read-only research tool (allowed: ${READ_ONLY_TOOLS.join(", ")})`,
    );
  }
  for (const flag of FORBIDDEN_FLAGS[name] ?? []) {
    if (args.some((arg) => arg === flag || arg.startsWith(`${flag}=`)
      || (flag.length === 2 && flag.startsWith("-") && inShortCluster(arg, flag[1]!)))) {
      throw new ValidationError(`command blocked: \`${name} ${flag}\` loads external arguments, writes or executes`);
    }
  }
  if (name === "sed") {
    for (const arg of args) {
      if (SED_IN_PLACE.test(arg)) throw new ValidationError("command blocked: sed -i edits files");
    }
    checkSed(args);
  }
  if (name === "git") {
    const allowed = [...READ_ONLY_GIT].sort().join(", ");
    const subcommand = args.find((arg) => !arg.startsWith("-"));
    if (subcommand === undefined) {
      throw new ValidationError(`command blocked: \`git\` needs a read-only subcommand (allowed: ${allowed})`);
    }
    if (!READ_ONLY_GIT.has(subcommand)) {
      throw new ValidationError(`command blocked: \`git ${subcommand}\` is not read-only (allowed: ${allowed})`);
    }
  }
  if (name === "xargs") {
    if (args.some((arg) => arg === "--arg-file" || arg.startsWith("--arg-file=") || (arg.startsWith("-a") && !arg.startsWith("--")))) {
      throw new ValidationError("command blocked: xargs argument files cannot be confined");
    }
    const target = xargsCommand(args);
    if (!["echo", "printf", "true", "false"].includes(target[0]!)) {
      throw new ValidationError("command blocked: xargs filesystem arguments cannot be confined");
    }
    checkTool(target);
  }
}

const XARGS_WITH_VALUE = new Set(["-n", "-I", "-d", "-P", "-L", "-s", "-E", "-a", "--arg-file", "--delimiter", "--max-args", "--max-procs"]);

function xargsCommand(args: string[]): string[] {
  let index = 0;
  while (index < args.length && args[index]!.startsWith("-")) index += XARGS_WITH_VALUE.has(args[index]!) ? 2 : 1;
  const command = args.slice(index);
  return command.length > 0 ? command : ["echo"];
}

const FOLLOW_FLAGS: Readonly<Record<string, string>> = { rg: "L", grep: "R", egrep: "R", fgrep: "R", find: "L", tree: "l", du: "L" };
const SHELL_CONSTRUCTS = new Set(["for", "while", "until", "if", "case", "function", "{", "eval", "exec", "source", "."]);

/** The command to run under restricted bash; throws a ValidationError naming the refusal. */
export function validateCommand(root: string, commandTextInput: string): string {
  if (!trimSpace(commandTextInput)) throw new ValidationError("empty shell command");
  if (trimSpace(commandTextInput).includes("\n")) throw new ValidationError("command blocked: send one command line per turn");
  // stderr is captured together with stdout already, so these two redirects are no-ops that models
  // write out of habit; rbash would refuse them as output redirection.
  const commandText = trimSpace(commandTextInput.replace(ALLOWED_STDERR_REDIRECT, " "));
  if (HIDDEN_EXPANSION.test(outsideSingleQuotes(commandText))) {
    throw new ValidationError(
      "command blocked: command substitution, process substitution and $-expansion are "
        + "not allowed; write paths and patterns literally",
    );
  }
  // Quoted program braces are literal; unquoted brace paths expand before execution.
  for (const token of shlexIter(commandText, { posix: false, whitespaceSplit: true, commenters: "" })) {
    if (token.startsWith("'") || token.startsWith('"') || token.startsWith("--exclude-dir=")) continue;
    if (token.includes("{") || token.includes("}")) {
      throw new ValidationError("command blocked: brace-expanded paths cannot be confined");
    }
  }
  const resolvedRoot = () => resolvePath(root);
  for (const segment of segments(commandText)) {
    for (let word of segment.slice(1)) {
      if (word.startsWith("--exclude-dir=")) continue;
      if (word === "<") continue;
      if (word.startsWith("-")) {
        if (!word.includes("=")) continue;
        word = word.slice(word.indexOf("=") + 1);
      }
      const candidate = joinPath(root, word);
      const candidates = hasGlobMagic(word) ? globPaths(candidate).map(cleanPath) : [candidate];
      for (const item of candidates) {
        if ((exists(item) || isSymlink(item)) && escapesRoot(resolvePath(item), resolvedRoot())) {
          throw new ValidationError("command blocked: relative path escapes workspace");
        }
      }
    }
    const tool = segment[0]!;
    const followFlags = Object.hasOwn(FOLLOW_FLAGS, tool) ? FOLLOW_FLAGS[tool] : undefined;
    if (followFlags && segment.slice(1).some((arg) => arg === "--follow" || arg === "--dereference"
      || arg === "--dereference-recursive" || inShortCluster(arg, followFlags))) {
      throw new ValidationError("command blocked: recursive symlink following cannot be confined");
    }
    if (tool.includes("=") && !tool.startsWith("=")) {
      throw new ValidationError("command blocked: variable assignment is not allowed");
    }
    if (SHELL_CONSTRUCTS.has(tool)) {
      throw new ValidationError(`command blocked: shell construct '${tool}' is not allowed; use one pipeline`);
    }
    checkTool(segment.filter((word) => word !== "<"));
  }
  assertCommandConfined(root, commandText);
  return commandText;
}
