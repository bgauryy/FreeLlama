// MCP progress notifications: the request-handler fields a tool needs to report a long wait.

/** The part of the SDK's request-handler `extra` that progress and cancellation use. */
export type ToolExtra = {
  signal: AbortSignal;
  sessionId?: string;
  _meta?: { progressToken?: string | number };
  sendNotification: (notification:
    | {
      method: "notifications/progress";
      params: { progressToken: string | number; progress: number; total?: number; message?: string };
    }
    | {
      method: "notifications/message";
      params: { level: "info" | "warning"; logger: string; data: string };
    }) => Promise<void>;
};

export type ProgressSink = (message: string) => void;

/** Whether the client's `logging/setLevel` filters out `level`; installed once by the server. */
let logLevelFilter: ((level: string, sessionId?: string) => boolean) | undefined;

/**
 * Make `notice` honor the level a client set with `logging/setLevel`. A request-scoped
 * `sendNotification` bypasses the SDK's own filter, which only `sendLoggingMessage` applies.
 */
export function useLogLevelFilter(filter: (level: string, sessionId?: string) => boolean): void {
  logLevelFilter = filter;
}

/**
 * A sink that sends `notifications/progress` when the client sent a progressToken, else
 * `undefined` so callers keep their silent path. Each message advances the progress count by one,
 * as MCP requires it to increase; a failed notification never fails the tool.
 */
export function progressSink(extra: Partial<ToolExtra> | undefined): ProgressSink | undefined {
  const token = extra?._meta?.progressToken;
  const send = extra?.sendNotification;
  if (token === undefined || !send) return undefined;
  let step = 0;
  return (message) => {
    step += 1;
    void send({ method: "notifications/progress", params: { progressToken: token, progress: step, message } })
      .catch(() => undefined);
  };
}

/**
 * A log notification (`notifications/message`), which a client shows without having asked for
 * progress: kept for what a person may act on, such as a long memory wait or what to close.
 */
export function notice(extra: Partial<ToolExtra> | undefined, level: "info" | "warning", message: string): void {
  if (logLevelFilter?.(level, extra?.sessionId)) return;
  void extra?.sendNotification?.({ method: "notifications/message", params: { level, logger: "freellama", data: message } })
    .catch(() => undefined);
}

const seconds = (ms: number) => `${(ms / 1000).toFixed(1)} s`;

/**
 * The notices a finished managed task deserves, read from its receipt: a load that took over a
 * second, models evicted to make room, or a wait for capacity over two seconds. Quick calls on a
 * loaded model say nothing.
 */
export function taskNotices(content: Record<string, unknown>): string[] {
  const record = (value: unknown) => (value && typeof value === "object" ? value as Record<string, unknown> : {});
  const metrics = record(content.metrics);
  const admission = record(content.admission);
  const execution = record(content.execution);
  const route = record(content.route);
  const model = typeof route.selected_model === "string" ? route.selected_model : "model";
  const notices: string[] = [];
  const loadMs = typeof metrics.load_duration_ns === "number" ? metrics.load_duration_ns / 1e6 : 0;
  const evicted = record(execution.memory_reservation).evicted_idle_models;
  const evictedNames = Array.isArray(evicted) ? evicted.filter((item) => typeof item === "string") : [];
  if (loadMs > 1000 || evictedNames.length > 0) {
    notices.push(`loaded ${model} in ${seconds(loadMs)}${evictedNames.length > 0 ? `; unloaded ${evictedNames.join(", ")} to make room` : ""}`);
  }
  const waitMs = typeof admission.total_wait_ms === "number" ? admission.total_wait_ms : 0;
  if (waitMs > 2000) notices.push(`${model} waited ${seconds(waitMs)} for capacity before running`);
  return notices;
}
