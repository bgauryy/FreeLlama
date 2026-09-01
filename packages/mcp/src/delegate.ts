// Per-model research trust: the on-disk evidence table and the verdict classifier that grades a
// delegated answer from what the run did, never the model's self-report.
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { REPO_ROOT } from "./config.js";

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
export type ModelGrade = { grade: "strong" | "weak" | "unusable"; note: string };

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
    } catch {
      // A malformed evidence file must not stop the server: an empty table degrades every verdict
      // to `verify`, which is conservative rather than wrong.
    }
  }
  return {};
})();

/**
 * Classify how far a delegated answer should be trusted, from observable facts only.
 *
 * The measured problem this exists to solve: the local model is 98.9% accurate on grounded
 * lookups and only ~67% on judgment calls, and it uses **the same confident tone for both** — so
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
        "and confidently while being wrong — a fast wrong answer is not a speed win. Re-run with " +
        "a ~27B model (see README), or answer it yourself.",
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
        "Check the evidence trail, or re-run on a ~27B model (see README) if the answer matters.",
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
  // Judgment questions are the ~67% bucket. This is a keyword heuristic, not a classifier, and is
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
        "The question reads as a judgment call (keyword heuristic), which is the ~67%-accurate " +
        "bucket rather than the 98.9% one — and the tone is identical either way. Check the " +
        "evidence trail against the claim before acting on it.",
      measuredBaseRate: `${model}: ~67% on judgment calls vs 98.9% on grounded lookups`,
    };
  }
  if (evidenceCount > 5) {
    return {
      recommendation: "verify",
      grounded: true,
      why:
        `${evidenceCount} tool calls is outside the 1-5 file envelope this tool was measured on. ` +
        "Wide searches are where it drifts; spot-check the evidence trail.",
      measuredBaseRate: `${model}: 98.9% on grounded lookups, measured within a 1-5 file scope`,
    };
  }
  return {
    recommendation: "accept",
    grounded: true,
    why:
      `Grounded in ${evidenceCount} tool call(s) within the measured 1-5 file envelope, the ` +
      `question is lookup-shaped, and ${model} is measured strong for this (${evidence.note}).`,
    measuredBaseRate: `${model}: 98.9% on grounded lookups (100+ questions)`,
  };
}
