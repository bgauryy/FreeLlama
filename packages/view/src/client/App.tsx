import { useState } from "react";
import type { Snapshot } from "../shared/contracts";
import { Badge, Empty, Icon, Panel, type Tone } from "./components";
import { Models } from "./Models";
import { Overview, useHistory } from "./Overview";
import { System } from "./System";
import { Usage } from "./Usage";
import { useRuntime } from "./useRuntime";

const pages = [
  {
    id: "overview",
    label: "Overview",
    title: "Runtime overview",
    description: "A live view of your local inference system.",
  },
  {
    id: "models",
    label: "Models",
    title: "Models & residency",
    description: "Installed inventory and observed runner placement.",
  },
  {
    id: "usage",
    label: "Usage",
    title: "Usage & tokens",
    description: "Task volume and token totals from the usage ledger.",
  },
  {
    id: "system",
    label: "System",
    title: "System & configuration",
    description: "Host details, effective settings, and their sources.",
  },
  {
    id: "diagnostics",
    label: "Diagnostics",
    title: "Runtime diagnostics",
    description: "Source freshness and the complete control API observations.",
  },
] as const;
type PageId = (typeof pages)[number]["id"];

function overall(
  snapshot: Snapshot | null,
  paused: boolean,
  error: string | null,
): { label: string; tone: Tone } {
  if (paused) return { label: "Paused", tone: "neutral" };
  if (error) return { label: "Disconnected", tone: "bad" };
  if (!snapshot) return { label: "Connecting", tone: "neutral" };
  const source = snapshot.sources.status;
  if (source.state === "unavailable")
    return { label: "Unavailable", tone: "bad" };
  if (source.state === "stale")
    return { label: "Stale observation", tone: "warn" };
  if (
    Object.values(source.data.backends).some(
      (b) => b.circuit.state !== "closed",
    ) ||
    source.data.loaded_models.some((m) => "error" in m)
  )
    return { label: "Degraded", tone: "warn" };
  if (source.data.host.holding)
    return { label: "Holding for resources", tone: "warn" };
  if (
    source.data.status !== "ok" ||
    !["ready", "holding"].includes(source.data.host.status) ||
    Object.values(snapshot.sources).some((s) => s.state !== "live")
  )
    return { label: "Partial observations", tone: "warn" };
  return { label: "Live", tone: "good" };
}

