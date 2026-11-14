/** Grade the workflow fixtures' exact object fields without running a model or CLI. */
export function gradeExactJson(answer, fields) {
  let parsed;
  try {
    parsed = JSON.parse(answer.replace(/^```(?:json)?\s*|\s*```$/g, ""));
  } catch {
    return ["not JSON"];
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return ["JSON answer must be an object"];
  }
  const failures = [];
  for (const [field, ok] of Object.entries(fields)) {
    if (!Object.hasOwn(parsed, field)) failures.push(`missing field ${field}`);
    else if (!ok(parsed[field])) failures.push(`field ${field} = ${JSON.stringify(parsed[field])}`);
  }
  for (const field of Object.keys(parsed)) {
    if (!Object.hasOwn(fields, field)) failures.push(`unexpected field ${field}`);
  }
  return failures;
}
