// Which adapter a `node <file>` process should run. Each adapter is bundled into its own file, and
// a bundle also contains the modules it imports: `github-agent.js` includes octocode-agent.ts, whose
// `import.meta.url` is then the bundle's own URL too. Matching the script's name as well keeps an
// imported adapter's entry block from also running.
import path from "node:path";
import { pathToFileURL } from "node:url";

export function isEntry(moduleUrl: string, name: string, script: string | undefined = process.argv[1]): boolean {
  if (!script || moduleUrl !== pathToFileURL(script).href) return false;
  return path.basename(script).replace(/\.[cm]?[jt]s$/, "") === name;
}
