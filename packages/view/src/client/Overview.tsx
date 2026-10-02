import { useEffect, useState } from "react";
import type { Backend, Snapshot, RuntimeStatus } from "../shared/contracts";
import {
  Badge,
  Empty,
  Meter,
  Panel,
  Row,
  SourceNote,
  Stat,
} from "./components";
import { bytes, duration, number } from "./format";
import { LoadedModels } from "./Models";

function BackendCard({ name, backend }: { name: string; backend: Backend }) {
  const a = backend.admission;
  const tone =
    backend.circuit.state === "open"
      ? "bad"
      : backend.circuit.state !== "closed" || a.queue_depth > 0
        ? "warn"
        : "good";
  return (
    <Panel
      title={
        name === "gpu"
          ? "Primary backend"
          : name === "cpu"
            ? "CPU backend"
            : name
      }
      caption={backend.upstream}
      action={
        <Badge tone={tone}>
          {backend.circuit.state === "closed"
            ? "Accepting"
            : backend.circuit.state.replaceAll("_", " ")}
        </Badge>
      }
    >
      <div className="capacity">
        <strong>
          {a.active_units}
          <span> / {a.slots_total}</span>
        </strong>
        <span>cost units in use</span>
      </div>
      <Meter
        used={a.active_units}
        total={a.slots_total}
        label={`${name} cost units in use`}
        tone={tone}
      />
      <div className="backend-facts">
        <div>
          <strong>{a.in_flight}</strong>
          <span>in flight</span>
        </div>
        <div>
          <strong>{a.queue_depth}</strong>
          <span>queued</span>
        </div>
        <div>
          <strong>{a.slots_available}</strong>
          <span>units available</span>
        </div>
      </div>
      <Row
        label="Slot / resource waiters"
        value={`${number(a.slot_waiters)} / ${number(a.resource_waiters)}`}
      />
      <Row
        label="Oldest wait"
        value={
          a.oldest_wait_ms == null ? "No waiters" : duration(a.oldest_wait_ms)
        }
      />
      <Row
        label="Queue full / timed out"
        value={`${a.queue_full_rejections} / ${a.queue_timeouts}`}
      />
      <Row
        label="Adaptive limit"
        value={
          backend.adaptive.enabled
            ? `${backend.adaptive.limit} / ${backend.adaptive.ceiling}`
            : "Off"
        }
      />
      {backend.adaptive.last_decrease_reason && (
        <Row
          label="Last limit decrease"
          value={backend.adaptive.last_decrease_reason.replaceAll("_", " ")}
        />
      )}
      {backend.circuit.retry_after_seconds != null && (
        <Row
          label="Circuit retry"
          value={duration(backend.circuit.retry_after_seconds * 1000)}
        />
      )}
    </Panel>
  );
}

type Sample = { time: number; active: number; queued: number };
export function useHistory(
  snapshot: Snapshot | null,
  paused: boolean,
  disconnected: boolean,
) {
  const [history, setHistory] = useState<Sample[]>([]);
  const source = snapshot?.sources.status;
  useEffect(() => {
    if (!source || source.state !== "live" || paused || disconnected) return;
    const time = Date.parse(source.updated_at);
    const backends = Object.values(source.data.backends);
    const next = {
      time,
      active: backends.reduce((n, b) => n + b.admission.in_flight, 0),
      queued: backends.reduce((n, b) => n + b.admission.queue_depth, 0),
    };
    setHistory((previous) =>
      previous.at(-1)?.time === time
        ? previous
        : [...previous.filter((s) => s.time > time - 120_000), next].slice(-60),
    );
  }, [source, paused, disconnected]);
  return history;
}

function Activity({ history }: { history: Sample[] }) {
  const maximum = Math.max(2, ...history.flatMap((s) => [s.active, s.queued]));
  const first = history[0];
  const last = history.at(-1);
  const span = first && last ? Math.max(1, last.time - first.time) : 1;
  const line = (key: "active" | "queued") =>
    history
      .map(
        (s) =>
          `${((s.time - (first?.time ?? s.time)) / span) * 620 + 10},${118 - (s[key] / maximum) * 96}`,
      )
      .join(" ");
  return (
    <Panel
      title="Recent activity"
      caption="Managed tasks observed in this browser · up to 2 minutes"
      action={
        <div className="chart-legend">
          <span className="legend-live">In flight</span>
          <span className="legend-wait">Queued</span>
        </div>
      }
    >
      {history.length < 2 ? (
        <Empty title="Collecting observations">
          The activity chart appears after two live readings.
        </Empty>
      ) : (
        <>
          <div className="activity-chart">
            <span className="axis-top">{maximum}</span>
            <span className="axis-zero">0</span>
            <svg
              viewBox="0 0 640 140"
              preserveAspectRatio="none"
              role="img"
              aria-label={`Recent activity: ${last?.active} tasks in flight, ${last?.queued} queued`}
            >
              {[22, 70, 118].map((y) => (
                <line
                  key={y}
                  x1="10"
                  x2="630"
                  y1={y}
                  y2={y}
                  className="grid-line"
                />
              ))}
              <polyline points={line("queued")} className="queue-line" />
              <polyline points={line("active")} className="active-line" />
            </svg>
          </div>
          <div className="chart-axis">
            <span>{new Date(first!.time).toLocaleTimeString()}</span>
            <span>{new Date(last!.time).toLocaleTimeString()}</span>
          </div>
        </>
      )}
    </Panel>
  );
}