function Diagnostics({ snapshot }: { snapshot: Snapshot }) {
  return (
    <>
      <Panel
        title="Observation sources"
        caption="Each source is independently validated and cached"
      >
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Source</th>
                <th>State</th>
                <th>Last observed</th>
                <th>Issue</th>
              </tr>
            </thead>
            <tbody>
              {Object.entries(snapshot.sources).map(([name, source]) => (
                <tr key={name}>
                  <td>
                    <strong>{name}</strong>
                  </td>
                  <td>
                    <Badge
                      tone={
                        source.state === "live"
                          ? "good"
                          : source.state === "stale"
                            ? "warn"
                            : "bad"
                      }
                    >
                      {source.state}
                    </Badge>
                  </td>
                  <td>
                    {source.updated_at
                      ? new Date(source.updated_at).toLocaleString()
                      : "Never"}
                  </td>
                  <td>{source.error ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Panel>
      {Object.entries(snapshot.sources).map(([name, source]) => (
        <details className="panel raw-data" key={name}>
          <summary>
            {name} observation <span>{source.state}</span>
          </summary>
          <pre>{JSON.stringify(source.data, null, 2)}</pre>
        </details>
      ))}
    </>
  );
}

export function App() {
  const [page, setPage] = useState<PageId>(() => {
    const hash = window.location.hash.slice(1);
    return pages.find((p) => p.id === hash)?.id ?? "overview";
  });
  const [paused, setPaused] = useState(false);
  const [revision, setRevision] = useState(0);
  const { snapshot, error, loading } = useRuntime(paused, revision);
  const history = useHistory(snapshot, paused, !!error);
  const current = pages.find((p) => p.id === page)!;
  const state = overall(snapshot, paused, error);
  const issues = snapshot
    ? Object.entries(snapshot.sources).filter(
        ([, source]) => source.state !== "live",
      )
    : [];
  const select = (id: PageId) => {
    setPage(id);
    window.history.replaceState(null, "", `#${id}`);
  };
  return (
    <div className="app-shell">
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <aside className="sidebar">
        <a
          className="brand"
          href="#overview"
          onClick={(e) => {
            e.preventDefault();
            select("overview");
          }}
        >
          <span className="brand-mark">
            <Icon name="models" size={23} />
          </span>
          <span>
            FreeLlama<small>LOCAL CONTROL PLANE</small>
          </span>
        </a>
        <span className="nav-label">WORKSPACE</span>
        <nav aria-label="Runtime views">
          {pages.map((p) => (
            <button
              className={page === p.id ? "active" : ""}
              aria-current={page === p.id ? "page" : undefined}
              key={p.id}
              onClick={() => select(p.id)}
            >
              <Icon name={p.id} />
              <span>{p.label}</span>
              {page === p.id && <span className="nav-indicator" />}
            </button>
          ))}
        </nav>
        <div className="sidebar-bottom">
          <div className="local-label">
            <span className="local-dot" />
            Running locally
          </div>
          <p>
            Your system.
            <br />
            Your models. Your view.
          </p>
          <span className="version">
            {snapshot?.sources.health.data
              ? `FreeLlama v${snapshot.sources.health.data.version}`
              : "FreeLlama runtime view"}
          </span>
        </div>
      </aside>
      <div className="workspace">
        <header className="topbar">
          <div className="breadcrumb">
            Workspace <span>/</span> <strong>{current.label}</strong>
          </div>
          <div className="topbar-right">
            <span className="loopback-label">LOCALHOST</span>
            <Badge tone={state.tone}>{state.label}</Badge>
          </div>
        </header>
        <main id="main">
          <div className="page-heading">
            <div>
              <span className="eyebrow">OBSERVE YOUR RUNTIME</span>
              <h1>{current.title}</h1>
              <p>{current.description}</p>
            </div>
            <div className="page-actions">
              <button
                className="button secondary"
                onClick={() => setPaused((p) => !p)}
              >
                <Icon name={paused ? "play" : "pause"} size={16} />
                {paused ? "Resume" : "Pause"}
              </button>
              <button
                className="button"
                onClick={() => {
                  setPaused(false);
                  setRevision((r) => r + 1);
                }}
              >
                <Icon name="refresh" size={16} />
                Refresh
              </button>
            </div>
          </div>
          <div className="connection-strip">
            <span className={`connection-dot ${state.tone}`} />
            <span>
              {snapshot?.endpoint ?? "Connecting to the local control API"}
            </span>
            <span className="connection-time">
              {paused
                ? "Updates paused"
                : snapshot
                  ? `Observed ${snapshot.sources.status.updated_at ? new Date(snapshot.sources.status.updated_at).toLocaleTimeString() : "never"} · refresh every 2s`
                  : "Waiting for first observation"}
            </span>
          </div>
          {error && (
            <div className="notice bad" role="status">
              {error}
            </div>
          )}
          {issues.length > 0 && (
            <div className="notice" role="status">
              <span>
                {issues.length} observation source
                {issues.length === 1 ? "" : "s"} need attention:{" "}
                {issues.map(([name]) => name).join(", ")}
                {paused
                  ? ". Updates are paused."
                  : ". Reconnecting automatically."}
              </span>
              <button onClick={() => select("diagnostics")}>
                Inspect sources <Icon name="arrow" size={14} />
              </button>
            </div>
          )}
          {snapshot ? (
            page === "overview" ? (
              <Overview snapshot={snapshot} history={history} />
            ) : page === "models" ? (
              <Models snapshot={snapshot} />
            ) : page === "usage" ? (
              <Usage snapshot={snapshot} />
            ) : page === "system" ? (
              <System snapshot={snapshot} />
            ) : (
              <Diagnostics snapshot={snapshot} />
            )
          ) : (
            <Panel
              title={
                loading
                  ? "Connecting to FreeLlama"
                  : "Waiting for the view server"
              }
            >
              <Empty
                title={
                  loading
                    ? "Gathering the first observations"
                    : "Connection unavailable"
                }
              >
                {loading
                  ? "Reading runtime status, inventory, and configuration."
                  : "This page will reconnect automatically when the local view server becomes available."}
              </Empty>
            </Panel>
          )}
          <footer>
            <span>FreeLlama / Runtime view</span>
            <span>Local observations · read-only</span>
          </footer>
        </main>
      </div>
    </div>
  );
}
