// Per-model research trust: the on-disk evidence table and the verdict classifier that grades a
// delegated answer from what the run did, never the model's self-report.
import { existsSync, readFileSync, realpathSync, statSync } from "node:fs";
import path from "node:path";
import { z } from "zod";
import { REPO_ROOT } from "./config.js";
import { errorResult } from "./helpers.js";

/** Research errors keep the declared result shape without claiming a usable answer. */
export function researchErrorResult(error: unknown, diagnostic?: string) {
  const result = errorResult(error);
  const summary = diagnostic ?? result.content[0].text;
  return {
    ...result,
    content: [{ type: "text" as const, text: summary }],
    structuredContent: {
      ...(result.structuredContent ?? { error: summary }),
      answer: "",
      summary,
      verification: {
        recommendation: "escalate",
        grounded: false,
        why: "The research request failed; no usable answer was produced.",
      },
    },
  };
}

/**
 * Per-model research grades, loaded from disk — never compiled in.
 *
 * These are one machine's benchmark results. Baking them into a shipped server would make the
 * binary carry someone else's measurements as if they were universal, and they would rot silently
 * the moment models changed. The server carries the *mechanism*; the data lives in
 * `benchmark/evidence/model-evidence.json` (override with FREELLAMA_MCP_MODEL_EVIDENCE).
 *
 * Empty by default. A model with no entry is treated as unmeasured, which yields a `verify`
 * verdict — the correct answer when nothing is known, and a safer default than assuming strength.
 */
type ModelGrade = { grade: "strong" | "weak" | "unusable"; note: string };

export const MODEL_EVIDENCE: Record<string, ModelGrade> = (() => {
  const configured = process.env.FREELLAMA_MCP_MODEL_EVIDENCE;
  const candidates = [
    configured,
    path.join(REPO_ROOT, "benchmark/evidence/model-evidence.json"),
  ].filter((x): x is string => Boolean(x));
  for (const file of candidates) {
    try {
      if (!existsSync(file)) continue;
      const parsed = JSON.parse(readFileSync(file, "utf8")) as { models?: Record<string, ModelGrade> };
      return parsed.models ?? {};
    } catch (error) {
      // A malformed evidence file must not stop the server: an empty table degrades every verdict
      // to `verify`, which is conservative rather than wrong. Say so, or the operator never learns
      // why every verdict became `verify`.
      process.stderr.write(
        `freellama-mcp: ignoring unreadable model evidence ${file}: ` +
          `${error instanceof Error ? error.message : String(error)}\n`,
      );
    }
  }
  return {};
})();

/** Recovery hint for weak/unusable models; the evidence file, not this code, names which are strong. */
export const STRONG_MODEL_HINT =
  "Re-run with a model graded strong in benchmark/evidence/model-evidence.json";

/**
 * Classify how far a delegated answer should be trusted, from observable facts only.
 *
 * The measured problem this exists to solve: the strongest model measured here (a ~27B Qwen) was
 * far more accurate on grounded lookups than on judgment calls, with **the same confident tone for
 * both** — so
 * the answer text alone carries no signal about which one you got. Everything here is derived
 * from what actually happened (did it read any files? how many?) plus one clearly-labelled
 * heuristic on the question's shape. Nothing is inferred from the model's own self-report, which
 * is exactly the thing that isn't reliable.
 *
 * This never escalates on its own. It emits a recommendation the orchestrator acts on, matching
 * how the rest of this server treats state-changing decisions.
 */
