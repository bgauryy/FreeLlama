// Global setup for the integration and e2e tiers: rebuild dist/ (esbuild, ~10ms) so the tests
// always exercise current source, and fail fast with a readable message if Ollama is down —
// otherwise every test times out individually and the real cause is buried.
import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const PKG = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const OLLAMA = process.env.FREELLAMA_OLLAMA_ENDPOINT ?? "http://127.0.0.1:11434";

export default async function setup(): Promise<void> {
  execFileSync("node", [path.join(PKG, "scripts", "build.mjs")], { stdio: "inherit" });

  try {
    const response = await fetch(`${OLLAMA}/api/version`, { signal: AbortSignal.timeout(3000) });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
  } catch (error) {
    throw new Error(
      `Ollama is unreachable at ${OLLAMA} (${error instanceof Error ? error.message : error}). ` +
        "The integration/e2e tiers need a live Ollama — start it, or run only the unit tier (`yarn test`).",
    );
  }
}
