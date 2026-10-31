// MCP progress notifications: the request-handler fields a tool needs to report a long wait.

/** The part of the SDK's request-handler `extra` that progress and cancellation use. */
export type ToolExtra = {
  signal: AbortSignal;
  _meta?: { progressToken?: string | number };
  sendNotification: (notification: {
    method: "notifications/progress";
    params: { progressToken: string | number; progress: number; total?: number; message?: string };
  }) => Promise<void>;
};

export type ProgressSink = (message: string) => void;

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
