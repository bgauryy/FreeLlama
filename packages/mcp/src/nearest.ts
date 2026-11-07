// `nearest` on an embedding run_task is ranked by the Rust core (platform/execution/nearest.rs), so
// the agent receives a few labelled matches instead of vectors it cannot read (one 768-dim vector
// is ~4,400 tokens), for synchronous calls and deferred jobs alike. This file holds the result
// types, the vector validation for unranked sliced embeddings, and the text an agent reads.

export type NearestRequest = { query?: string; top?: number; minScore?: number };
export type NearestMatch = { index: number; label: string; score: number };
export type NearestItem = { index: number; label: string };
export type NearestPair = { a: NearestItem; b: NearestItem; score: number };
export type NearestResult = { mode: "query"; matches: NearestMatch[] } | { mode: "pairs"; pairs: NearestPair[] };

/** Validate provider output before combining or comparing vectors. */
export function validateEmbeddingVectors(value: unknown, expectedCount: number, expectedDimensions?: number): number[][] {
  if (!Array.isArray(value) || value.length !== expectedCount) {
    throw new Error(`embedding cardinality mismatch: expected ${expectedCount} vectors, received ${Array.isArray(value) ? value.length : "no vector array"}`);
  }
  let dimensions = expectedDimensions;
  for (const [index, vector] of value.entries()) {
    if (!Array.isArray(vector) || vector.length === 0) throw new Error(`embedding vector ${index} must have a nonzero dimension`);
    dimensions ??= vector.length;
    if (vector.length !== dimensions) throw new Error(`embedding dimension mismatch at vector ${index}: expected ${dimensions}, received ${vector.length}`);
    for (const item of vector) if (typeof item !== "number" || !Number.isFinite(item)) {
      throw new Error(`embedding vector ${index} contains a nonfinite or nonnumeric value`);
    }
  }
  return value as number[][];
}

/** One line per match, for the text an agent reads. */
export function nearestText(result: NearestResult): string {
  if (result.mode === "query") {
    if (result.matches.length === 0) return "no item reached minScore";
    return result.matches.map((match) => `${match.score.toFixed(3)}  ${match.label}`).join("\n");
  }
  if (result.pairs.length === 0) return "no pair reached minScore";
  return result.pairs.map((pair) => `${pair.score.toFixed(3)}  ${pair.a.label}  ~  ${pair.b.label}`).join("\n");
}
