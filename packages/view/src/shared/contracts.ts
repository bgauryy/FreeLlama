import { z } from "zod";

// The Rust control API owns the wire contract. Validate every field the view consumes,
// preserve additive fields, and leave absent telemetry unknown rather than inventing zeroes.
const count = z.number().finite().nonnegative();
const maybeNumber = count.nullish();
const object = z.object({}).passthrough();
const setting = z
  .object({ value: z.unknown(), source: z.string() })
  .passthrough();
export const usageTotalsSchema = z
  .object({
    tasks: count,
    errors: count,
    prompt_tokens: count,
    output_tokens: count,
    busy_ms: count,
    queue_wait_ms: count,
  })
  .passthrough();

const admissionSchema = z
  .object({
    slots_total: count,
    slots_ceiling: count,
    slots_available: count,
    active_units: count,
    in_flight: count,
    queue_depth: count,
    queue_limit: count,
    slot_waiters: maybeNumber,
    resource_waiters: maybeNumber,
    oldest_wait_ms: maybeNumber,
    admitted: maybeNumber,
    released: maybeNumber,
    queue_full_rejections: count,
    queue_timeouts: count,
    queue_cancellations: maybeNumber,
    transition_timeouts: maybeNumber,
  })
  .passthrough();
export const backendSchema = z
  .object({
    upstream: z.string(),
    admission: admissionSchema,
    adaptive: z
      .object({
        enabled: z.boolean(),
        limit: count,
        ceiling: count,
        healthy_streak: count,
        last_decrease_reason: z.string().nullable(),
        baseline_output_tokens_per_second: z
          .record(z.object({ tps: count, samples: count }))
          .optional(),
      })
      .passthrough(),
    circuit: z
      .object({
        state: z.string(),
        retry_after_seconds: maybeNumber,
        consecutive_failures: maybeNumber,
        times_opened: maybeNumber,
      })
      .passthrough(),
  })
  .passthrough();

export const runnerSchema = z
  .object({
    backend: z.string(),
    name: z.string(),
    size_bytes: count,
    size_vram_bytes: count,
    gpu_percent: count,
    context_length: maybeNumber,
    expires_at: z.string().nullish(),
    pinned: z.boolean(),
    active_tasks: count,
    recent_uses: count,
    idle_seconds: maybeNumber,
    measured_load_seconds: maybeNumber,
  })
  .passthrough();
const runnerErrorSchema = z
  .object({ backend: z.string(), error: z.string() })
  .passthrough();

const evictionSchema = z
  .object({
    for_model: z.string(),
    backend: z.string(),
    memory: z.string(),
    shortfall_bytes: count,
    covers_shortfall: z.boolean(),
    unloaded: z.array(
      z
        .object({
          model: z.string(),
          freed_bytes: count,
          reload_seconds: count,
        })
        .passthrough(),
    ),
    kept: z.array(
      z.object({ model: z.string(), reason: z.string() }).passthrough(),
    ),
  })
  .passthrough();
const reloadSchema = z
  .object({
    reloads: count,
    last_error: z.string().nullable(),
    last_reload_unix: maybeNumber,
  })
  .passthrough();
const ollamaConfigSchema = z
  .object({
    settings: z.record(setting),
    process_inspection: z.string(),
    effective: object.optional(),
  })
  .passthrough();
export const statusSchema = z
  .object({
    status: z.string(),
    backends: z.record(backendSchema),
    raw_proxy: z
      .object({
        limit: count,
        active: count,
        waiting: count,
        queue_wait_seconds: count,
        admitted: count,
        rejected: count,
      })
      .passthrough(),
    loaded_models: z.array(z.union([runnerSchema, runnerErrorSchema])),
    task_jobs: z
      .object({
        scope: z.string(),
        jobs: z.array(
          z
            .object({
              id: z.string(),
              status: z.string(),
              task: z.string(),
              priority: z.string(),
              requested_model: z.string().nullable(),
              selected_model: z.string().nullable(),
              backend: z.string().nullable(),
              reason: z.string(),
              created_at: count,
              deadline_at: count,
              timeout_seconds: count,
              detail: z.unknown(),
            })
            .passthrough(),
        ),
      })
      .passthrough()
      .optional(),
    host: z
      .object({
        status: z.string(),
        holding: z.boolean(),
        reasons: z.array(z.string()),
        total_memory_bytes: maybeNumber,
        available_memory_bytes: maybeNumber,
        effective_available_bytes: maybeNumber,
        reserved_bytes: count,
        memory_pressure: z.string().nullish(),
        memory_psi_some_avg10: maybeNumber,
        cgroup_memory_limit_bytes: maybeNumber,
        load_average_one_minute: maybeNumber,
        logical_cpus: maybeNumber,
        thermal_throttled: z.boolean().nullish(),
        gpu_memory_total_bytes: maybeNumber,
        gpu_memory_free_bytes: maybeNumber,
        gpu_telemetry_source: z.string().nullish(),
        sample_age_ms: count,
      })
      .passthrough(),
    ollama: z
      .object({
        config: ollamaConfigSchema,
        cpu_config: ollamaConfigSchema.nullish(),
        default_context: z
          .object({ tokens: count, source: z.string() })
          .nullable(),
        context_mode: z.string(),
      })
      .passthrough(),
    scheduling: z
      .object({
        last_eviction: evictionSchema.nullable(),
        measured_footprints: count,
        evict_idle_models: z.boolean(),
        division_of_work: z.record(z.string()),
      })
      .passthrough(),
    usage_today: usageTotalsSchema,
    usage_ledger: z
      .object({
        enabled: z.boolean(),
        path: z.string().nullable(),
        records: count,
        last_error: z.string().nullable(),
      })
      .passthrough(),
    runtime_config: z
      .object({ file: z.string().nullable(), reload: reloadSchema })
      .passthrough(),
  })
  .passthrough();

