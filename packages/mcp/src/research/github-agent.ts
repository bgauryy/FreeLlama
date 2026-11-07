// GitHub research adapter: a local Ollama model whose only tools are Octocode's read-only GitHub
// tools. Run as `node github-agent.js` with the same environment as `octocode-agent`.
import { pathToFileURL } from "node:url";
import { runAdapter } from "./agent-loop.js";
import { GITHUB_ADAPTER } from "./octocode-agent.js";

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.exitCode = await runAdapter(GITHUB_ADAPTER);
}
