import { useState } from "react";
import type {
  Runner,
  RuntimeStatus,
  Snapshot,
  Source,
} from "../shared/contracts";
import { Badge, Empty, Icon, Meter, Panel, SourceNote } from "./components";
import { bytes, duration, number } from "./format";

export function LoadedModels({
  source,
  query = "",
}: {
  source: Source<RuntimeStatus>;
  query?: string;
}) {
  const rows = source.data?.loaded_models ?? [];
  const errors = rows.filter(
    (row) => "error" in row && typeof row.error === "string",
  );
  const runners = rows.filter(
    (row): row is Runner => "name" in row && typeof row.name === "string",
  );
  const visibleRunners = runners.filter((row) =>
    row.name.toLowerCase().includes(query.toLowerCase()),
  );
  return (
    <Panel
      title="Loaded models"
      caption="Observed runner memory from Ollama /api/ps"
      action={
        <Badge tone={errors.length ? "warn" : "neutral"}>
          {!source.data
            ? "Residency unknown"
            : errors.length
              ? `${runners.length} observed · partial`
              : `${runners.length} resident`}
        </Badge>
      }
    >
      {errors.map((row, i) => (
        <div className="inline-alert" key={i}>
          {row.backend}: {String(row.error)}
        </div>
      ))}
      {visibleRunners.length === 0 ? (
        <Empty
          title={
            !source.data || errors.length
              ? "Residency is unknown"
              : query
                ? "No matching loaded models"
                : "No loaded models"
          }
        >
          {!source.data || errors.length
            ? "One or more upstreams could not report their resident runners."
            : query
              ? "Try another model name."
              : "Loaded runners will appear here when Ollama reports them."}
        </Empty>
      ) : (
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Model / backend</th>
                <th>Runner memory</th>
                <th>GPU memory share</th>
                <th>Context</th>
                <th>Demand</th>
                <th>Load time</th>
                <th>Idle / expiry</th>
              </tr>
            </thead>
            <tbody>
              {visibleRunners.map((m, i) => {
                return (
                  <tr key={`${m.backend}:${m.name}:${i}`}>
                    <td>
                      <strong className="model-name">{m.name}</strong>
                      <span className="table-sub">
                        {m.backend === "gpu" ? "primary" : m.backend}
                        {m.pinned ? " · pinned" : ""}
                      </span>
                    </td>
                    <td>{bytes(m.size_bytes)}</td>
                    <td>
                      <div className="gpu-share">
                        <span>
                          {m.size_bytes > 0 ? `${m.gpu_percent}%` : "Unknown"}
                        </span>
                        <Meter
                          used={m.size_vram_bytes}
                          total={m.size_bytes}
                          label={`${m.name} GPU memory share`}
                        />
                      </div>
                      <span className="table-sub">
                        {bytes(m.size_vram_bytes)} reported
                      </span>
                    </td>
                    <td>{number(m.context_length)}</td>
                    <td>
                      <Badge tone={m.active_tasks > 0 ? "good" : "neutral"}>
                        {m.active_tasks} tasks
                      </Badge>
                    </td>
                    <td>
                      {m.measured_load_seconds == null
                        ? "Unmeasured"
                        : duration(m.measured_load_seconds * 1000)}
                    </td>
                    <td>
                      {m.idle_seconds == null
                        ? "Unknown"
                        : duration(m.idle_seconds * 1000)}
                      <span className="table-sub">
                        {m.expires_at
                          ? `Expires ${new Date(m.expires_at).toLocaleTimeString()}`
                          : "Expiry unknown"}
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      <p className="table-footnote">
        Backend assignment is configured intent. The GPU share above is observed
        memory residency, not processor utilization.
      </p>
    </Panel>
  );
}

export function Models({ snapshot }: { snapshot: Snapshot }) {
  const [query, setQuery] = useState("");
  const [residentOnly, setResidentOnly] = useState(false);
  const source = snapshot.sources.models;
  const models =
    source.data?.models.filter(
      (model) =>
        model.name.toLowerCase().includes(query.toLowerCase()) &&
        (!residentOnly || model.resident),
    ) ?? [];
  return (
    <>
      <div className="filters">
        <label className="search">
          <Icon name="search" size={18} />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search models…"
            aria-label="Search models"
          />
        </label>
        <label className="check-filter">
          <input
            type="checkbox"
            checked={residentOnly}
            onChange={(e) => setResidentOnly(e.target.checked)}
          />
          Resident inventory only
        </label>
      </div>
      <SourceNote source={snapshot.sources.status} />
      <LoadedModels source={snapshot.sources.status} query={query} />
      <SourceNote source={source} />
      <Panel
        title="Installed inventory"
        caption="Catalog refreshed every 30 seconds · configured placement is shown separately from residency"
        action={
          <Badge>{source.data?.models.length ?? "Unknown"} installed</Badge>
        }
      >
        {models.length === 0 ? (
          <Empty
            title={source.data ? "No matching models" : "Inventory unavailable"}
          >
            {source.data
              ? "Change the filter or inspect the installed model catalog in FreeLlama."
              : "The catalog appears when the control API can discover installed models."}
          </Empty>
        ) : (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Model</th>
                  <th>Disk size</th>
                  <th>Capabilities</th>
                  <th>Advertised context</th>
                  <th>Assigned backend</th>
                  <th>Catalog residency</th>
                </tr>
              </thead>
              <tbody>
                {models.map((m) => (
                  <tr key={`${m.execution.backend}:${m.name}`}>
                    <td>
                      <strong className="model-name">{m.name}</strong>
                      <span className="table-sub">
                        {m.model_type.replaceAll("_", " ")}
                      </span>
                    </td>
                    <td>{bytes(m.size)}</td>
                    <td>
                      <div className="capabilities">
                        {m.capabilities.map((c) => (
                          <span key={c}>{c}</span>
                        ))}
                      </div>
                    </td>
                    <td>{number(m.advertised_context)}</td>
                    <td>
                      {m.execution.backend}
                      <span className="table-sub">
                        Intent: {m.execution.placement}
                      </span>
                    </td>
                    <td>
                      <Badge tone={m.resident ? "good" : "neutral"}>
                        {m.resident ? "Resident" : "Unloaded"}
                      </Badge>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </>
  );
}
