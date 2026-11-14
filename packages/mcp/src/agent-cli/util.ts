// Small shape helpers shared by the agent CLI modules.

export type Fields = Record<string, unknown>;

/** `value` as a plain object, else undefined. */
export const record = (value: unknown): Fields | undefined =>
  value && typeof value === "object" && !Array.isArray(value) ? value as Fields : undefined;

/** `fields` without its undefined values. */
export function withoutUndefined(fields: Fields): Fields {
  return Object.fromEntries(Object.entries(fields).filter(([, value]) => value !== undefined));
}

/** `fields` without undefined values or empty objects (an empty `options` is not sent). */
export function withoutEmpty(fields: Fields): Fields {
  return Object.fromEntries(Object.entries(fields).filter(([, value]) => value !== undefined && !(record(value) && Object.keys(value as Fields).length === 0)));
}
