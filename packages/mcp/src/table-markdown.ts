// OCR models answer "Table Recognition:" in their own markup: MiniCPM-OCR in OTSL
// (`<fcel>cell<lcel><nl>`), others in HTML `<table>`. The read-table workflow promises Markdown, so
// both become a Markdown table. Merged cells become empty cells; the first row is the header.

/** One Markdown table from rows of cell text. */
function markdownTable(rows: string[][]): string {
  const width = Math.max(...rows.map((row) => row.length));
  const cell = (text: string) => text.replace(/\s+/g, " ").trim().replace(/\|/g, "\\|");
  const line = (row: string[]) => `| ${Array.from({ length: width }, (_, index) => cell(row[index] ?? "")).join(" | ")} |`;
  const [header, ...body] = rows;
  return [line(header!), `| ${Array.from({ length: width }, () => "---").join(" | ")} |`, ...body.map(line)].join("\n");
}

/** OTSL: `<fcel>` starts a filled cell, `<ecel>` an empty one, `<lcel>`/`<ucel>`/`<xcel>` a merged one, `<nl>` ends a row. */
function otslRows(otsl: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  for (const [, tag, text] of otsl.matchAll(/<(fcel|ecel|lcel|ucel|xcel|ched|rhed|srow|nl)>([^<]*)/g)) {
    if (tag === "nl") {
      if (row.length > 0) rows.push(row);
      row = [];
      continue;
    }
    row.push(tag === "fcel" || tag === "ched" || tag === "rhed" || tag === "srow" ? text! : "");
  }
  if (row.length > 0) rows.push(row);
  return rows;
}

const decodeEntities = (text: string) => text.replace(/&nbsp;/g, " ").replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, "\"").replace(/&#39;/g, "'").replace(/&amp;/g, "&");

/** HTML `<tr>` rows of `<th>`/`<td>` cells; a `colspan` adds empty cells after its text. */
function htmlRows(table: string): string[][] {
  const rows: string[][] = [];
  for (const [, tr] of table.matchAll(/<tr\b[^>]*>([\s\S]*?)<\/tr>/gi)) {
    const row: string[] = [];
    for (const [, attrs, inner] of tr!.matchAll(/<t[hd]\b([^>]*)>([\s\S]*?)<\/t[hd]>/gi)) {
      row.push(decodeEntities(inner!.replace(/<br\s*\/?>/gi, " ").replace(/<[^>]+>/g, "")));
      const span = Number(/colspan\s*=\s*["']?(\d+)/i.exec(attrs!)?.[1] ?? 1);
      for (let extra = 1; extra < span; extra += 1) row.push("");
    }
    if (row.length > 0) rows.push(row);
  }
  return rows;
}

/** The text with every OTSL or HTML table in it rewritten as a Markdown table; other text is kept. */
export function tablesToMarkdown(text: string): string {
  const replace = (rows: string[][], original: string) => (rows.length > 0 ? markdownTable(rows) : original);
  let out = text.replace(/<table\b[\s\S]*?<\/table>/gi, (table) => replace(htmlRows(table), table));
  out = out.replace(/<otsl>([\s\S]*?)(?:<\/otsl>|$)/gi, (whole, body: string) => replace(otslRows(body), whole));
  // OTSL without its wrapper: a run of cell tags that ends at least one row.
  out = out.replace(/(?:<(?:fcel|ecel|lcel|ucel|xcel|ched|rhed|srow|nl)>[^<\n]*)+/g, (run) => (run.includes("<nl>") ? replace(otslRows(run), run) : run));
  return out;
}
