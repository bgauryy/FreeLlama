import { useEffect, useState } from "react";
import { snapshotSchema } from "../shared/contracts";
import type { Snapshot } from "../shared/contracts";

export function useRuntime(paused: boolean, revision: number) {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    if (paused) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const controller = new AbortController();
    async function refresh() {
      try {
        const response = await fetch("/api/view/snapshot", {
          cache: "no-store",
          signal: AbortSignal.any([
            controller.signal,
            AbortSignal.timeout(12_000),
          ]),
        });
        if (!response.ok)
          throw new Error(`The view server returned HTTP ${response.status}.`);
        const next = snapshotSchema.parse(await response.json());
        if (!disposed) {
          setSnapshot(next);
          setError(null);
        }
      } catch (failure) {
        if (!disposed)
          setError(
            failure instanceof Error &&
              failure.message.startsWith("The view server")
              ? failure.message
              : "Connection lost. Showing the last observation; reconnecting automatically.",
          );
      } finally {
        if (!disposed) {
          setLoading(false);
          timer = setTimeout(() => {
            void refresh();
          }, 2000);
        }
      }
    }
    void refresh();
    return () => {
      disposed = true;
      controller.abort();
      clearTimeout(timer);
    };
  }, [paused, revision]);
  return { snapshot, error, loading };
}
