import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { assessDelegatedAnswer as assessArtifact, MODEL_EVIDENCE, parseModelEvidence, verifiedResearchDigest } from "../../src/delegate.js";

const DIGEST = "a".repeat(64);
const saved = { ...MODEL_EVIDENCE };
beforeEach(() => Object.assign(MODEL_EVIDENCE, parseModelEvidence({ models: {
  "qwen3.8:27b-mlx": { grade: "strong", note: "fixture measured strong", model_digest: DIGEST },
  "gemma4:12b-mlx": { grade: "weak", note: "fixture measured weak", model_digest: DIGEST },
  "qwen2.5:0.5b": { grade: "unusable", note: "fixture measured unusable", model_digest: DIGEST },
} })));
afterEach(() => {
  for (const name of Object.keys(MODEL_EVIDENCE)) delete MODEL_EVIDENCE[name];
  Object.assign(MODEL_EVIDENCE, saved);
});
const assessDelegatedAnswer = (question: string, count: number, model: string) => assessArtifact(question, count, model, DIGEST);

// Deterministic measured-artifact fixtures; never assign today's digest to historical evidence.
describe("MODEL_EVIDENCE", () => {
  it("accepts only validated evidence records", () => {
    expect(Object.keys(MODEL_EVIDENCE).length).toBeGreaterThan(0);
    expect(MODEL_EVIDENCE["qwen3.8:27b-mlx"]?.grade).toBe("strong");
    expect(MODEL_EVIDENCE["qwen2.5:0.5b"]?.grade).toBe("unusable");
  });

  it("does not qualify old records or invalid grade strings", () => {
    expect(parseModelEvidence({ models: {
      old: { grade: "strong", note: "name only" },
      invalid: { grade: "strnog", note: "typo", model_digest: DIGEST },
      guessed: { grade: "strong", note: "no identity", model_digest: "guess" },
      valid: { grade: "weak", note: "measured", model_digest: `sha256:${DIGEST}` },
    } })).toEqual({ valid: { grade: "weak", note: "measured", model_digest: DIGEST } });
    expect(() => parseModelEvidence({ models: "invalid" })).toThrow();
  });

  it("requires the same executed artifact on every managed turn", () => {
    const model = "qwen3.8:27b-mlx";
    const receipt = (digest = DIGEST) => ({ model, execution: {
      model_digest: digest, observation: { resident: true, digest },
    } });
    expect(verifiedResearchDigest([receipt(), receipt()], model)).toBe(DIGEST);
    for (const records of [[], [receipt(), receipt("b".repeat(64))], [receipt(), { model, execution: { model_digest: DIGEST } }],
      [{ ...receipt(), model: "other:tag" }]]) expect(verifiedResearchDigest(records, model)).toBeUndefined();
    for (const digest of [undefined, "b".repeat(64)]) {
      expect(assessArtifact("Where is x declared?", 2, model, digest).recommendation).toBe("verify");
    }
  });
});

describe("assessDelegatedAnswer", () => {
  const STRONG = "qwen3.8:27b-mlx";

  it("escalates an unusable model regardless of grounding, naming its measured note", () => {
    const verdict = assessDelegatedAnswer("what does config.ts export?", 3, "qwen2.5:0.5b");
    expect(verdict.recommendation).toBe("escalate");
    expect(verdict.measuredBaseRate).toMatch(/qwen2\.5:0\.5b/);
  });

  it("returns verify for an unmeasured model — no base rate applies", () => {
    const verdict = assessDelegatedAnswer("what does config.ts export?", 3, "totally-unmeasured:1b");
    expect(verdict.recommendation).toBe("verify");
    expect(verdict.measuredBaseRate).toMatch(/no measured base rate/);
  });

  it("escalates an ungrounded answer from a strong model — recall is not research", () => {
    const verdict = assessDelegatedAnswer("what does config.ts export?", 0, STRONG);
    expect(verdict.recommendation).toBe("escalate");
    expect(verdict.grounded).toBe(false);
  });

  it("returns verify for a weak model even when grounded", () => {
    const verdict = assessDelegatedAnswer("what does config.ts export?", 2, "gemma4:12b-mlx");
    expect(verdict.recommendation).toBe("verify");
  });

  it("returns verify for judgment-shaped questions (the low-accuracy bucket)", () => {
    const verdict = assessDelegatedAnswer("should we refactor the proxy retry logic?", 2, STRONG);
    expect(verdict.recommendation).toBe("verify");
    expect(verdict.measuredBaseRate).toMatch(/judgment/);
  });

  it("does not mistake factual documentation wording containing 'should' for judgment", () => {
    const verdict = assessDelegatedAnswer(
      "What should a custom Jinja loader subclass, and which method should it override?",
      2,
      STRONG,
    );
    expect(verdict.recommendation).toBe("accept");
  });

  it("returns verify outside the measured 1-5 file envelope", () => {
    const verdict = assessDelegatedAnswer("list every retry site", 6, STRONG);
    expect(verdict.recommendation).toBe("verify");
    expect(verdict.why).toMatch(/envelope/);
  });

  it("accepts a grounded, lookup-shaped answer from a strong model in envelope", () => {
    const verdict = assessDelegatedAnswer(
      "In packages/rust-core/Cargo.toml, what optional feature enables the Node addon?",
      2,
      STRONG,
    );
    expect(verdict.recommendation).toBe("accept");
    expect(verdict.grounded).toBe(true);
  });

  it("quotes each strong model's own measured note, never another model's base rate", () => {
    const strong = Object.keys(MODEL_EVIDENCE).filter((name) => MODEL_EVIDENCE[name].grade === "strong");
    expect(strong.length).toBeGreaterThan(0);
    for (const model of strong) {
      const verdict = assessDelegatedAnswer("where is DEFAULT_SERVE_ENDPOINT defined?", 2, model);
      expect(verdict.measuredBaseRate).toBe(`${model}: ${MODEL_EVIDENCE[model].note}`);
      expect(verdict.why).not.toMatch(/27B|see README/);
    }
  });
});
