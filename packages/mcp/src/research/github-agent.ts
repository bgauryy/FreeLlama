// GitHub research adapter: a local Ollama model whose only tools are Octocode's read-only GitHub
// tools. Run as `node github-agent.js` with the same environment as `octocode-agent`.
import { runAdapter } from "./agent-loop.js";
import { GITHUB_ADAPTER } from "./octocode-agent.js";
import { isEntry } from "./entry.js";

if (isEntry(import.meta.url, "github-agent")) {
  // Remote files are not on local disk: a cited path counts when the run's own gh results showed it.
  process.exitCode = await runAdapter({ ...GITHUB_ADAPTER, grounding: "observed" });
}
