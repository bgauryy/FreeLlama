import type { IncomingMessage, ServerResponse } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, resolve } from "node:path";
import type { Snapshot } from "../shared/contracts";

type Middleware = (
  req: IncomingMessage,
  res: ServerResponse,
  next: () => void,
) => void;
interface AppOptions {
  collector: { snapshot(): Promise<Snapshot> };
  staticDir?: string;
  middleware?: Middleware;
  development?: boolean;
}

function localRequest(req: IncomingMessage): boolean {
  const host = req.headers.host;
  if (!host || !/^(localhost|127\.0\.0\.1|\[::1\])(?::\d+)?$/.test(host))
    return false;
  if (req.headers["sec-fetch-site"] === "cross-site") return false;
  if (req.headers.origin) {
    try {
      const origin = new URL(req.headers.origin);
      if (
        origin.host !== host ||
        !["http:", "https:"].includes(origin.protocol)
      )
        return false;
    } catch {
      return false;
    }
  }
  return true;
}

function json(res: ServerResponse, code: number, value: unknown) {
  res.writeHead(code, { "Content-Type": "application/json; charset=utf-8" });
  res.end(JSON.stringify(value));
}

const contentTypes: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
};

async function serveFile(
  req: IncomingMessage,
  res: ServerResponse,
  pathname: string,
  staticDir: string,
) {
  const base = resolve(staticDir);
  const relative =
    pathname === "/"
      ? "index.html"
      : decodeURIComponent(pathname).replace(/^\//, "");
  const file = resolve(base, relative);
  if (
    !file.startsWith(`${base}/`) ||
    relative.split(/[\\/]/).some((part) => part.startsWith("."))
  ) {
    json(res, 404, { error: "Not found." });
    return;
  }
  try {
    const metadata = await stat(file);
    if (!metadata.isFile()) {
      json(res, 404, { error: "Not found." });
      return;
    }
    const data = req.method === "HEAD" ? undefined : await readFile(file);
    res.writeHead(200, {
      "Content-Type": contentTypes[extname(file)] ?? "application/octet-stream",
      "Content-Length": metadata.size,
      "Cache-Control": relative.startsWith("assets/")
        ? "public, max-age=31536000, immutable"
        : "no-store",
    });
    res.end(data);
  } catch {
    json(res, 404, { error: "Not found." });
  }
}

export function createApp(options: AppOptions) {
  return async (req: IncomingMessage, res: ServerResponse): Promise<void> => {
    res.setHeader("Cache-Control", "no-store");
    res.setHeader("X-Content-Type-Options", "nosniff");
    res.setHeader("X-Frame-Options", "DENY");
    res.setHeader("Referrer-Policy", "no-referrer");
    if (!localRequest(req)) {
      json(res, 403, {
        error: "Only same-origin localhost requests are allowed.",
      });
      return;
    }
    if (!options.development)
      res.setHeader(
        "Content-Security-Policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
      );
    try {
      const url = new URL(req.url ?? "/", "http://localhost");
      if (url.pathname.startsWith("/api/")) {
        if (req.method !== "GET") {
          res.setHeader("Allow", "GET");
          json(res, 405, { error: "The runtime view is read-only." });
        } else if (url.pathname !== "/api/view/snapshot")
          json(res, 404, { error: "Unknown view endpoint." });
        else if (url.search)
          json(res, 400, {
            error: "Snapshot does not accept query parameters.",
          });
        else json(res, 200, await options.collector.snapshot());
        return;
      }
      if (req.method !== "GET" && req.method !== "HEAD") {
        res.setHeader("Allow", "GET, HEAD");
        json(res, 405, { error: "Method not allowed." });
        return;
      }
      if (options.middleware)
        options.middleware(req, res, () => {
          json(res, 404, { error: "Not found." });
        });
      else if (options.staticDir)
        await serveFile(req, res, url.pathname, options.staticDir);
      else json(res, 404, { error: "Not found." });
    } catch {
      if (!res.headersSent)
        json(res, 500, { error: "Could not produce a runtime snapshot." });
      else res.end();
    }
  };
}
