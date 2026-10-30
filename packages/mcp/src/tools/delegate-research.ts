// delegate_research: bounded read-only lookup through a local research adapter subprocess.
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import * as z from "zod";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import {
  DEFAULT_DELEGATE_MAX_TURNS, DEFAULT_DELEGATE_MODEL, DEFAULT_DELEGATE_TIMEOUT_SECONDS, DEFAULT_RESEARCH_ADAPTER,
  DEFAULT_SERVE_ENDPOINT, DEFAULT_TOKEN_CALIBRATION_DIR, RESEARCH_ADAPTERS, type ResearchAdapter, assertAllowedWorkspace,
  delegateEnvironment,
} from "../config.js";
import { route } from "../serve-client.js";
import { killDelegateTree, liveDelegates, runDelegate } from "../lifecycle.js";
import { EXTERNAL_COST } from "../telemetry.js";
import {
  ANSWER_TEXT_MAX_CHARS, clipText, costTelemetry, endpointParam,
  structuredResult,
} from "../helpers.js";
import {
  MODEL_EVIDENCE, STRONG_MODEL_HINT, assessDelegatedAnswer, extractExistingWorkspacePath, parseAdapterResult, researchErrorResult,
} from "../delegate.js";

const researchResultSchema = z.object({
  answer: z.string(), summary: z.string(), citations: z.unknown().optional(), verification: z.unknown(), usage: z.unknown().optional(), telemetry: z.unknown().optional(),
}).loose();


async function routedResearchModel(endpoint: string | undefined): Promise<string> {
  const decision = JSON.parse(
    await route(endpoint, "coding", "balanced", undefined, undefined, undefined, [], undefined, undefined, undefined),
  ) as { selected_model?: unknown };
  if (typeof decision.selected_model !== "string" || !decision.selected_model) {
    throw new Error(
      "No research model: pass `model`, set FREELLAMA_MCP_DEFAULT_MODEL, or install a coding model " +
        "(models{view:'installed'} shows what routing can use).",
    );
  }
  return decision.selected_model;
}

