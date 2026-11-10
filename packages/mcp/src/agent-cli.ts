#!/usr/bin/env node
// `fllama` entry: see agent-cli/main.ts. Exits once stdout is flushed, so a lingering keep-alive
// socket or the unref'd serve child never holds a one-shot command open.
import { main } from "./agent-cli/main.js";

const code = await main(process.argv.slice(2));
process.stdout.write("", () => process.exit(code));
