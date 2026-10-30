// Canonical JSON for hashed identities, calibration keys and model-facing action echoes, where the
// exact bytes matter. Defaults: sorted keys, `", "` / `": "` separators, and every non-ASCII
// character escaped as `\uXXXX`. Numbers render as integers unless their key is in `floatKeys`,
// which renders them as floats (`8` → `8.0`, `1e-05`, `1e+16`).

export interface CanonicalJsonOptions {
  sortKeys?: boolean;
  ensureAscii?: boolean;
  separators?: readonly [item: string, key: string];
  floatKeys?: ReadonlySet<string>;
}

export function canonicalJson(value: unknown, options: CanonicalJsonOptions = {}): string {
  const { sortKeys = true, ensureAscii = true, separators: [item, colon] = [", ", ": "], floatKeys } = options;
  const quote = (text: string): string => {
    const json = JSON.stringify(text);
    return ensureAscii
      ? json.replace(/[\u007f-\uffff]/g, (char) => `\\u${char.charCodeAt(0).toString(16).padStart(4, "0")}`)
      : json;
  };
  const render = (node: unknown, float: boolean): string => {
    if (node === null) return "null";
    if (typeof node === "string") return quote(node);
    if (typeof node === "boolean") return String(node);
    if (typeof node === "number") return numberText(node, float);
    if (Array.isArray(node)) return `[${node.map((entry) => render(entry, false)).join(item)}]`;
    if (typeof node === "object") {
      const record = node as Record<string, unknown>;
      const keys = Object.keys(record).filter((key) => record[key] !== undefined);
      if (sortKeys) keys.sort();
      return `{${keys.map((key) => `${quote(key)}${colon}${render(record[key], floatKeys?.has(key) ?? false)}`).join(item)}}`;
    }
    throw new TypeError(`cannot serialize a ${typeof node} as JSON`);
  };
  return render(value, false);
}

function numberText(value: number, float: boolean): string {
  if (!Number.isFinite(value)) throw new RangeError(`cannot serialize ${value} as JSON`);
  if (Number.isInteger(value) && !float) return Math.abs(value) < 1e21 ? String(value) : BigInt(value).toString();
  const [mantissa, power] = value.toExponential().split("e") as [string, string];
  const exponent = Number(power);
  if (value !== 0 && (exponent < -4 || exponent >= 16)) {
    return `${mantissa}e${exponent < 0 ? "-" : "+"}${String(Math.abs(exponent)).padStart(2, "0")}`;
  }
  const text = String(value);
  return Number.isInteger(value) ? `${text}.0` : text;
}

/** Whether `value` is a JSON object (not an array or null). */
export const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);
