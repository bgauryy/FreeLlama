// `nearest` on an embedding run_task: rank inside the MCP server so the agent receives a few
// labelled matches instead of vectors it cannot read (one 768-dim vector is ~4,400 tokens).

export type NearestRequest = { query?: string; top?: number; minScore?: number };
export type NearestMatch = { index: number; label: string; score: number };
export type NearestItem = { index: number; label: string };
export type NearestPair = { a: NearestItem; b: NearestItem; score: number };

/** Most items `nearest` compares pairwise: n² dot products stay well under a second. */
export const MAX_PAIRWISE_ITEMS = 4_000;

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

// Scaling before squaring avoids overflow/underflow for finite provider values.
function norm(vector: number[]): { scale: number; length: number } {
  let scale = 0;
  for (const value of vector) scale = Math.max(scale, Math.abs(value));
  if (scale === 0) return { scale, length: 0 };
  let sum = 0;
  for (const value of vector) sum += (value / scale) ** 2;
  return { scale, length: Math.sqrt(sum) };
}

function cosine(a: number[], b: number[], normA: ReturnType<typeof norm>, normB: ReturnType<typeof norm>): number {
  if (normA.scale === 0 || normB.scale === 0) return 0;
  let dot = 0;
  for (let i = 0; i < a.length; i++) dot += (a[i]! / normA.scale / normA.length) * (b[i]! / normB.scale / normB.length);
  return Math.max(-1, Math.min(1, dot));
}

const round = (score: number) => Math.round(score * 1000) / 1000;

/**
 * With a query (embedded first, at index 0), the `top` items closest to it; without one, the
 * `top` most similar pairs among the items, for duplicates. `labels[i]` names item i (after the
 * query). Scores are cosine similarity, rounded to 3 places.
 */
export function nearest(
  vectors: number[][],
  labels: string[],
  request: NearestRequest,
): { mode: "query"; matches: NearestMatch[] } | { mode: "pairs"; pairs: NearestPair[] } {
  validateEmbeddingVectors(vectors, labels.length + (request.query !== undefined ? 1 : 0));
  const top = Math.max(1, Math.min(request.top ?? 5, 100));
  const floor = request.minScore ?? -1;
  const norms = vectors.map(norm);
  if (request.query !== undefined) {
    const [query, ...items] = vectors;
    if (!query) return { mode: "query", matches: [] };
    // A file split into chunks is listed once, as its best chunk, so the top results name
    // `top` different files instead of neighbouring chunks of one.
    const seen = new Set<string>();
    const matches = items
      .map((vector, index) => ({ index, label: labels[index] ?? `input[${index}]`, score: cosine(query, vector, norms[0]!, norms[index + 1]!) }))
      .filter((match) => match.score >= floor)
      .sort((left, right) => right.score - left.score)
      .filter((match) => {
        const file = fileOf(match.label) ?? match.label;
        if (seen.has(file)) return false;
        seen.add(file);
        return true;
      })
      .slice(0, top)
      .map((match) => ({ ...match, score: round(match.score) }));
    return { mode: "query", matches };
  }
  if (vectors.length > MAX_PAIRWISE_ITEMS) {
    throw new Error(`nearest without a query compares every pair; at most ${MAX_PAIRWISE_ITEMS} items`);
  }
  const best: NearestPair[] = [];
  for (let i = 0; i < vectors.length; i++) {
    for (let j = i + 1; j < vectors.length; j++) {
      // Two chunks of one file are not a duplicate pair.
      if (sameFile(labels[i], labels[j])) continue;
      const score = cosine(vectors[i]!, vectors[j]!, norms[i]!, norms[j]!);
      if (score < floor) continue;
      if (best.length === top && score <= best[best.length - 1]!.score) continue;
      best.push({ a: { index: i, label: labels[i] ?? `input[${i}]` }, b: { index: j, label: labels[j] ?? `input[${j}]` }, score });
      best.sort((left, right) => right.score - left.score);
      if (best.length > top) best.pop();
    }
  }
  return {
    mode: "pairs",
    pairs: best.map((pair) => ({ ...pair, score: round(pair.score) })),
  };
}

/** The file of a chunk label `path:Lstart-Lend`, or undefined for a plain item. */
function fileOf(label: string | undefined): string | undefined {
  return label?.match(/^(.*):L\d+-\d+$/)?.[1];
}

function sameFile(left: string | undefined, right: string | undefined): boolean {
  const a = fileOf(left);
  return a !== undefined && a === fileOf(right);
}

/** One line per match, for the text an agent reads. */
export function nearestText(result: ReturnType<typeof nearest>): string {
  if (result.mode === "query") {
    if (result.matches.length === 0) return "no item reached minScore";
    return result.matches.map((match) => `${match.score.toFixed(3)}  ${match.label}`).join("\n");
  }
  if (result.pairs.length === 0) return "no pair reached minScore";
  return result.pairs.map((pair) => `${pair.score.toFixed(3)}  ${pair.a.label}  ~  ${pair.b.label}`).join("\n");
}
