import { describe, expect, it } from "vitest";
import { assessDelegatedAnswer, MODEL_EVIDENCE } from "../../src/delegate.js";

// The evidence table is loaded from benchmark/evidence/model-evidence.json at import time; these
// tests run inside the repo, so the real table is present. If a model named here is removed from
// the evidence file, the first test fails loudly instead of the verdicts silently degrading.
describe("MODEL_EVIDENCE", () => {
  it("loads the on-disk evidence table (not compiled in, not empty)", () => {
    expect(Object.keys(MODEL_EVIDENCE).length).toBeGreaterThan(0);
    expect(MODEL_EVIDENCE["qwen3.8:27b-mlx"]?.grade).toBe("strong");
    expect(MODEL_EVIDENCE["qwen2.5:0.5b"]?.grade).toBe("unusable");
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

  it("returns verify for judgment-shaped questions (the ~67% bucket)", () => {
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
});
