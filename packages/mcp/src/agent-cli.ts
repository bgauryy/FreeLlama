#!/usr/bin/env node
// `fllama` entry: see agent-cli/main.ts. Exits once stdout and stderr are flushed (piped stderr is
// asynchronous on macOS, and check: lines must not be lost), so a lingering keep-alive socket or
// the unref'd serve child never holds a one-shot command open.
import { main } from "./agent-cli/main.js";

const code = await main(process.argv.slice(2));
const flushed = (stream: NodeJS.WriteStream) => new Promise<void>((resolve) => stream.write("", () => resolve()));
await Promise.all([flushed(process.stdout), flushed(process.stderr)]);
process.exit(code);
