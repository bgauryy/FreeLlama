// Shared MCP client plumbing for the integration/e2e tiers: spawn the built server over stdio
// and hand back a connected SDK client. Paths resolve from this file, never the working
// directory, so the suites run identically from the repo root and from packages/mcp.
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { type ChildProcess, spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { createServer } from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));

export const SERVER_ENTRY = path.resolve(HERE, "..", "..", "dist", "index.js");
export const SERVE_ENDPOINT = process.env.FREELLAMA_SERVE_ENDPOINT ?? "http://127.0.0.1:11435";

export const REPO_ROOT = (() => {
  let dir = HERE;
  for (let depth = 0; depth < 10; depth += 1) {
    if (existsSync(path.join(dir, "Cargo.toml"))) return dir;
    dir = path.dirname(dir);
  }
  throw new Error("could not find the repo root (no Cargo.toml above test/setup)");
})();
export const RELEASE_BINARY = path.join(REPO_ROOT, "target", "release", "freellama");
export const releaseServeAvailable = existsSync(RELEASE_BINARY);

export function serveAuthHeaders(): Record<string, string> | undefined {
  const tokenFile = process.env.FREELLAMA_AUTH_TOKEN_FILE;
  return tokenFile
    ? { authorization: `Bearer ${readFileSync(tokenFile, "utf8").trim()}` }
    : undefined;
}

export async function connectClient(env?: Record<string, string>): Promise<Client> {
  const transport = new StdioClientTransport({
    command: "node",
    args: [SERVER_ENTRY],
    // The SDK's implicit child environment is intentionally allowlisted and drops product-specific
    // variables. Always pass the test process environment explicitly so endpoint/config overrides
    // used by isolated live tests actually reach the MCP server child.
    env: { ...process.env, ...env } as Record<string, string>,
  });
  const client = new Client({ name: "vitest-client", version: "0.0.1" });
  await client.connect(transport);
  return client;
}

/** True when a `freellama serve` instance is answering on the selected control-plane endpoint. */
export async function serveIsUp(endpoint = SERVE_ENDPOINT): Promise<boolean> {
  try {
    const response = await fetch(`${endpoint}/_freellama/v1/health`, {
      headers: serveAuthHeaders(),
      signal: AbortSignal.timeout(2000),
    });
    return response.ok;
  } catch {
    return false;
  }
}

export type IsolatedServe = { endpoint: string; child: ChildProcess };

/** Start the current release binary away from any stale developer service already on :11435. */
export async function startIsolatedServe(): Promise<IsolatedServe> {
  if (!releaseServeAvailable) throw new Error(`release binary not found at ${RELEASE_BINARY}`);
  const listener = createServer();
  await new Promise<void>((resolve, reject) => {
    listener.once("error", reject);
    listener.listen(0, "127.0.0.1", resolve);
  });
  const address = listener.address();
  if (address === null || typeof address === "string") throw new Error("could not reserve an E2E port");
  await new Promise<void>((resolve, reject) =>
    listener.close((error) => (error ? reject(error) : resolve())),
  );

  const endpoint = `http://127.0.0.1:${address.port}`;
  let stderr = "";
  const child = spawn(
    RELEASE_BINARY,
    [
      "serve",
      "--listen",
      `127.0.0.1:${address.port}`,
      "--recommendation-catalog",
      "recommendations.example.toml",
      "--ephemeral-feedback",
    ],
    { cwd: REPO_ROOT, stdio: ["ignore", "ignore", "pipe"] },
  );
  const spawnErrors: Error[] = [];
  child.once("error", (error) => {
    spawnErrors.push(error);
  });
  child.stderr?.on("data", (chunk) => {
    stderr = `${stderr}${String(chunk)}`.slice(-4000);
  });
  const deadline = Date.now() + 15_000;
  while (!(await serveIsUp(endpoint))) {
    if (spawnErrors.length > 0) {
      throw new Error(`could not start isolated freellama serve: ${spawnErrors[0]?.message}`);
    }
    if (child.exitCode !== null) {
      throw new Error(`isolated freellama serve exited ${child.exitCode}: ${stderr}`);
    }
    if (Date.now() > deadline) {
      child.kill();
      throw new Error(`isolated freellama serve did not come up at ${endpoint}: ${stderr}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  return { endpoint, child };
}
