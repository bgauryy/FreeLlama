// The compiled Rust core (napi addon) and the server version, isolated so the CJS/ESM interop
// dance lives in one place.
import { createRequire } from "node:module";

// Default (CJS) import — the native binding module doesn't declare static named exports
// (`index.js` re-exports a `require()`'d `.node` file), so a named `import { doctor } from ...`
// is not reliably detectable by Node's CJS/ESM interop. Destructuring after a default import
// sidesteps that entirely.
import native from "../native/index.js";
export const { doctor, machine, listModels, route, runTask } = native as {
  doctor: (endpoint?: string | null) => Promise<string>;
  machine: (endpoint?: string | null) => Promise<string>;
  listModels: (endpoint?: string | null) => Promise<string>;
  route: (
    endpoint: string | null | undefined,
    task: string,
    objective?: string | null,
    model?: string | null,
    sessionId?: string | null,
    contextTokens?: number | null,
    requiredCapabilities?: string[] | null,
    minConfidence?: string | null,
  ) => Promise<string>;
  runTask: (
    endpoint: string | null | undefined,
    task: string,
    objective?: string | null,
    model?: string | null,
    sessionId?: string | null,
    contextTokens?: number | null,
    requiredCapabilities?: string[] | null,
    prompt?: string | null,
    images?: string[] | null,
    messages?: unknown | null,
    input?: unknown | null,
    tools?: unknown | null,
    keepAlive?: string | null,
    minConfidence?: string | null,
  ) => Promise<string>;
};

// Single source of truth for the version — a hardcoded literal here silently drifts from the
// package it ships in (it already had: this file said 0.1.0 while the crate it wraps was 0.2.0).
// package.json is always present in an npm tarball regardless of the `files` allowlist, and
// `../package.json` resolves to the package root from `dist/index.js`.
export const { version: SERVER_VERSION } = createRequire(import.meta.url)("../package.json") as {
  version: string;
};
