export function bytes(value: number | null | undefined): string {
  if (value == null) return "Unknown";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let scaled = value;
  let index = 0;
  while (scaled >= 1024 && index < units.length - 1) {
    scaled /= 1024;
    index++;
  }
  return `${scaled.toLocaleString(undefined, { maximumFractionDigits: index === 0 ? 0 : 1 })} ${units[index]}`;
}
export const number = (value: number | null | undefined) =>
  value == null ? "Unknown" : value.toLocaleString();
export function duration(ms: number | null | undefined): string {
  if (ms == null) return "Unknown";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  if (ms < 3_600_000) return `${(ms / 60_000).toFixed(1)} min`;
  return `${(ms / 3_600_000).toFixed(1)} h`;
}
export function display(value: unknown): string {
  if (value == null) return "Unknown";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}
export function percent(
  used: number | null | undefined,
  total: number | null | undefined,
): number | null {
  return used == null || total == null || total <= 0
    ? null
    : Math.min(100, Math.max(0, (used / total) * 100));
}