function Host({ status }: { status: RuntimeStatus }) {
  const h = status.host;
  const used =
    h.total_memory_bytes == null || h.available_memory_bytes == null
      ? null
      : Math.max(0, h.total_memory_bytes - h.available_memory_bytes);
  const gpuUsed =
    h.gpu_memory_total_bytes == null || h.gpu_memory_free_bytes == null
      ? null
      : Math.max(0, h.gpu_memory_total_bytes - h.gpu_memory_free_bytes);
  return (
    <Panel
      title="Memory & host"
      caption={`Telemetry age ${duration(h.sample_age_ms)}`}
      action={
        <Badge
          tone={
            h.holding ? "warn" : h.status === "unknown" ? "neutral" : "good"
          }
        >
          {h.holding ? "Holding" : h.status}
        </Badge>
      }
    >
      <div className="memory-label">
        <span>Host memory used</span>
        <strong>
          {bytes(used)} <span>/ {bytes(h.total_memory_bytes)}</span>
        </strong>
      </div>
      <Meter
        used={used}
        total={h.total_memory_bytes}
        label="Host memory used"
        tone={h.holding ? "warn" : "good"}
      />
      <Row
        label="Available after reservations"
        value={bytes(h.effective_available_bytes)}
      />
      <Row label="Reserved for loads" value={bytes(h.reserved_bytes)} />
      <Row label="Memory pressure" value={h.memory_pressure ?? "Unknown"} />
      <Row
        label="Load average · 1 min"
        value={
          h.load_average_one_minute == null
            ? "Unknown"
            : `${h.load_average_one_minute.toFixed(2)} / ${number(h.logical_cpus)} logical CPUs`
        }
      />
      <Row
        label="Thermal throttling"
        value={
          h.thermal_throttled == null
            ? "Unknown"
            : h.thermal_throttled
              ? "Yes"
              : "No"
        }
      />
      <div className="memory-label second">
        <span>Discrete GPU memory used</span>
        <strong>
          {bytes(gpuUsed)} <span>/ {bytes(h.gpu_memory_total_bytes)}</span>
        </strong>
      </div>
      <Meter
        used={gpuUsed}
        total={h.gpu_memory_total_bytes}
        label="Discrete GPU memory used"
      />
      {h.gpu_telemetry_source ? (
        <p className="muted small">Source: {h.gpu_telemetry_source}</p>
      ) : (
        <p className="muted small">
          Discrete GPU telemetry is unavailable. Runner residency is shown
          separately.
        </p>
      )}
      {h.reasons.map((reason) => (
        <p className="warning-text small" key={reason}>
          {reason.replaceAll("_", " ")}
        </p>
      ))}
    </Panel>
  );
}