export function registerDelegateResearchTool(server: McpServer): void {
  server.registerTool(
    "delegate_research",
    {
      description:
        "Use when: a bounded read-only question about files under workspacePath (where is X, what calls Y). Do not use when: editing, external facts, or final judgment. " +
        "Returns: citations/answer; discard verification.recommendation=escalate, otherwise verify citations.",
      inputSchema: z.object({
        question: z.string().min(1),
        workspacePath: z.string().min(1),
        adapter: z.enum(["bash", "octocode"]).optional(),
        model: z.string().min(1).optional(),
        endpoint: endpointParam,
        agent: z
          .object({
            maxTurns: z.number().int().positive().optional(),
            contextTokens: z.number().int().positive().optional(),
            outputTokens: z.number().int().positive().optional(),
          })
          .strict()
          .optional(),
      }).strict(),
      outputSchema: researchResultSchema,
      annotations: { destructiveHint: false },
    },
    async ({ question, workspacePath, adapter, model, endpoint, agent }, extra) => {
      const chosenAdapter: ResearchAdapter = adapter ?? DEFAULT_RESEARCH_ADAPTER;
      let resolvedWorkspace: string;
      let chosenModel: string;
      try {
        resolvedWorkspace = await assertAllowedWorkspace(workspacePath);
        chosenModel = model ?? DEFAULT_DELEGATE_MODEL ?? await routedResearchModel(endpoint);
      } catch (error) {
        return researchErrorResult(error);
      }
      // Pre-flight, not post-hoc: a model this repo measured at 0-38% will not become right by
      // running it, so refuse before spending a model load and 10-40s of wall time on it.
      const known = MODEL_EVIDENCE[chosenModel];
      if (known?.grade === "unusable") {
        return structuredResult({
          adapter: chosenAdapter,
          verification: assessDelegatedAnswer(question, 0, chosenModel),
          answer: "",
          toolCallCount: 0,
          usage: { inputTokens: null, outputTokens: null },
          telemetry: costTelemetry({ inputTokens: null, outputTokens: null }, EXTERNAL_COST),
          evidence: [],
          summary:
            `Refused before running: ${chosenModel} is measured unusable for research here ` +
            `(${known.note}). ${STRONG_MODEL_HINT}, or answer it yourself.`,
        });
      }
      const dir = await mkdtemp(path.join(tmpdir(), "freellama-delegate-"));
      const promptFile = path.join(dir, "prompt.md");
      const resultFile = path.join(dir, "result.json");
      try {
        await writeFile(promptFile, `${question}\n`, "utf8");
        const adapter = RESEARCH_ADAPTERS[chosenAdapter];
        if (!existsSync(adapter)) {
          return researchErrorResult(
            new Error(
              `research adapter not found at ${adapter}. It is compiled to <package>/dist/research ` +
                "by the package build. Reinstall, or run `yarn build` from packages/mcp/ to rebuild it.",
            ),
          );
        }
        const running = runDelegate(process.execPath, [adapter], {
          signal: extra.signal,
          env: {
            ...delegateEnvironment(),
            FREELLAMA_TARGET_MODEL: chosenModel,
            FREELLAMA_AGENT_MANAGED_ENDPOINT: endpoint ?? DEFAULT_SERVE_ENDPOINT,
            FREELLAMA_AGENT_TOKEN_CALIBRATION_DIR: DEFAULT_TOKEN_CALIBRATION_DIR,
            FREELLAMA_BENCH_WORKSPACE: resolvedWorkspace,
            FREELLAMA_BENCH_PROMPT: promptFile,
            FREELLAMA_AGENT_RESULT: resultFile,
            FREELLAMA_AGENT_MAX_TURNS: String(agent?.maxTurns ?? DEFAULT_DELEGATE_MAX_TURNS),
            ...(agent?.contextTokens !== undefined ? { FREELLAMA_AGENT_NUM_CTX: String(agent.contextTokens) } : {}),
            ...(agent?.outputTokens !== undefined ? { FREELLAMA_AGENT_NUM_PREDICT: String(agent.outputTokens) } : {}),
          },
          timeoutMs: DEFAULT_DELEGATE_TIMEOUT_SECONDS * 1000,
          // The answer is read from `resultFile`, never from stdout — but execFile's usual 1 MB
          // maxBuffer still applies to the agent's own progress logging, and overflowing it kills
          // the subprocess and surfaces as a research failure with no explanation. Give the logs
          // room; nothing here is proportional to the size of the answer.
          maxBuffer: 32 * 1024 * 1024,
          // On timeout, abort, or overflow runDelegate SIGKILLs the whole process group: SIGTERM can
          // be swallowed by an adapter blocked in a long model call, and killing the adapter alone
          // would orphan `npx octocode` and sandbox shells.
        });
        liveDelegates.add(running.child);
        // The adapter exits non-zero for its *own* failures — the model returned prose instead of
        // JSON, the endpoint was unreachable — but it still writes result.json first, with the real
        // diagnosis in `final_answer`. Letting the exec rejection propagate replaced that diagnosis
        // with "Command failed: node …", which names the wrong layer and hides the evidence trail
        // showing how far the run actually got. Capture it and prefer the adapter's own account.
        let adapterError: unknown = null;
        try {
          await running.done;
        } catch (error) {
          adapterError = error;
        } finally {
          liveDelegates.delete(running.child);
          // Reap anything the adapter left behind in its group, even after a normal exit.
          killDelegateTree(running.child);
        }
        let result: ReturnType<typeof parseAdapterResult>;
        try {
          // Read AND parse under one guard. A SIGKILL at the timeout can land mid-write, leaving a
          // truncated result.json — reading it succeeds and only the parse fails, so guarding the
          // read alone reported "Unexpected end of JSON input" and threw away the timeout diagnosis
          // that actually explains the run. A valid object missing `final_answer` is the same class
          // of failure: unusable, not "empty trail".
          result = parseAdapterResult(await readFile(resultFile, "utf8"));
        } catch {
          // No usable result file: a hard kill (the SIGKILL timeout above) or a crash before the
          // adapter could finish writing. Here the exec error genuinely is the best account.
          return researchErrorResult(
            adapterError ??
              new Error(
                "research adapter exited without writing a readable result file — it was killed " +
                  `before it could report. Check that the model is loadable and that ${DEFAULT_DELEGATE_TIMEOUT_SECONDS}s ` +
                  "is enough for this question.",
              ),
          );
        }
        // Surface the evidence trail (which tool, which path) so the orchestrator can spot-check
        // *how* the answer was reached without re-deriving it — verifier independence in practice,
        // not just in principle. A citable but unread-through answer is exactly the failure mode
        // task-delegation.md warns about for judgment-heavy tasks.
        // The two adapters describe their calls differently — octocode puts the tool under
        // `arguments.tool` with a path in `arguments.queries.path`, bash reports `shell` with the
        // command line in `arguments.command`. Normalize both so the evidence trail reads the same
        // whichever adapter ran.
        const evidence = result.tool_calls.map((call, index) => {
          const target = call.arguments?.queries?.path;
          const rawDetail = call.arguments?.command ?? null;
          const detail = rawDetail ? clipText(rawDetail, 400) : null;
          return {
            step: index + 1,
            tool: call.arguments?.tool ?? call.raw_name ?? "?",
            // The adapters record "ok" | "error" | "repeat" per call. Carrying it through is what
            // lets a reader tell a run that read three files from one that failed three commands —
            // indistinguishable in the trail before, and graded identically.
            status: call.status ?? "ok",
            path: target
              ? path.relative(resolvedWorkspace, target)
              : detail
                ? extractExistingWorkspacePath(detail, resolvedWorkspace)
                : null,
            detail,
            detail_truncated: rawDetail !== detail,
          };
        });
        const succeeded = evidence.filter((step) => step.status === "ok");
        const failed = evidence.length - succeeded.length;
        // Commands can be arbitrarily long and are not reasoning-relevant by themselves. Keep a
        // marked excerpt in both halves; the cited path and tool remain enough to spot-check source.
        const evidenceText = evidence
          .map(
            (step) =>
              `  ${step.step}. ${step.tool}${step.status === "ok" ? "" : ` [${step.status}]`}` +
              `${step.path ? ` -> ${step.path}` : ""}` +
              `${step.detail ? `: ${step.detail}` : ""}`,
          )
          .join("\n");
        // The compact machine-readable half. Two independent small-model callers asked for exactly
        // this shape — recommendation, why, citations — rather than parsing it back out of the prose.
        // Successful steps only: a failed command is not a citation for anything.
        // Citations point into `evidence` by step instead of repeating each command a second time.
        const citations = succeeded.map((step) => ({ step: step.step, tool: step.tool, path: step.path }));
        if (adapterError) {
          const diagnostic = `research adapter failed: ${result.final_answer}` +
            (evidenceText ? `\nEvidence collected before the failure:\n${evidenceText}` : "");
          const refusal = z.object({ receipt: z.object({ error: z.string() }).loose() })
            .safeParse(result.model_metadata?.transport_error);
          if (refusal.success) {
            return researchErrorResult(new Error(JSON.stringify(refusal.data.receipt)), diagnostic);
          }
          return researchErrorResult(new Error(diagnostic));
        }
        // Grade on what actually read something. A run of failed commands is ungrounded no matter
        // how many of them there were.
        const verification = assessDelegatedAnswer(question, succeeded.length, chosenModel);
        const summary =
          `Delegated answer ready: ${result.tool_calls.length} tool call(s)` +
          (failed > 0 ? `, ${failed} of which did not succeed` : "") +
          `; ${result.usage.input_tokens ?? "?"} input / ${result.usage.output_tokens ?? "?"} output local tokens; ` +
          `verification=${verification.recommendation}. Read structuredContent.answer and citations.`;
        const payload = {
            adapter: chosenAdapter,
            verification,
            answer: result.final_answer,
            // recommendation + why live under `verification`; `citations` completes the triple so a
            // caller never has to read `summary` to act on the result.
            citations,
            toolCallCount: result.tool_calls.length,
            successfulToolCallCount: succeeded.length,
            usage: {
              inputTokens: result.usage.input_tokens,
              outputTokens: result.usage.output_tokens,
            },
            telemetry: costTelemetry({
              inputTokens: result.usage.input_tokens ?? null,
              outputTokens: result.usage.output_tokens ?? null,
            }, EXTERNAL_COST),
            contextManagement: result.model_metadata?.context_management ?? null,
            execution: { receipts: result.model_metadata?.execution_receipts ?? [] },
            evidence,
            summary,
          };
        // The answer itself goes in TextContent: clients that forward only text to the model
        // otherwise received a pointer to structuredContent and never the answer.
        const cited = [...new Set(citations.map((citation) => citation.path).filter(Boolean))];
        const answerText =
          `${clipText(result.final_answer, ANSWER_TEXT_MAX_CHARS)}\n\n` +
          `verification: ${verification.recommendation}` +
          (cited.length ? `\ncited: ${cited.slice(0, 20).join(", ")}` : "") +
          `\n(${chosenModel}, ${result.tool_calls.length} tool call(s))`;
        return structuredResult(payload, { text: answerText });
      } catch (error) {
        return researchErrorResult(error);
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    },
  );
}
