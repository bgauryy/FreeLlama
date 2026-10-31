// Batch progress: read GET /task-batches/{id} while a batch call runs and report each change.
import { serveBatchProgress, type BatchProgressView } from "./serve-http.js";

/** One line for a batch's progress: `batch: 3/8 done (1 failed), running: a, b`. */
export function batchProgressMessage(view: BatchProgressView): string {
  const failed = view.failed > 0 ? ` (${view.failed} failed)` : "";
  const running = view.running.length > 0 ? `, running: ${view.running.join(", ")}` : "";
  const queued = view.queued > 0 ? `, ${view.queued} queued` : "";
  return `batch: ${view.done}/${view.total} done${failed}${running}${queued}`;
}

/**
 * While `work` runs, poll the batch's progress and report each change; polling starts at 250 ms
 * and backs off to 2 s while nothing changes. Stops when `work` settles.
 */
export async function reportBatchProgress<T>(
  id: string,
  endpoint: string | undefined,
  report: (message: string) => void,
  work: Promise<T>,
  signal?: AbortSignal,
): Promise<T> {
  let settled = false;
  const done = work.finally(() => {
    settled = true;
  });
  const poll = (async () => {
    let delay = 250;
    let last = "";
    while (!settled && !signal?.aborted) {
      await new Promise((resolve) => setTimeout(resolve, delay));
      if (settled) break;
      const view = await serveBatchProgress(id, { endpoint, signal });
      if (view === "unsupported") break;
      const message = view === undefined ? "" : batchProgressMessage(view);
      if (message && message !== last) {
        report(message);
        last = message;
        delay = 250;
      } else {
        delay = Math.min(delay * 2, 2000);
      }
    }
  })();
  const result = await done;
  await poll.catch(() => undefined);
  return result;
}

