// Compact text for results that carry no `summary`. structuredContent keeps the whole object;
// these lines are what a text-only client reads, so each states the facts an agent acts on.
type Fields = Record<string, unknown>;

const obj = (value: unknown): Fields | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Fields) : undefined;
const list = (value: unknown): unknown[] => (Array.isArray(value) ? value : []);
const num = (value: unknown): number | undefined => (typeof value === "number" && Number.isFinite(value) ? value : undefined);
const str = (value: unknown): string | undefined => (typeof value === "string" && value ? value : undefined);

const gib = (bytes: unknown): string => {
  const n = num(bytes);
  return n === undefined ? "?" : `${(n / 2 ** 30).toFixed(n < 10 * 2 ** 30 ? 1 : 0)} GiB`;
};
const seconds = (ms: unknown): string => {
  const n = num(ms);
  return n === undefined ? "?" : `${Math.round(n / 1000)} s`;
};
const MORE_LIMIT = 12;
const capped = (items: string[], limit = MORE_LIMIT): string =>
  items.length > limit ? `${items.slice(0, limit).join(", ")}, +${items.length - limit} more` : items.join(", ");

/** `doctor view:status`: host, queues, loaded models, jobs and today's usage in a few lines. */
export function statusText(status: Fields): string {
  const host = obj(status.host) ?? {};
  const lines: string[] = [];
  const reasons = list(host.reasons).map(String);
  lines.push(
    `serve ${String(status.status ?? "?")}; host ${String(host.status ?? "?")}${host.holding === true ? " (holding new loads)" : ""}: ` +
      `${gib(host.effective_available_bytes ?? host.available_memory_bytes)} free of ${gib(host.total_memory_bytes)}, ` +
      `memory pressure ${String(host.memory_pressure ?? "?")}, load ${String(host.load_average_one_minute ?? "?")}` +
      (reasons.length > 0 ? `; reasons: ${reasons.join(", ")}` : "") + ".",
  );
  for (const [name, backend] of Object.entries(obj(status.backends) ?? {})) {
    const admission = obj(obj(backend)?.admission);
    if (!admission) continue;
    lines.push(
      `backend ${name}: ${String(admission.in_flight ?? 0)} running, queue ${String(admission.queue_depth ?? 0)}/${String(admission.queue_limit ?? "?")}, ` +
        `${String(admission.slots_available ?? "?")}/${String(admission.slots_total ?? "?")} slots free` +
        (num(admission.resource_waiters) ? `, ${String(admission.resource_waiters)} waiting for memory` : "") + ".",
    );
  }
  const loaded = list(status.loaded_models).map((m) => str(obj(m)?.name ?? obj(m)?.model) ?? String(m));
  lines.push(`loaded models: ${loaded.length === 0 ? "none" : capped(loaded)}.`);
  const jobs = list(obj(status.task_jobs)?.jobs);
  lines.push(`task jobs: ${jobs.length}.`);
  const today = obj(status.usage_today);
  if (today) {
    lines.push(
      `usage today: ${String(today.tasks ?? 0)} tasks, ${String(today.errors ?? 0)} errors, ${String(today.prompt_tokens ?? 0)} prompt / ` +
        `${String(today.output_tokens ?? 0)} output tokens, queue wait ${seconds(today.queue_wait_ms)}.`,
    );
  }
  lines.push("Full object: structuredContent.");
  return lines.join("\n");
}

/** `doctor view:usage`: window totals and the busiest models. */
export function usageText(usage: Fields): string {
  const totals = obj(usage.totals) ?? {};
  const lines = [
    `usage, last ${String(usage.window_days ?? "?")} day(s): ${String(totals.tasks ?? 0)} tasks, ${String(totals.errors ?? 0)} errors, ` +
      `${String(totals.prompt_tokens ?? 0)} prompt / ${String(totals.output_tokens ?? 0)} output tokens, ` +
      `busy ${seconds(totals.busy_ms)}, queue wait ${seconds(totals.queue_wait_ms)}.`,
  ];
  const rows = Object.entries(obj(usage.by_model) ?? {})
    .map(([name, row]) => ({ name, row: obj(row) ?? {} }))
    .sort((a, b) => (num(b.row.tasks) ?? 0) - (num(a.row.tasks) ?? 0));
  for (const { name, row } of rows.slice(0, 10)) {
    lines.push(`${name}: ${String(row.tasks ?? 0)} tasks, ${String(row.errors ?? 0)} errors, ${String(row.prompt_tokens ?? 0)}/${String(row.output_tokens ?? 0)} tokens`);
  }
  if (rows.length > 10) lines.push(`+${rows.length - 10} more models; per day and per model in structuredContent.`);
  return lines.join("\n");
}

/** `doctor view:full`: versions, residency, serve health and the config posture items to act on. */
export function doctorFullText(report: Fields, headline: string): string {
  const lines = [headline];
  const machine = obj(report.machine);
  const chip = str(machine?.chip) ?? str(machine?.cpu_brand) ?? str(machine?.cpu);
  if (machine) lines.push(`machine: ${chip ? `${chip}, ` : ""}${gib(machine.memory_bytes)} memory.`);
  const posture = obj(report.local_conservative_config_posture);
  if (posture) {
    const flagged = Object.entries(obj(posture.settings) ?? {})
      .map(([name, rule]) => ({ name, status: str(obj(rule)?.status) }))
      .filter((entry) => entry.status && entry.status !== "ready")
      .map((entry) => `${entry.name} ${entry.status}`);
    lines.push(`config posture ${String(posture.overall ?? "?")}${flagged.length > 0 ? `: ${flagged.join(", ")}` : ""}.`);
  }
  lines.push("Ollama configuration, environment and admission details: structuredContent.");
  return lines.join("\n");
}