export function assessDelegatedAnswer(
  question: string,
  /**
   * Count of tool calls that actually **succeeded**. Not the raw call count: a run whose commands
   * all errored, or which only repeated an earlier call, read nothing — and a verdict computed
   * from "it made 3 calls" would have graded that `accept` while the answer was pure recall. That
   * is precisely the failure this function exists to catch, so it must not be fed a number that
   * counts failures as evidence.
   */
  evidenceCount: number,
  model: string,
): {
  recommendation: "accept" | "verify" | "escalate";
  grounded: boolean;
  why: string;
  measuredBaseRate: string;
} {
  const grounded = evidenceCount > 0;
  const evidence = MODEL_EVIDENCE[model];

  // The model gates everything else. No amount of grounding rescues a model measured at 0-38%,
  // and an unmeasured model has no base rate to quote in the first place.
  if (evidence?.grade === "unusable") {
    return {
      recommendation: "escalate",
      grounded,
      why:
        `${model} is not viable for research on this machine (${evidence.note}). It answers fast ` +
        "and confidently while being wrong — a fast wrong answer is not a speed win. " +
        `${STRONG_MODEL_HINT}, or answer it yourself.`,
      measuredBaseRate: `${model}: ${evidence.note}`,
    };
  }
  if (!evidence) {
    return {
      recommendation: "verify",
      grounded,
      why:
        `${model} has no measured accuracy in this repo's benchmarks, so no base rate applies to ` +
        "this answer. Treat it as unverified until it has been evaluated — accuracy fell off a " +
        "cliff below ~12B in the models that were measured.",
      measuredBaseRate: "no measured base rate for this model",
    };
  }
  if (evidence.grade === "weak" && grounded) {
    return {
      recommendation: "verify",
      grounded,
      why:
        `${model} holds up on simple single-file lookups but not beyond (${evidence.note}). ` +
        `Check the evidence trail, or ${STRONG_MODEL_HINT.toLowerCase()} if the answer matters.`,
      measuredBaseRate: `${model}: ${evidence.note}`,
    };
  }
  if (!grounded) {
    return {
      recommendation: "escalate",
      grounded: false,
      why:
        "The model answered without reading a single file, so this is parametric recall, not " +
        "research — the failure mode where `run_task` was verified inventing wrong facts about " +
        "this project's own architecture. Re-ask with a narrower question, or answer it yourself.",
      measuredBaseRate: "ungrounded answers have no measured accuracy — they were never the tested path",
    };
  }
  // Judgment questions are the low-accuracy bucket. This is a keyword heuristic, not a classifier, and is
  // labelled as one: it errs toward asking for verification, which costs a read, not a wrong answer.
  // Bare "should" is common in factual documentation questions ("what should a custom loader
  // subclass?"). Treat explicit requests for a decision as judgment, while letting grounded
  // contract lookups reach the normal evidence gate.
  const judgmentSignals = /\b(should (we|i|you)|better|best|worth|review|assess|evaluate|improve|opinion|recommend|why is|is it (good|safe|correct)|design|refactor)\b/i;
  if (judgmentSignals.test(question)) {
    return {
      recommendation: "verify",
      grounded: true,
      why:
        "The question reads as a judgment call (keyword heuristic). Measured accuracy on judgment " +
        "calls was well below grounded lookups, and the tone is identical either way. Check the " +
        "evidence trail against the claim before acting on it.",
      measuredBaseRate: `${model}: ${evidence.note}; measured on lookups, not judgment calls`,
    };
  }
  if (evidenceCount > 5) {
    return {
      recommendation: "verify",
      grounded: true,
      why:
        `${evidenceCount} tool calls is outside the 1-5 file envelope this tool was measured on. ` +
        "Wide searches are where it drifts; spot-check the evidence trail.",
      measuredBaseRate: `${model}: ${evidence.note}; measured within a 1-5 file scope`,
    };
  }
  return {
    recommendation: "accept",
    grounded: true,
    why:
      `Grounded in ${evidenceCount} tool call(s) within the measured 1-5 file envelope, the ` +
      `question is lookup-shaped, and ${model} is measured strong for this (${evidence.note}).`,
    measuredBaseRate: `${model}: ${evidence.note}`,
  };
}

/**
 * Find the first existing file explicitly named in a shell command, confined to the workspace.
 *
 * Bash-agent calls carry only a command string, unlike Octocode calls with a structured path.
 * This deliberately does not guess from globs, `find -name`, or answer prose: a citation path is
 * emitted only when the command itself contains an existing file path inside the allowed root.
 */
export function extractExistingWorkspacePath(command: string, workspace: string): string | null {
  let root: string;
  try {
    root = realpathSync(workspace);
  } catch {
    return null;
  }
  const tokens = command.match(/(?:[^\s"'\\]+|"(?:\\.|[^"])*"|'[^']*')+/g) ?? [];
  const patternFlags = new Set(["-name", "-iname", "-path", "-ipath", "-regex", "-iregex", "-wholename"]);
  for (let index = 0; index < tokens.length; index += 1) {
    const raw = tokens[index] ?? "";
    if (index > 0 && patternFlags.has(tokens[index - 1] ?? "")) continue;
    const token = raw
      .replace(/^["']|["']$/g, "")
      .replace(/[|;&)]+$/g, "")
      .replace(/:\d+(?::\d+)?$/, "");
    if (!token || token.startsWith("-") || /[*?$`]/.test(token)) continue;
    const candidate = path.isAbsolute(token) ? token : path.resolve(root, token);
    if (!existsSync(candidate)) continue;
    try {
      const resolved = realpathSync(candidate);
      const relative = path.relative(root, resolved);
      const withinRoot = relative && relative !== ".." && !relative.startsWith(`..${path.sep}`);
      if (withinRoot && statSync(resolved).isFile()) return relative;
    } catch {
      // A raced-away or unreadable token is not citation evidence; inspect the next token.
    }
  }
  return null;
}

const adapterCallSchema = z
  .object({
    raw_name: z.string().optional(),
    status: z.string().optional(),
    arguments: z
      .object({
        tool: z.string().optional(),
        command: z.string().optional(),
        queries: z.object({ path: z.string().optional() }).passthrough().optional(),
      })
      .passthrough()
      .optional(),
  })
  .passthrough();

const adapterResultSchema = z.object({
  final_answer: z.string(),
  tool_calls: z.array(adapterCallSchema).default([]),
  usage: z
    .object({
      input_tokens: z.number().nullable().optional(),
      output_tokens: z.number().nullable().optional(),
    })
    .passthrough()
    .default({}),
  model_metadata: z
    .object({
      context_compactions: z.number().int().nonnegative().optional(),
      runtime_config: z.record(z.string(), z.unknown()).optional(),
      context_management: z
        .object({
          token_counting: z.enum(["configured_estimate", "model_calibrated_estimate"]),
          estimate_scale: z.number().positive(),
          calibration_samples: z.number().int().nonnegative(),
          pinned_overflow: z.enum(["error", "clip"]),
          compactions: z.number().int().nonnegative(),
        })
        .passthrough()
        .optional(),
    })
    .passthrough()
    .optional(),
});

type AdapterResult = z.infer<typeof adapterResultSchema>;

/** Parse adapter result.json; reject JSON missing `final_answer`/`tool_calls` here, not later. */
export function parseAdapterResult(raw: string): AdapterResult {
  return adapterResultSchema.parse(JSON.parse(raw));
}