export const healthSchema = z
  .object({
    status: z.string(),
    version: z.string(),
    contracts: z.record(z.string()).optional(),
    sessions: z
      .object({ active: count, max_sessions: count, idle_ttl_seconds: count })
      .passthrough(),
    security: z
      .object({ authentication: z.string(), remote_access: z.boolean() })
      .passthrough(),
    feedback: object.optional(),
  })
  .passthrough();
export const machineSchema = z
  .object({
    os: z.string(),
    architecture: z.string(),
    chip: z.string().nullable(),
    logical_cpus: count,
    memory_bytes: maybeNumber,
    memory_kind: z.string(),
    unified_memory_bytes: maybeNumber,
    available_disk_bytes: maybeNumber,
    ollama_endpoint: z.string(),
  })
  .passthrough();
export const inventorySchema = z
  .object({
    models: z.array(
      z
        .object({
          name: z.string(),
          size: count,
          capabilities: z.array(z.string()),
          advertised_context: maybeNumber,
          resident: z.boolean(),
          resident_vram: maybeNumber,
          model_type: z.string(),
          execution: z
            .object({
              placement: z.string(),
              backend: z.string(),
              upstream: z.string(),
              observation: object,
            })
            .passthrough(),
          benchmark: z.record(count),
          policy_rank: z.record(count),
        })
        .passthrough(),
    ),
  })
  .passthrough();
export const usageSchema = z
  .object({
    window_days: count,
    totals: usageTotalsSchema,
    by_model: z.record(usageTotalsSchema),
    by_day: z.array(
      z.object({ day: z.string(), models: z.record(usageTotalsSchema) }),
    ),
    ledger: z
      .object({
        path: z.string().nullable(),
        records: count,
        last_error: z.string().nullable(),
      })
      .passthrough(),
  })
  .passthrough();
export const configSchema = z
  .object({
    file: z.string().nullable(),
    precedence: z.array(z.string()),
    reload: reloadSchema,
    settings: z.record(setting),
    ollama: object.optional(),
  })
  .passthrough();

export function sourceSchema<T extends z.ZodTypeAny>(data: T) {
  return z.discriminatedUnion("state", [
    z.object({
      state: z.enum(["live", "stale"]),
      data,
      updated_at: z.string().datetime(),
      error: z.string().nullable(),
    }),
    z.object({
      state: z.literal("unavailable"),
      data: z.null(),
      updated_at: z.null(),
      error: z.string(),
    }),
  ]);
}

export const snapshotSchema = z.object({
  schema_version: z.literal(1),
  collected_at: z.string().datetime(),
  endpoint: z.string(),
  poll_interval_ms: count,
  sources: z.object({
    status: sourceSchema(statusSchema),
    health: sourceSchema(healthSchema),
    machine: sourceSchema(machineSchema),
    models: sourceSchema(inventorySchema),
    usage: sourceSchema(usageSchema),
    config: sourceSchema(configSchema),
  }),
});
export type Snapshot = z.infer<typeof snapshotSchema>;
export type RuntimeStatus = z.infer<typeof statusSchema>;
export type Backend = z.infer<typeof backendSchema>;
export type Runner = z.infer<typeof runnerSchema>;
export type UsageTotals = z.infer<typeof usageTotalsSchema>;
export type Source<T> =
  | {
      state: "live" | "stale";
      data: T;
      updated_at: string;
      error: string | null;
    }
  | { state: "unavailable"; data: null; updated_at: null; error: string };
