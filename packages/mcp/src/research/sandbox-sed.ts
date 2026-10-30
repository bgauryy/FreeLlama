// The bounded read-only sed grammar the research shell accepts: inline programs only, addresses,
// substitutions with harmless flags, and commands that cannot read, write or execute files.
import { ValidationError } from "./errors.js";
import { isSpace, trimSpace } from "./text.js";

// Unicode digits: decimal digits (Nd) plus the Numeric_Type=Digit characters (superscripts,
// subscripts, circled and dingbat digits, ...). The corpus pins this address digit set.
const DIGIT = /^[\p{Nd}\u{b2}-\u{b3}\u{b9}\u{1369}-\u{1371}\u{19da}\u{2070}\u{2074}-\u{2079}\u{2080}-\u{2089}\u{2460}-\u{2468}\u{2474}-\u{247c}\u{2488}-\u{2490}\u{24ea}\u{24f5}-\u{24fd}\u{24ff}\u{2776}-\u{277e}\u{2780}-\u{2788}\u{278a}-\u{2792}\u{10a40}-\u{10a43}\u{10e60}-\u{10e68}\u{11052}-\u{1105a}\u{1f100}-\u{1f10a}]$/u;
const isDigit = (char: string | undefined) => char !== undefined && DIGIT.test(char);
/** Whether `char` is one of `set`; the end of input is in every set. */
const within = (char: string | undefined, set: string) => set.includes(char ?? "");

/** Check every sed program in `args`. */
export function checkSed(args: string[]): void {
  for (const program of sedPrograms(args)) checkSedProgram(program);
}

/** Accept the bounded read-only sed grammar, including addressed substitutions. */
function checkSedProgram(text: string): void {
  const program = Array.from(text);
  const length = program.length;
  let index = 0;
  const delimited = (start: number): number => {
    const delimiter = program[start];
    let cursor = start + 1;
    while (cursor < length) {
      if (program[cursor] === "\\") cursor += 2;
      else if (program[cursor] === delimiter) return cursor + 1;
      else cursor += 1;
    }
    throw new ValidationError("command blocked: unterminated sed expression");
  };
  while (index < length) {
    while (index < length && (isSpace(program[index]) || within(program[index], ";{}"))) index += 1;
    if (index === length) return;
    // An address may be numeric, last-line, or a delimited regular expression.
    for (let address = 0; address < 2; address += 1) {
      if (index < length && isDigit(program[index])) {
        while (index < length && isDigit(program[index])) index += 1;
      } else if (index < length && program[index] === "$") {
        index += 1;
      } else if (index < length && program[index] === "/") {
        index = delimited(index);
      } else if (index + 1 < length && program[index] === "\\") {
        index = delimited(index + 1);
      } else {
        break;
      }
      while (index < length && isSpace(program[index])) index += 1;
      if (address === 0 && index < length && program[index] === ",") {
        index += 1;
        continue;
      }
      break;
    }
    while (index < length && (isSpace(program[index]) || program[index] === "!")) index += 1;
    if (index === length) throw new ValidationError("command blocked: sed command is missing");
    const command = program[index]!;
    index += 1;
    if (command === "s" && index < length) {
      index = delimited(index);
      // The replacement shares the first delimiter, so include it in the next scan.
      index = delimited(index - 1);
      const flagsStart = index;
      while (index < length && !within(program[index], ";}\n")) index += 1;
      const flags = Array.from(trimSpace(program.slice(flagsStart, index).join("")));
      if (flags.some((flag) => !within(flag, "gIp0123456789"))) {
        throw new ValidationError("command blocked: sed substitution flags can read, write or execute");
      }
    } else if (!within(command, "pPdDqQnNhHgGx={}")) {
      throw new ValidationError("command blocked: sed command can read, write or execute files");
    }
    if (!within(command, "{}") && index < length && !(isSpace(program[index]) || within(program[index], ";{}"))) {
      throw new ValidationError("command blocked: unsupported sed command argument");
    }
  }
}

/** Inspect every inline program; loading another file would hide its commands. */
function sedPrograms(args: string[]): string[] {
  const programs: string[] = [];
  let index = 0;
  while (index < args.length) {
    const arg = args[index]!;
    if (arg === "--") {
      programs.push(...args.slice(index + 1));
      break;
    }
    if (arg === "--file" || arg.startsWith("--file=")) {
      throw new ValidationError("command blocked: sed script files can read, write or execute");
    }
    if (arg === "--expression") {
      index += 1;
      if (index >= args.length) throw new ValidationError("command blocked: sed expression is missing");
      programs.push(args[index]!);
    } else if (arg.startsWith("--expression=")) {
      programs.push(arg.slice(arg.indexOf("=") + 1));
    } else if (arg.startsWith("-") && !arg.startsWith("--")) {
      // n/E/r are no-argument mode switches. e/f end a short-option cluster and consume the
      // remainder (or next argument), so attached programs stay visible.
      const short = Array.from(arg).slice(1);
      for (let offset = 0; offset < short.length; offset += 1) {
        const option = short[offset]!;
        if (option === "f") throw new ValidationError("command blocked: sed script files can read, write or execute");
        if (option === "e") {
          let program = short.slice(offset + 1).join("");
          if (!program) {
            index += 1;
            if (index >= args.length) throw new ValidationError("command blocked: sed expression is missing");
            program = args[index]!;
          }
          programs.push(program);
          break;
        }
        if (!within(option, "nEr")) throw new ValidationError("command blocked: unsupported sed option");
      }
    } else if (!arg.startsWith("--")) {
      if (programs.length === 0) programs.push(arg);
    } else {
      throw new ValidationError("command blocked: unsupported sed option");
    }
    index += 1;
  }
  return programs;
}
