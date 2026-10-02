import type { Snapshot, UsageTotals } from "../shared/contracts";
import { Badge, Empty, Panel, Row, SourceNote, Stat } from "./components";
import { duration, number } from "./format";

function sum(values: UsageTotals[]) {
  return values.reduce((n, value) => n + value.tasks, 0);
}
export function Usage({ snapshot }: { snapshot: Snapshot }) {
  const source = snapshot.sources.usage;
  const usage = source.data;
  const totals = usage?.totals;
  const days = [...(usage?.by_day ?? [])].sort((a, b) =>
    a.day.localeCompare(b.day),
  );
  const peak = Math.max(
    1,
    ...days.map((day) => sum(Object.values(day.models))),
  );
  return (
    <>
      <SourceNote source={source} />
      <div className="stats-grid">
        <Stat
          label="Tasks · 7 days"
          value={number(totals?.tasks)}
          detail="Managed task completions"
        />
        <Stat
          label="Errors · 7 days"
          value={number(totals?.errors)}
          detail="Recorded unsuccessful outcomes"
        />
        <Stat
          label="Output tokens"
          value={number(totals?.output_tokens)}
          detail={`${number(totals?.prompt_tokens)} prompt tokens`}
        />
        <Stat
          label="Busy time"
          value={duration(totals?.busy_ms)}
          detail={`${duration(totals?.queue_wait_ms)} spent waiting`}
        />
      </div>
      <Panel
        title="Daily task volume"
        caption="Usage ledger · calendar days in UTC"
        action={<Badge>{usage?.window_days ?? 7} day window</Badge>}
      >
        {days.length === 0 ? (
          <Empty title={usage ? "No usage recorded" : "Usage unavailable"}>
            {usage
              ? "Daily volume appears as managed tasks complete."
              : "Waiting for a valid observation from the usage endpoint."}
          </Empty>
        ) : (
          <div className="usage-chart">
            {days.map((day) => {
              const tasks = sum(Object.values(day.models));
              const errors = Object.values(day.models).reduce(
                (n, m) => n + m.errors,
                0,
              );
              return (
                <div className="usage-column" key={day.day}>
                  <span>{number(tasks)}</span>
                  <div className="usage-track">
                    <div
                      style={{ height: `${(tasks / peak) * 100}%` }}
                      title={`${day.day}: ${tasks} tasks, ${errors} errors`}
                    />
                  </div>
                  <span>{day.day.slice(5)}</span>
                </div>
              );
            })}
          </div>
        )}
      </Panel>
      <Panel
        title="Usage by model"
        caption="Prompt and output tokens reported by the runtime"
      >
        {usage && Object.keys(usage.by_model).length > 0 ? (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Model</th>
                  <th>Tasks</th>
                  <th>Errors</th>
                  <th>Prompt tokens</th>
                  <th>Output tokens</th>
                  <th>Busy time</th>
                  <th>Queue wait</th>
                </tr>
              </thead>
              <tbody>
                {Object.entries(usage.by_model)
                  .sort(([, a], [, b]) => b.tasks - a.tasks)
                  .map(([name, m]) => (
                    <tr key={name}>
                      <td>
                        <strong className="model-name">{name}</strong>
                      </td>
                      <td>{number(m.tasks)}</td>
                      <td>{m.errors}</td>
                      <td>{number(m.prompt_tokens)}</td>
                      <td>{number(m.output_tokens)}</td>
                      <td>{duration(m.busy_ms)}</td>
                      <td>{duration(m.queue_wait_ms)}</td>
                    </tr>
                  ))}
              </tbody>
            </table>
          </div>
        ) : (
          <Empty
            title={usage ? "No model usage yet" : "Model usage unavailable"}
          >
            Only managed tasks enter the usage ledger. Raw proxy traffic is
            counted separately.
          </Empty>
        )}
        {usage && (
          <>
            <Row label="Ledger records" value={number(usage.ledger.records)} />
            <Row
              label="Persistence"
              value={usage.ledger.path ?? "In memory only"}
            />
            {usage.ledger.last_error && (
              <p className="warning-text">{usage.ledger.last_error}</p>
            )}
          </>
        )}
      </Panel>
    </>
  );
}
