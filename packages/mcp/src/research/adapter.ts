// The contract between the shared research loop (agent-loop.ts) and one adapter: its prompt,
// action parser, tool runner and labels, plus test and embedding seams.
import type { Action } from "./actions.js";
import type { AgentRuntimeConfig, Env } from "./context-policy.js";
import type { ObservationStore } from "./observations.js";
import type { resolveModelIdentity } from "./transport.js";

export type { Action };

export interface ToolCall {
  /** Repeat-detection payload (`callSignature`). */
  signature: unknown;
  /** Run the tool; a failure that satisfies `isToolError` becomes a `tool error: ...` observation. */
  run: () => Promise<string>;
  /** Fields recorded in `tool_calls[]` before `status` and `duration_ms`. */
  record: Record<string, unknown>;
  /** Observation-store ledger metadata, given the call status. */
  ledger: (status: string) => Record<string, unknown>;
}

export interface AdapterSpec {
  defaultToolTimeoutSeconds: number;
  systemPrompt: (workspace: string) => string;
  parseAction: (content: string) => Action;
  /** Enter adapter resources (e.g. the read-only shell) after the context is fitted. */
  open?: (workspace: string) => Promise<AdapterSession> | AdapterSession;
  /** Per-turn reminder appended to an observation, given the remaining call budget. */
  remaining: (count: number) => string;
  exhausted: string;
  adapterMetadata: (session: AdapterSession | undefined) => Record<string, unknown>;
}

export interface AdapterSession {
  toolCall: (action: Action, workspace: string, runtime: AgentRuntimeConfig) => ToolCall;
  close?: () => void;
}

/** Seams for tests and embedding; every field defaults to the real implementation. */
export interface AdapterHooks {
  env?: Env;
  requestJson?: (url: string, payload: unknown, timeoutSeconds: number) => unknown | Promise<unknown>;
  resolveModelIdentity?: typeof resolveModelIdentity;
  /** Replaces the tool runner; receives the recorded call arguments. */
  runTool?: (args: Record<string, unknown>) => string | Promise<string>;
  observationStore?: (pageSize: number) => ObservationStore;
  print?: (line: string) => void;
}

/** The page action shape: a positive integer (`true` counts as 1). */
export function isPositiveInt(value: unknown): boolean {
  return (typeof value === "boolean" || (typeof value === "number" && Number.isInteger(value))) && Number(value) >= 1;
}
