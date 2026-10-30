// Opaque-cursor paging for live lists (raw Ollama tags, library tag pages).
import { createHash } from "node:crypto";

type Page<T> = { items: T[]; returned: number; total: number; next_cursor: string | null };

/** Page a live list with an opaque cursor that refuses to continue after list drift. */
export function pageLiveList<T>(items: T[], limit: number | undefined, cursor: string | undefined, identity: (item: T) => string): Page<T> {
  const pageSize = limit ?? 20;
  const fingerprint = createHash("sha256").update(items.map(identity).join("\n")).digest("base64url").slice(0, 16);
  let offset = 0;
  if (cursor) {
    try {
      const parsed = JSON.parse(Buffer.from(cursor, "base64url").toString("utf8")) as { offset?: unknown; fingerprint?: unknown };
      if (!Number.isInteger(parsed.offset) || (parsed.offset as number) < 0 || parsed.fingerprint !== fingerprint) {
        throw new Error("invalid or stale cursor");
      }
      offset = parsed.offset as number;
    } catch {
      throw new Error("`cursor` is invalid or the model list changed; restart without cursor.");
    }
  }
  const page = items.slice(offset, offset + pageSize);
  const nextOffset = offset + page.length;
  return {
    items: page,
    returned: page.length,
    total: items.length,
    next_cursor: nextOffset < items.length
      ? Buffer.from(JSON.stringify({ offset: nextOffset, fingerprint }), "utf8").toString("base64url")
      : null,
  };
}
