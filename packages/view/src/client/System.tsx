import type { Snapshot } from "../shared/contracts";
import { Badge, Panel, Row, SourceNote } from "./components";
import { bytes, display, duration, number } from "./format";

export function System({ snapshot }: { snapshot: Snapshot }) {
  const { machine, health, config, status } = snapshot.sources;
  const host = machine.data;
  return (
    <>
      <div className="two-columns">
        <Panel title="Machine" caption="Portable host profile">
          <SourceNote source={machine} />
          <Row label="Chip" value={host?.chip ?? "Unknown"} />
          <Row label="Operating system" value={host?.os ?? "Unknown"} />
          <Row label="Architecture" value={host?.architecture ?? "Unknown"} />
          <Row label="Logical CPUs" value={number(host?.logical_cpus)} />
          <Row label="Total RAM" value={bytes(host?.memory_bytes)} />
          <Row label="Memory kind" value={host?.memory_kind ?? "Unknown"} />
          <Row
            label="Available disk"
            value={bytes(host?.available_disk_bytes)}
          />
          <Row
            label="Ollama endpoint"
            value={host?.ollama_endpoint ?? "Unknown"}
          />
        </Panel>
        <Panel title="Control plane" caption="Service, sessions, and security">
          <SourceNote source={health} />
          <Row
            label="FreeLlama version"
            value={health.data?.version ?? "Unknown"}
          />
          <Row
            label="Service status"
            value={health.data?.status ?? "Unknown"}
          />
          <Row
            label="Active affinity sessions"
            value={number(health.data?.sessions.active)}
          />
          <Row
            label="Session capacity"
            value={number(health.data?.sessions.max_sessions)}
          />
          <Row
            label="Idle session TTL"
            value={
              health.data
                ? duration(health.data.sessions.idle_ttl_seconds * 1000)
                : "Unknown"
            }
          />
          <Row
            label="Control API authentication"
            value={health.data?.security.authentication ?? "Unknown"}
          />
          <Row
            label="Control API remote access"
            value={
              health.data
                ? health.data.security.remote_access
                  ? "Enabled"
                  : "Disabled"
                : "Unknown"
            }
          />
          <p className="muted small">
            The runtime view listens on loopback. Affinity sessions hold routing
            metadata.
          </p>
        </Panel>
      </div>
      <Panel
        title="Runtime settings"
        caption="Effective values and their sources"
        action={<Badge>Read-only</Badge>}
      >
        <SourceNote source={config} />
        <Row
          label="Runtime file"
          value={
            config.data ? (config.data.file ?? "No file configured") : "Unknown"
          }
        />
        <Row
          label="Precedence"
          value={config.data?.precedence.join(" → ") ?? "Unknown"}
        />
        <Row
          label="Successful reloads"
          value={number(config.data?.reload.reloads)}
        />
        {config.data?.reload.last_error && (
          <div className="inline-alert">
            Last reload: {config.data.reload.last_error}
          </div>
        )}
        {config.data && (
          <div className="table-wrap settings-table">
            <table>
              <thead>
                <tr>
                  <th>Setting</th>
                  <th>Value</th>
                  <th>Source</th>
                </tr>
              </thead>
              <tbody>
                {Object.entries(config.data.settings)
                  .sort(([a], [b]) => a.localeCompare(b))
                  .map(([name, setting]) => (
                    <tr key={name}>
                      <td>{name}</td>
                      <td className="setting-value">
                        {display(setting.value)}
                      </td>
                      <td>
                        <Badge>{setting.source}</Badge>
                      </td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
      <Panel
        title="Ollama settings"
        caption="Probed values, inherited hints, and defaults"
      >
        <SourceNote source={status} />
        {status.data && (
          <>
            <Row label="Context mode" value={status.data.ollama.context_mode} />
            <Row
              label="Default context"
              value={
                status.data.ollama.default_context
                  ? `${number(status.data.ollama.default_context.tokens)} · ${status.data.ollama.default_context.source}`
                  : "Unknown"
              }
            />
            <div className="table-wrap settings-table">
              <table>
                <thead>
                  <tr>
                    <th>Setting</th>
                    <th>Value</th>
                    <th>Source</th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(status.data.ollama.config.settings).map(
                    ([name, setting]) => (
                      <tr key={name}>
                        <td>{name}</td>
                        <td>
                          {setting.value == null
                            ? "Unset / Ollama default"
                            : display(setting.value)}
                        </td>
                        <td>
                          <Badge>{setting.source.replaceAll("_", " ")}</Badge>
                        </td>
                      </tr>
                    ),
                  )}
                </tbody>
              </table>
            </div>
            <p className="muted small">
              {status.data.ollama.config.process_inspection}
            </p>
          </>
        )}
      </Panel>
      {status.data?.ollama.cpu_config && (
        <Panel
          title="CPU Ollama settings"
          caption="Settings observed on the optional CPU upstream"
        >
          <div className="table-wrap settings-table">
            <table>
              <thead>
                <tr>
                  <th>Setting</th>
                  <th>Value</th>
                  <th>Source</th>
                </tr>
              </thead>
              <tbody>
                {Object.entries(status.data.ollama.cpu_config.settings).map(
                  ([name, setting]) => (
                    <tr key={name}>
                      <td>{name}</td>
                      <td>
                        {setting.value == null
                          ? "Unset / Ollama default"
                          : display(setting.value)}
                      </td>
                      <td>
                        <Badge>{setting.source.replaceAll("_", " ")}</Badge>
                      </td>
                    </tr>
                  ),
                )}
              </tbody>
            </table>
          </div>
          <p className="muted small">
            {status.data.ollama.cpu_config.process_inspection}
          </p>
        </Panel>
      )}
    </>
  );
}
