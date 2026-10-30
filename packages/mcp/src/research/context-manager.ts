// Stateful context management for one adapter run: fit each request, calibrate the token estimate
// from Ollama's `prompt_eval_count`, and persist the calibration per verified model identity.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import path from "node:path";
import { canonicalJson, isRecord } from "./canonical-json.js";
import { AgentRuntimeConfig, COMPACTION_TARGET_RATIO, contextBudget, type Message } from "./context-policy.js";
import { ContextBudgeter, fitToContext } from "./fit.js";

export interface CalibrationContract {
  schema_version: number;
  model: string;
  identity: string;
  estimator_version: number;
  chars_per_token: number;
  image_token_estimate: number;
}


function timeNs(): string {
  const subMillis = process.hrtime.bigint() % 1_000_000n;
  return (BigInt(Date.now()) * 1_000_000n + subMillis).toString();
}

/** Stateful adapter facade: fit, calibrate, count compactions, and report policy. */
export class AgentContextManager {
  readonly runtime: AgentRuntimeConfig;
  readonly model: string;
  readonly calibrationDir: string | null;
  readonly calibrationIdentity: string;
  readonly budgeter: ContextBudgeter;
  readonly calibrationSource: string;
  compactions = 0;

  constructor(runtime: AgentRuntimeConfig, options: { model?: string; calibrationDir?: string | null; calibrationIdentity?: string } = {}) {
    this.runtime = runtime;
    this.model = options.model ?? "";
    this.calibrationDir = options.calibrationDir ?? null;
    this.calibrationIdentity = options.calibrationIdentity ?? "";
    const loaded = this.loadCalibration();
    this.budgeter = new ContextBudgeter(runtime.context, { initialScale: loaded?.scale ?? 1, calibrationSamples: loaded?.samples ?? 0 });
    this.calibrationSource = loaded ? "persistent_model_cache" : "current_process";
  }

  modelKey(): string {
    return createHash("sha256").update(this.calibrationContractJson(), "utf8").digest("hex").slice(0, 24);
  }

  calibrationContract(): CalibrationContract {
    return {
      schema_version: 2,
      model: this.model,
      identity: this.calibrationIdentity,
      estimator_version: 1,
      chars_per_token: this.runtime.context.chars_per_token,
      image_token_estimate: this.runtime.context.image_token_estimate,
    };
  }

  /** The canonical JSON the model key hashes. */
  calibrationContractJson(): string {
    return canonicalJson(this.calibrationContract(), { floatKeys: this.runtime.context.floatKeys });
  }

  /** One persisted calibration record: the contract plus the float `scale` and integer `samples`. */
  calibrationRecordJson(scale: number, samples: number): string {
    const floatKeys = new Set([...this.runtime.context.floatKeys, "scale"]);
    return `${canonicalJson({ ...this.calibrationContract(), scale, samples }, { floatKeys })}\n`;
  }

  /** Record files `<key>-*.json`; the key is hexadecimal, so only the literal affixes matter. */
  private records(directory: string, key: string): string[] {
    const prefix = `${key}-`;
    return readdirSync(directory).filter((name) => name.startsWith(prefix) && name.endsWith(".json") && name.length >= prefix.length + 5);
  }

  private loadCalibration(): { scale: number; samples: number } | null {
    if (!this.model || this.calibrationDir === null || !existsSync(this.calibrationDir)) return null;
    let bestScale = 1;
    let samples = 0;
    const contract = this.calibrationContract();
    for (const name of this.records(this.calibrationDir, this.modelKey())) {
      let record: unknown;
      try {
        record = JSON.parse(readFileSync(path.join(this.calibrationDir, name), "utf8"));
      } catch {
        continue;
      }
      if (!isRecord(record)) continue;
      if (Object.entries(contract).some(([key, value]) => record[key] !== value)) continue;
      const scale = record.scale ?? 1;
      const recordSamples = record.samples ?? 0;
      if (typeof scale !== "number" || typeof recordSamples !== "number") continue;
      if (!Number.isFinite(scale) || scale < 1 || !(Math.trunc(recordSamples) > 0)) continue;
      bestScale = Math.max(bestScale, scale);
      samples = Math.max(samples, Math.trunc(recordSamples));
    }
    return samples ? { scale: bestScale, samples } : null;
  }

  private persistCalibration(): void {
    if (!this.model || this.calibrationDir === null) return;
    mkdirSync(this.calibrationDir, { recursive: true });
    const key = this.modelKey();
    const stem = `${key}-${timeNs()}-${process.pid}`;
    const temporary = path.join(this.calibrationDir, `${stem}.tmp`);
    writeFileSync(temporary, this.calibrationRecordJson(this.budgeter.scale, this.budgeter.calibration_samples), "utf8");
    renameSync(temporary, path.join(this.calibrationDir, `${stem}.json`));
    // Bounded, race-tolerant history: independent records keep concurrent writers monotonic; the
    // newest 32 are retained without a cross-platform file lock. Names are ASCII, so UTF-16 order
    // equals code-point order.
    const records = this.records(this.calibrationDir, key).sort();
    for (const stale of records.slice(0, Math.max(records.length - 32, 0))) {
      try {
        unlinkSync(path.join(this.calibrationDir, stale));
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
      }
    }
  }

  fit(messages: Message[]): Message[] {
    const [fitted, compacted] = fitToContext(messages, {
      numCtx: this.runtime.num_ctx,
      numPredict: this.runtime.num_predict,
      policy: this.runtime.context,
      tokenCounter: this.budgeter.estimate,
    });
    this.compactions += compacted ? 1 : 0;
    return fitted;
  }

  observe(messages: Message[], promptEvalCount: unknown): void {
    if (this.budgeter.observe(messages, promptEvalCount)) this.persistCalibration();
  }

  metadata(): Record<string, unknown> {
    return {
      ...this.runtime.context.metadata(),
      ...this.budgeter.metadata(),
      calibration_source: this.calibrationSource,
      calibration_scope: this.calibrationIdentity ? "provided_identity_and_estimator" : "model_tag_and_estimator",
      compaction_target_ratio: COMPACTION_TARGET_RATIO,
      estimated_input_budget: contextBudget(this.runtime.num_ctx, this.runtime.num_predict, this.runtime.context.safety_margin_tokens),
      compactions: this.compactions,
    };
  }
}

/** Write the minimum valid adapter result for a failure before the normal result path. */
export function writeFailureResult(resultPath: string, answer: string): void {
  mkdirSync(path.dirname(resultPath), { recursive: true });
  writeFileSync(resultPath, `${JSON.stringify({ final_answer: answer, tool_calls: [], usage: {} }, null, 2)}\n`, "utf8");
}
