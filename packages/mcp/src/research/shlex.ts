// Shell-style lexer with the state machine of Python's `shlex.shlex` (`read_token`), state for
// state. Command validation depends on its exact token boundaries.
import { ValidationError } from "./errors.js";

export interface ShlexOptions {
  posix: boolean;
  punctuationChars?: string;
  whitespaceSplit?: boolean;
  commenters?: string;
}

const ASCII_WORDCHARS = "abcdfeghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_";
const POSIX_WORDCHARS = "ßàáâãäåæçèéêëìíîïðñòóôõöøùúûüýþÿÀÁÂÃÄÅÆÇÈÉÊËÌÍÎÏÐÑÒÓÔÕÖØÙÚÛÜÝÞ";

/** Whether `char` is in `set`; the empty string (end of input) is in every set. */
function within(char: string, set: string): boolean {
  return set.includes(char);
}

/** Every token of `text`. */
export function shlexTokens(text: string, options: ShlexOptions): string[] {
  return [...shlexIter(text, options)];
}

/**
 * Tokens of `text`, produced lazily: a caller that refuses an early token throws before a later
 * lexing error is reached.
 */
export function* shlexIter(text: string, options: ShlexOptions): Generator<string, void, undefined> {
  const posix = options.posix;
  const punctuation = options.punctuationChars ?? "";
  const whitespaceSplit = options.whitespaceSplit ?? false;
  const commenters = options.commenters ?? "#";
  let wordchars = ASCII_WORDCHARS + (posix ? POSIX_WORDCHARS : "");
  if (punctuation) {
    wordchars += "~-./*?=";
    wordchars = [...wordchars].filter((char) => !punctuation.includes(char)).join("");
  }
  const whitespace = " \t\r\n";
  const quotes = "'\"";
  const escape = "\\";
  const escapedquotes = '"';
  const eof: string | null = posix ? null : "";

  const chars = Array.from(text);
  let cursor = 0;
  const pushbackChars: string[] = [];
  const pushback: string[] = [];
  let state: string | null = " ";
  let token = "";

  const readChar = (): string => {
    if (punctuation && pushbackChars.length > 0) return pushbackChars.pop()!;
    if (cursor < chars.length) return chars[cursor++]!;
    return "";
  };
  const skipLine = (): void => {
    while (cursor < chars.length) {
      const char = chars[cursor++]!;
      if (char === "\n") break;
    }
  };

  const readToken = (): string | null => {
    let quoted = false;
    let escapedstate = " ";
    for (;;) {
      const nextchar = readChar();
      if (state === null) {
        token = "";
        break;
      } else if (state === " ") {
        if (!nextchar) {
          state = null;
          break;
        } else if (within(nextchar, whitespace)) {
          if (token || (posix && quoted)) break;
          continue;
        } else if (within(nextchar, commenters)) {
          skipLine();
        } else if (posix && within(nextchar, escape)) {
          escapedstate = "a";
          state = nextchar;
        } else if (within(nextchar, wordchars)) {
          token = nextchar;
          state = "a";
        } else if (within(nextchar, punctuation)) {
          token = nextchar;
          state = "c";
        } else if (within(nextchar, quotes)) {
          if (!posix) token = nextchar;
          state = nextchar;
        } else if (whitespaceSplit) {
          token = nextchar;
          state = "a";
        } else {
          token = nextchar;
          if (token || (posix && quoted)) break;
          continue;
        }
      } else if (within(state, quotes)) {
        quoted = true;
        if (!nextchar) throw new ValidationError("No closing quotation");
        if (nextchar === state) {
          if (!posix) {
            token += nextchar;
            state = " ";
            break;
          }
          state = "a";
        } else if (posix && within(nextchar, escape) && within(state, escapedquotes)) {
          escapedstate = state;
          state = nextchar;
        } else {
          token += nextchar;
        }
      } else if (within(state, escape)) {
        if (!nextchar) throw new ValidationError("No escaped character");
        if (within(escapedstate, quotes) && nextchar !== state && nextchar !== escapedstate) {
          token += state;
        }
        token += nextchar;
        state = escapedstate;
      } else if (state === "a" || state === "c") {
        if (!nextchar) {
          state = null;
          break;
        } else if (within(nextchar, whitespace)) {
          state = " ";
          if (token || (posix && quoted)) break;
          continue;
        } else if (within(nextchar, commenters)) {
          skipLine();
          if (posix) {
            state = " ";
            if (token || (posix && quoted)) break;
            continue;
          }
        } else if (state === "c") {
          if (within(nextchar, punctuation)) {
            token += nextchar;
          } else {
            if (!within(nextchar, whitespace)) pushbackChars.push(nextchar);
            state = " ";
            break;
          }
        } else if (posix && within(nextchar, quotes)) {
          state = nextchar;
        } else if (posix && within(nextchar, escape)) {
          escapedstate = "a";
          state = nextchar;
        } else if (
          within(nextchar, wordchars) || within(nextchar, quotes)
          || (whitespaceSplit && !within(nextchar, punctuation))
        ) {
          token += nextchar;
        } else {
          if (punctuation) pushbackChars.push(nextchar);
          else pushback.unshift(nextchar);
          state = " ";
          if (token || (posix && quoted)) break;
          continue;
        }
      }
    }
    const result = token;
    token = "";
    if (posix && !quoted && result === "") return null;
    return result;
  };

  for (;;) {
    const next = pushback.length > 0 ? pushback.shift()! : readToken();
    if (next === eof) return;
    yield next as string;
  }
}
