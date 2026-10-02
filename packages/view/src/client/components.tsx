import type { CSSProperties, ReactNode } from "react";
import type { Source } from "../shared/contracts";
import { percent } from "./format";

export type Tone = "good" | "warn" | "bad" | "neutral";
export function Badge({
  children,
  tone = "neutral",
}: {
  children: ReactNode;
  tone?: Tone;
}) {
  return (
    <span className={`badge ${tone}`}>
      <span className="badge-dot" />
      {children}
    </span>
  );
}
export function Panel({
  title,
  caption,
  action,
  children,
  className = "",
}: {
  title: string;
  caption?: string;
  action?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`panel ${className}`}>
      <div className="panel-heading">
        <div>
          <h2>{title}</h2>
          {caption && <p>{caption}</p>}
        </div>
        {action}
      </div>
      {children}
    </section>
  );
}
export function Stat({
  label,
  value,
  detail,
  children,
}: {
  label: string;
  value: string;
  detail: string;
  children?: ReactNode;
}) {
  return (
    <div className="stat">
      <span className="eyebrow">{label}</span>
      <div className="stat-value">
        {value}
        {children}
      </div>
      <p>{detail}</p>
    </div>
  );
}
export function Row({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="detail-row">
      <span>{label}</span>
      <span>{value}</span>
    </div>
  );
}
export function Meter({
  used,
  total,
  tone = "good",
  label,
}: {
  used: number | null | undefined;
  total: number | null | undefined;
  tone?: Tone;
  label: string;
}) {
  const share = percent(used, total);
  return (
    <div
      className={`meter ${tone} ${share === null ? "unknown" : ""}`}
      role={share === null ? "img" : "meter"}
      aria-label={share === null ? `${label}: Unknown` : label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={share ?? undefined}
      aria-valuetext={share === null ? "Unknown" : `${share.toFixed(0)}%`}
      style={{ "--share": `${share ?? 0}%` } as CSSProperties}
    >
      <span />
    </div>
  );
}
export function SourceNote<T>({ source }: { source: Source<T> }) {
  if (source.state === "live") return null;
  return (
    <div className={`source-note ${source.state}`} role="status">
      <Badge tone={source.state === "stale" ? "warn" : "bad"}>
        {source.state === "stale" ? "Last known observation" : "Unavailable"}
      </Badge>
      <span>{source.error}</span>
      {source.updated_at && (
        <small>
          Observed {new Date(source.updated_at).toLocaleTimeString()}
        </small>
      )}
    </div>
  );
}
export function Empty({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <div className="empty">
      <div className="empty-symbol">◌</div>
      <h3>{title}</h3>
      <p>{children}</p>
    </div>
  );
}
export function Icon({ name, size = 20 }: { name: string; size?: number }) {
  const paths: Record<string, ReactNode> = {
    overview: (
      <>
        <rect x="3" y="3" width="7" height="7" rx="1.5" />
        <rect x="14" y="3" width="7" height="7" rx="1.5" />
        <rect x="3" y="14" width="7" height="7" rx="1.5" />
        <rect x="14" y="14" width="7" height="7" rx="1.5" />
      </>
    ),
    models: (
      <>
        <path d="m12 3 9 5-9 5-9-5 9-5Z" />
        <path d="m3 12 9 5 9-5M3 16l9 5 9-5" />
      </>
    ),
    usage: (
      <>
        <path d="M4 19h16M7 15V9m5 6V5m5 10v-4" />
      </>
    ),
    system: (
      <>
        <rect x="5" y="5" width="14" height="14" rx="3" />
        <rect x="9" y="9" width="6" height="6" rx="1" />
        <path d="M9 2v3m6-3v3M9 19v3m6-3v3M2 9h3m-3 6h3m14-6h3m-3 6h3" />
      </>
    ),
    diagnostics: (
      <>
        <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9l-6-6Z" />
        <path d="M14 3v6h6M8 13h8m-8 4h5" />
      </>
    ),
    refresh: (
      <>
        <path d="M20 7v5h-5M4 17v-5h5" />
        <path d="M6 7a7 7 0 0 1 12-2l2 3M4 16l2 3a7 7 0 0 0 12-2" />
      </>
    ),
    pause: (
      <>
        <path d="M8 5v14M16 5v14" />
      </>
    ),
    play: <path d="m8 4 12 8-12 8V4Z" />,
    arrow: <path d="m9 5 7 7-7 7" />,
    search: (
      <>
        <circle cx="10.5" cy="10.5" r="6.5" />
        <path d="m16 16 5 5" />
      </>
    ),
  };
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {paths[name] ?? paths.system}
    </svg>
  );
}
