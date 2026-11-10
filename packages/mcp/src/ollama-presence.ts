// Whether Ollama is running, installed but stopped, or missing, so a command can say what to do
// at once instead of after serve's startup waits: install it (with the link), or start it.
import { existsSync } from "node:fs";
import path from "node:path";
import { DEFAULT_OLLAMA_ENDPOINT } from "./config.js";

export const OLLAMA_DOWNLOAD_URL = "https://ollama.com/download";

export const OLLAMA_MISSING = `Ollama is not installed. Please install Ollama: ${OLLAMA_DOWNLOAD_URL}`;
export const OLLAMA_STOPPED = "Ollama is installed but not running: open the Ollama app or run `ollama serve`, then retry.";

export type OllamaPresence = "running" | "stopped" | "missing";

/** Where an Ollama install puts its binary or app, besides PATH. */
function knownLocations(env: NodeJS.ProcessEnv, platform: NodeJS.Platform): string[] {
  if (platform === "darwin") return ["/Applications/Ollama.app", "/usr/local/bin/ollama", "/opt/homebrew/bin/ollama"];
  if (platform === "win32") return env.LOCALAPPDATA ? [path.join(env.LOCALAPPDATA, "Programs", "Ollama", "ollama.exe")] : [];
  return ["/usr/local/bin/ollama", "/usr/bin/ollama"];
}

/** Whether an `ollama` binary or app is on this machine (PATH, then the installers' default places). */
export function ollamaInstalled(env: NodeJS.ProcessEnv = process.env, platform: NodeJS.Platform = process.platform, exists: (file: string) => boolean = existsSync): boolean {
  const names = platform === "win32" ? ["ollama.exe", "ollama"] : ["ollama"];
  const dirs = (env.PATH ?? env.Path ?? "").split(path.delimiter).filter(Boolean);
  return dirs.some((dir) => names.some((name) => exists(path.join(dir, name)))) || knownLocations(env, platform).some(exists);
}

/** Running when the endpoint answers; otherwise stopped or missing by whether it is installed. */
export async function ollamaPresence(
  endpoint = DEFAULT_OLLAMA_ENDPOINT,
  options: { fetch?: typeof fetch; installed?: () => boolean; timeoutMs?: number } = {},
): Promise<OllamaPresence> {
  const get = options.fetch ?? fetch;
  try {
    await get(new URL("/api/version", endpoint), { signal: AbortSignal.timeout(options.timeoutMs ?? 1_500) });
    return "running";
  } catch {
    return (options.installed ?? (() => ollamaInstalled()))() ? "stopped" : "missing";
  }
}