export function Overview({
  snapshot,
  history,
}: {
  snapshot: Snapshot;
  history: Sample[];
}) {
  const source = snapshot.sources.status;
  const status = source.data;
  const backends = Object.entries(status?.backends ?? {});
  const active = status
    ? backends.reduce((n, [, b]) => n + b.admission.in_flight, 0)
    : null;
  const queued = status
    ? backends.reduce((n, [, b]) => n + b.admission.queue_depth, 0)
    : null;
  return (
    <>
      <SourceNote source={source} />
      <div className="stats-grid">
        <Stat
          label="In flight"
          value={number(active)}
          detail="Managed tasks admitted"
        />
        <Stat
          label="Waiting"
          value={number(queued)}
          detail="Slot and resource queues"
        />
        <Stat
          label="Available memory"
          value={bytes(status?.host.effective_available_bytes)}
          detail="Host RAM after reservations"
        />
        <Stat
          label="Tokens today"
          value={
            status
              ? number(
                  status.usage_today.prompt_tokens +
                    status.usage_today.output_tokens,
                )
              : "Unknown"
          }
          detail="Prompt + output · since 00:00 UTC"
        />
      </div>
      {status ? (
        <>
          <div className="section-label">
            <span>Execution backends</span>
            <span>{backends.length} configured</span>
          </div>
          <div className="backend-grid">
            {backends.map(([name, backend]) => (
              <BackendCard key={name} name={name} backend={backend} />
            ))}
            <Panel
              title="Raw Ollama traffic"
              caption="Compatibility proxy"
              action={
                <Badge tone={status.raw_proxy.waiting > 0 ? "warn" : "neutral"}>
                  {status.raw_proxy.waiting > 0 ? "Waiting" : "Pass-through"}
                </Badge>
              }
            >
              <div className="capacity">
                <strong>
                  {status.raw_proxy.active}
                  <span> / {status.raw_proxy.limit}</span>
                </strong>
                <span>active streams</span>
              </div>
              <Meter
                used={status.raw_proxy.active}
                total={status.raw_proxy.limit}
                label="Raw streams in use"
                tone="neutral"
              />
              <Row label="Waiting" value={status.raw_proxy.waiting} />
              <Row label="Admitted" value={number(status.raw_proxy.admitted)} />
              <Row label="Refused" value={number(status.raw_proxy.rejected)} />
              <Row
                label="Wait budget"
                value={duration(status.raw_proxy.queue_wait_seconds * 1000)}
              />
              <p className="muted small proxy-note">
                Raw streams have a separate admission cap and use the primary
                upstream.
              </p>
            </Panel>
          </div>
          <div className="overview-middle">
            <Activity history={history} />
            <Host status={status} />
          </div>
          <LoadedModels source={source} />
          {status.task_jobs && (
            <Panel
              title="Deferred task jobs"
              caption={`Registry scope: ${status.task_jobs.scope}`}
              action={<Badge>{status.task_jobs.jobs.length} tracked</Badge>}
            >
              {status.task_jobs.jobs.length === 0 ? (
                <p className="muted small">
                  No deferred jobs are tracked in this process.
                </p>
              ) : (
                <div className="table-wrap">
                  <table>
                    <thead>
                      <tr>
                        <th>Task / ID</th>
                        <th>State</th>
                        <th>Model / backend</th>
                        <th>Priority</th>
                        <th>Reason</th>
                        <th>Deadline</th>
                      </tr>
                    </thead>
                    <tbody>
                      {status.task_jobs.jobs.map((job) => (
                        <tr key={job.id}>
                          <td>
                            {job.task}
                            <span className="table-sub">
                              {job.id.slice(0, 12)}
                            </span>
                          </td>
                          <td>
                            <Badge
                              tone={
                                job.status === "failed" ||
                                job.status === "expired"
                                  ? "bad"
                                  : job.status === "running" ||
                                      job.status === "completed"
                                    ? "good"
                                    : "neutral"
                              }
                            >
                              {job.status.replaceAll("_", " ")}
                            </Badge>
                          </td>
                          <td>
                            {job.selected_model ??
                              job.requested_model ??
                              "Pending route"}
                            <span className="table-sub">
                              {job.backend ?? "Unassigned"}
                            </span>
                          </td>
                          <td>{job.priority}</td>
                          <td>{job.reason.replaceAll("_", " ")}</td>
                          <td>
                            {new Date(
                              job.deadline_at * 1000,
                            ).toLocaleTimeString()}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </Panel>
          )}
          <Panel
            title="Residency & scheduling"
            caption="The latest eviction decision"
          >
            <div className="scheduling-summary">
              <Badge tone="neutral">
                {status.scheduling.evict_idle_models
                  ? "Idle eviction enabled"
                  : "Idle eviction off"}
              </Badge>
              <span>
                {status.scheduling.measured_footprints} measured footprints
              </span>
            </div>
            {status.scheduling.last_eviction ? (
              <div className="eviction">
                <Row
                  label="Loading"
                  value={status.scheduling.last_eviction.for_model}
                />
                <Row
                  label="Memory needed"
                  value={bytes(status.scheduling.last_eviction.shortfall_bytes)}
                />
                <Row
                  label="Plan covers shortfall"
                  value={
                    status.scheduling.last_eviction.covers_shortfall
                      ? "Yes"
                      : "No"
                  }
                />
                {status.scheduling.last_eviction.unloaded.map((m) => (
                  <Row
                    key={m.model}
                    label={`Unloaded ${m.model}`}
                    value={`${bytes(m.freed_bytes)} · reload ~${duration(m.reload_seconds * 1000)}`}
                  />
                ))}
                {status.scheduling.last_eviction.kept.map((m) => (
                  <Row
                    key={m.model}
                    label={`Kept ${m.model}`}
                    value={m.reason.replaceAll("_", " ")}
                  />
                ))}
              </div>
            ) : (
              <p className="muted">
                No eviction recorded. Idle runners are considered when a new
                load needs memory.
              </p>
            )}
          </Panel>
        </>
      ) : (
        <Panel title="Waiting for FreeLlama">
          <Empty title="The control API is unavailable">
            Start FreeLlama with the serve command. This view will reconnect
            automatically.
          </Empty>
        </Panel>
      )}
    </>
  );
}
