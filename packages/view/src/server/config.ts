import { readFile } from "node:fs/promises";

export interface ViewConfig {
  endpoint: string;
  port: number;
  token?: string;
}

export function parseConfig(env: NodeJS.ProcessEnv): Omit<ViewConfig, "token"> {
  const endpoint = new URL(
    env.FREELLAMA_SERVE_ENDPOINT ?? "http://127.0.0.1:11435",
  );
  if (
    !["http:", "https:"].includes(endpoint.protocol) ||
    !["localhost", "127.0.0.1", "[::1]"].includes(endpoint.hostname) ||
    endpoint.username ||
    endpoint.password ||
    endpoint.search ||
    endpoint.hash ||
    endpoint.pathname !== "/"
  ) {
    throw new Error(
      "FREELLAMA_SERVE_ENDPOINT must be a loopback HTTP(S) origin without credentials or a path.",
    );
  }
  const portText = env.FREELLAMA_VIEW_PORT ?? "5173";
  const port = Number(portText);
  if (
    !/^\d+$/.test(portText) ||
    !Number.isInteger(port) ||
    port < 1 ||
    port > 65535
  ) {
    throw new Error(
      "FREELLAMA_VIEW_PORT must be an integer between 1 and 65535.",
    );
  }
  return { endpoint: endpoint.origin, port };
}

export async function loadConfig(
  env: NodeJS.ProcessEnv = process.env,
): Promise<ViewConfig> {
  const config = parseConfig(env);
  const file = env.FREELLAMA_AUTH_TOKEN_FILE;
  if (!file) return config;
  const token = (await readFile(file, "utf8")).trim();
  if (!token || /\s/.test(token))
    throw new Error(
      "FREELLAMA_AUTH_TOKEN_FILE must contain one nonempty bearer token.",
    );
  return { ...config, token };
}