const capabilitiesOf = (entry: Fields): string => list(entry.capabilities).map(String).join("/");

/** `models view:installed`: one line per model, name first. */
export function installedModelsText(data: Fields): string {
  const models = list(data.models).map((m) => obj(m) ?? {});
  if (models.length === 0) return "no installed models.";
  const resident = models.filter((m) => m.resident === true).length;
  const lines = [`${models.length} installed model(s), ${resident} resident:`];
  for (const m of models) {
    const placement = str(obj(m.execution)?.placement);
    lines.push(
      [
        String(m.name ?? "?"), gib(m.size), capabilitiesOf(m) || "-",
        m.resident === true ? "resident" : "",
        placement ? `placement ${placement}` : "",
      ].filter(Boolean).join("  "),
    );
  }
  lines.push("Context, policy rank and benchmark per model: structuredContent; one model: models{view:\"detail\", model}.");
  return lines.join("\n");
}

/** `models view:resident`: what is loaded, where, and how much. */
export function residentModelsText(data: Fields): string {
  const models = list(data.models).map((m) => obj(m) ?? {});
  if (models.length === 0) return "no resident models.";
  const lines = [`${models.length} resident model(s):`];
  for (const m of models) {
    const placement = obj(m.placement);
    lines.push(
      [
        String(m.name ?? "?"), `size ${gib(m.resident_size ?? m.size)}`,
        placement ? `${String(placement.processor ?? "?")} ${String(placement.gpu_percent ?? "?")}% GPU` : "",
        num(m.resident_context) !== undefined ? `ctx ${String(m.resident_context)}` : "",
        str(placement?.warning) ? `warning: ${String(placement?.warning)}` : "",
      ].filter(Boolean).join("  "),
    );
  }
  return lines.join("\n");
}

/** `models view:detail`: the facts of one installed model. */
export function modelDetailText(model: string, data: Fields): string {
  const details = obj(data.details) ?? {};
  const parts = [
    model,
    str(details.family) ?? str(details.architecture),
    str(details.parameter_size),
    str(details.quantization_level),
    num(data.max_context_length) !== undefined ? `max context ${String(data.max_context_length)}` : undefined,
    list(data.capabilities).length > 0 ? `capabilities ${list(data.capabilities).map(String).join("/")}` : undefined,
  ].filter(Boolean);
  return `${parts.join("  ")}\nTemplate, parameters, model_info and knowledge: structuredContent.`;
}

/** `run_task preview:true`: the routing decision in two lines. */
export function routeText(route: Fields): string {
  const execution = obj(route.execution) ?? {};
  const assessment = obj(execution.resource_assessment);
  const lines = [
    `route: ${String(route.task ?? route.profile ?? "?")} -> ${String(route.selected_model ?? "no model")}` +
      `${route.resident === true ? " (resident)" : " (not resident)"}; confidence ${String(route.confidence ?? "?")}, ` +
      `evidence ${String(route.evidence ?? "?")}; context ${String(obj(route.options)?.num_ctx ?? "?")} tokens, ${String(route.context_window_fit ?? "?")}.`,
  ];
  const reasons = list(route.reasons).map(String);
  if (reasons.length > 0) lines.push(`reasons: ${reasons.join(", ")}.`);
  const placement = [str(execution.backend), str(execution.requested_processor)].filter(Boolean).join("/");
  const readiness = str(assessment?.status) ?? str(execution.reason);
  if (placement || readiness) lines.push(`execution: ${[placement, readiness].filter(Boolean).join("; ")}.`);
  const rejected = list(route.rejected).map((r) => str(obj(r)?.model)).filter((m): m is string => m !== undefined);
  if (rejected.length > 0) lines.push(`rejected: ${capped(rejected, 5)}.`);
  lines.push("Nothing was executed or reserved; full decision in structuredContent.");
  return lines.join("\n");
}

/** One acknowledged job: `<job id> <task> <model> queued; poll task_jobs get`. */
function jobAck(job: Fields): string {
  const model = str(job.selected_model) ?? str(job.requested_model) ?? "-";
  return `${String(job.id ?? "?")} ${String(job.task ?? "?")} ${model} ${String(job.status ?? "queued")}; poll task_jobs get`;
}

/** `run_task defer:true`: the single acknowledgement, or one `<item id>: <job id> <status>` line per item. */
export function deferText(payload: Fields): string | undefined {
  const rows = list(payload.jobs);
  if (rows.length > 0) {
    return rows.map((row) => {
      const item = obj(row) ?? {};
      const job = obj(item.job);
      if (job) return `${String(item.id ?? "?")}: ${String(job.id ?? "?")} ${String(job.status ?? "queued")}`;
      const error = obj(item.error);
      return `${String(item.id ?? "?")}: not submitted: ${str(error?.error) ?? JSON.stringify(item.error ?? item)}`;
    }).join("\n") + "\npoll task_jobs get with a job id";
  }
  const job = obj(payload.job);
  return job ? jobAck(job) : undefined;
}

/** A successful structured result with its text replaced; an error result passes through untouched. */
export function withText<T extends { structuredContent?: unknown; isError?: boolean; content: unknown }>(
  result: T, render: (content: Fields) => string | undefined,
): T {
  const content = obj(result.structuredContent);
  if (result.isError || !content) return result;
  const text = render(content);
  return text === undefined ? result : { ...result, content: [{ type: "text" as const, text }] };
}
