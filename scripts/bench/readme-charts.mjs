// The benchmark overview image: Ollama, FreeLlama and the most popular alternatives on six measured
// KPIs, computed from a run's raw files, plus what FreeLlama adds over Ollama. Writes an SVG, then a
// PNG through headless Chrome when it is installed (a throwaway profile; nothing else is touched).
// Usage: node scripts/bench/readme-charts.mjs --run FORWARD_DIR --run REVERSED_DIR --facts FILE.json
//   [--features FEATURES.json] --out DIR (the features image: scripts/bench/features.mjs results)
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const args = process.argv.slice(2);
const values = (flag) => args.flatMap((arg, index) => (arg === flag ? [args[index + 1]] : []));
const [out] = values("--out");
const [factsFile] = values("--facts");
const runs = values("--run");
if (!out || !factsFile || runs.length === 0) {
  console.error("Usage: node scripts/bench/readme-charts.mjs --run DIR [--run DIR] --facts FILE.json --out DIR");
  process.exit(2);
}
const facts = JSON.parse(readFileSync(factsFile, "utf8"));

const INK = "#0b0b0b", INK_2 = "#52514e", MUTED = "#76746d", GRID = "#e4e3df", SURFACE = "#fcfcfb", PANEL = "#ffffff";
const COLOR = { freellama: "#2a78d6", ollama: "#3d3c38", other: "#a3a198" };
const FONT = "-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif";
const esc = (text) => String(text).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
const text = (x, y, body, { size = 16, weight = 400, fill = INK, anchor = "start" } = {}) =>
  `<text x="${x}" y="${y}" font-family="${FONT}" font-size="${size}" font-weight="${weight}" fill="${fill}" text-anchor="${anchor}">${esc(body)}</text>`;
const quantile = (xs, p) => {
  const sorted = [...xs].sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  const k = (sorted.length - 1) * p, f = Math.floor(k), c = Math.min(f + 1, sorted.length - 1);
  return sorted[f] + (sorted[c] - sorted[f]) * (k - f);
};
const lines = (file) => (existsSync(file) ? readFileSync(file, "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line)) : []);

/** Every KPI for one vendor id, from the runs' raw trials, concurrency and cold samples. */
function measure(id) {
  const all = runs.flatMap((run) => lines(join(run, id, "trials.jsonl"))).filter((trial) => !trial.warmup);
  const warm = all.filter((trial) => trial.outcome === "verified");
  const perRun = runs.map((run) => JSON.parse(readFileSync(join(run, "summary.json"), "utf8")).vendors.find((vendor) => vendor.id === id)).filter(Boolean);
  const four = runs.flatMap((run) => lines(join(run, id, "concurrency-4.jsonl")));
  const fourWindow = four.length ? Math.max(...four.map((t) => t.client_end_ms)) - Math.min(...four.map((t) => t.client_start_ms)) : 0;
  const cold = runs.flatMap((run) => lines(join(run, id, "cold.jsonl"))).filter((trial) => trial.outcome === "verified");
  return {
    correct: `${warm.length}/${all.length}`,
    p50: quantile(warm.map((t) => t.elapsed_ms), 0.5),
    p95: quantile(warm.map((t) => t.elapsed_ms), 0.95),
    ttft: quantile(warm.map((t) => t.first_content_ms).filter((v) => typeof v === "number"), 0.5),
    tpm1: perRun.length ? perRun.reduce((sum, vendor) => sum + vendor.useful_tasks_per_minute, 0) / perRun.length : null,
    tpm4: fourWindow > 0 ? four.filter((t) => t.outcome === "verified").length / (fourWindow / 60_000) : null,
    cold: cold.length ? quantile(cold.map((t) => t.elapsed_ms), 0.5) / 1000 : null,
  };
}

const systems = facts.systems.map((system) => {
  const measured = measure(system.id);
  // A system can take one KPI from another vendor block (FreeLlama's first token comes from its streamed mode).
  for (const [kpi, from] of Object.entries(system.from ?? {})) measured[kpi] = measure(from)[kpi];
  for (const kpi of system.notComparable ?? []) measured[kpi] = null;
  return { ...system, ...measured };
});

const KPIS = [
  { key: "p50", title: "Median task time", better: "lower", format: (v) => `${Math.round(v)} ms` },
  { key: "p95", title: "Slowest 5% (p95)", better: "lower", format: (v) => `${Math.round(v)} ms` },
  { key: "ttft", title: "Time to first token", better: "lower", format: (v) => `${Math.round(v)} ms` },
  { key: "tpm1", title: "Throughput, 1 client", better: "higher", format: (v) => `${Math.round(v)}/min` },
  { key: "tpm4", title: "Throughput, 4 clients", better: "higher", format: (v) => `${Math.round(v)}/min` },
  { key: "cold", title: "Cold request (model unloaded)", better: "lower", format: (v) => `${v.toFixed(2)} s` },
];

function overview() {
  const width = 1600, margin = 48, gap = 28, columns = 3;
  const panelW = (width - 2 * margin - (columns - 1) * gap) / columns, panelH = 300;
  const rowsTop = 176;
  const barH = 30, barGap = 14, labelW = 150, valueW = 96;
  const stripTop = rowsTop + 2 * panelH + 92, stripH = 150;
  const height = stripTop + stripH + 92;
  let body = `<rect width="100%" height="100%" fill="${SURFACE}"/>`;
  body += text(margin, 62, facts.title, { size: 36, weight: 800 });
  body += text(margin, 98, facts.subtitle, { size: 19, fill: INK_2 });
  // Legend, in popularity order. Identity is never color alone: names are printed on every bar.
  let x = margin;
  for (const system of systems) {
    const label = system.stars ? `${system.name}  ★ ${system.stars}` : `${system.name}  (${system.note})`;
    body += `<rect x="${x}" y="127" width="16" height="16" rx="3" fill="${COLOR[system.kind]}"/>`;
    body += text(x + 24, 141, label, { size: 16, weight: system.kind === "other" ? 500 : 700 });
    x += 44 + label.length * 8.2;
  }
  body += text(width - margin, 141, "Ordered by GitHub stars", { size: 14, fill: MUTED, anchor: "end" });

  KPIS.forEach((kpi, index) => {
    const px = margin + (index % columns) * (panelW + gap);
    const py = rowsTop + Math.floor(index / columns) * (panelH + 24);
    body += `<rect x="${px}" y="${py}" width="${panelW}" height="${panelH}" rx="10" fill="${PANEL}" stroke="${GRID}"/>`;
    body += text(px + 20, py + 36, kpi.title, { size: 19, weight: 700 });
    body += text(px + panelW - 20, py + 36, `${kpi.better} is better`, { size: 13, fill: MUTED, anchor: "end" });
    const max = Math.max(...systems.map((system) => system[kpi.key]).filter((v) => typeof v === "number"));
    const barX = px + 20 + labelW, barW = panelW - 40 - labelW - valueW;
    systems.forEach((system, row) => {
      const y = py + 60 + row * (barH + barGap);
      const value = system[kpi.key];
      body += text(barX - 10, y + barH / 2 + 6, system.name, { size: 15, weight: system.kind === "other" ? 500 : 700, anchor: "end" });
      if (typeof value !== "number") {
        body += text(barX, y + barH / 2 + 5, system.notes?.[kpi.key] ?? "not comparable", { size: 13, fill: MUTED });
        return;
      }
      const w = Math.max(6, (value / max) * barW);
      body += `<path d="M${barX},${y} H${barX + w - 4} Q${barX + w},${y} ${barX + w},${y + 4} V${y + barH - 4} Q${barX + w},${y + barH} ${barX + w - 4},${y + barH} H${barX} Z" fill="${COLOR[system.kind]}"/>`;
      body += text(barX + w + 8, y + barH / 2 + 6, kpi.format(value), { size: 15, weight: system.kind === "other" ? 500 : 800 });
    });
  });

  // What FreeLlama adds over Ollama: the measured cost and the measured gains.
  body += text(margin, stripTop - 6, facts.adds.title, { size: 21, weight: 800 });
  const tiles = facts.adds.tiles, tileGap = 16;
  const tileW = (width - 2 * margin - (tiles.length - 1) * tileGap) / tiles.length;
  tiles.forEach((tile, index) => {
    const tx = margin + index * (tileW + tileGap), ty = stripTop + 10;
    body += `<rect x="${tx}" y="${ty}" width="${tileW}" height="${stripH - 20}" rx="10" fill="#eef4fc" stroke="#c9dcf5"/>`;
    body += text(tx + 18, ty + 46, tile.value, { size: 30, weight: 800, fill: "#1c5cab" });
    body += text(tx + 18, ty + 76, tile.label, { size: 15, weight: 700 });
    body += text(tx + 18, ty + 100, tile.detail, { size: 13, fill: INK_2 });
  });
  body += text(margin, height - 50, facts.footnote, { size: 14, fill: INK_2 });
  body += text(margin, height - 26, facts.source, { size: 14, fill: MUTED });
  return { width, height, svg: `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">${body}</svg>\n` };
}

/** A bar panel: rows of {label, value, kind, text}; bars scale to the panel's largest value. */
function barPanel(x, y, w, h, panel) {
  let body = `<rect x="${x}" y="${y}" width="${w}" height="${h}" rx="10" fill="${PANEL}" stroke="${GRID}"/>`;
  body += text(x + 20, y + 34, panel.title, { size: 19, weight: 700 });
  body += text(x + w - 20, y + 34, `${panel.better} is better`, { size: 13, fill: MUTED, anchor: "end" });
  if (panel.note) body += text(x + 20, y + 58, panel.note, { size: 13, fill: INK_2 });
  const labelW = panel.labelW ?? 150, valueW = 150, barH = 30, gap = 16;
  const barX = x + 20 + labelW, barW = w - 40 - labelW - valueW;
  const max = Math.max(...panel.rows.map((row) => row.value));
  panel.rows.forEach((row, index) => {
    const by = y + 78 + index * (barH + gap);
    const bw = Math.max(6, (row.value / max) * barW);
    body += text(barX - 10, by + barH / 2 + 6, row.label, { size: 15, weight: row.kind === "freellama" ? 700 : 500, anchor: "end" });
    body += `<path d="M${barX},${by} H${barX + bw - 4} Q${barX + bw},${by} ${barX + bw},${by + 4} V${by + barH - 4} Q${barX + bw},${by + barH} ${barX + bw - 4},${by + barH} H${barX} Z" fill="${COLOR[row.kind]}"/>`;
    body += text(barX + bw + 8, by + barH / 2 + 6, row.text, { size: 15, weight: row.kind === "freellama" ? 800 : 500 });
  });
  if (panel.footer) body += text(x + 20, y + h - 18, panel.footer, { size: 13, fill: INK_2 });
  return body;
}

/** The features image: quality and time per workflow, then each management feature before and after. */
function features(data) {
  const width = 1600, margin = 48, gap = 28;
  let body = `<rect width="100%" height="100%" fill="${SURFACE}"/>`;
  body += text(margin, 62, data.title, { size: 36, weight: 800 });
  body += text(margin, 98, data.subtitle, { size: 19, fill: INK_2 });
  body += `<rect x="${margin}" y="127" width="16" height="16" rx="3" fill="${COLOR.freellama}"/>` + text(margin + 24, 141, "With FreeLlama", { size: 16, weight: 700 });
  body += `<rect x="${margin + 190}" y="127" width="16" height="16" rx="3" fill="${COLOR.other}"/>` + text(margin + 214, 141, "Without it: Ollama alone, a one-line prompt, or a first call", { size: 16 });

  // Row 1: per-workflow quality and time (left), priority (right).
  const rowTop = 172, leftW = (width - 2 * margin - gap) * 0.64, rightW = width - 2 * margin - gap - leftW;
  const wf = data.workflows, lineH = 34;
  const leftH = 92 + wf.length * lineH + 30;
  body += `<rect x="${margin}" y="${rowTop}" width="${leftW}" height="${leftH}" rx="10" fill="${PANEL}" stroke="${GRID}"/>`;
  body += text(margin + 20, rowTop + 34, data.workflowTitle, { size: 19, weight: 700 });
  body += text(margin + 20, rowTop + 58, data.workflowNote, { size: 13, fill: INK_2 });
  const colName = margin + 20, colBar = margin + 190, barW = 210, colB = colBar + barW + 150;
  body += text(colBar, rowTop + 82, "Built-in prompt: pass rate · median time", { size: 13, weight: 700, fill: INK_2 });
  body += text(colB, rowTop + 82, "One-line prompt", { size: 13, weight: 700, fill: INK_2 });
  wf.forEach((row, index) => {
    const y = rowTop + 92 + index * lineH;
    const a = row.shipped_pass / row.shipped_runs, b = row.oneline_pass / row.oneline_runs;
    body += text(colName, y + 20, row.workflow, { size: 15, weight: 600 });
    body += `<rect x="${colBar}" y="${y + 6}" width="${Math.max(4, a * barW)}" height="18" rx="3" fill="${COLOR.freellama}"/>`;
    body += text(colBar + Math.max(4, a * barW) + 8, y + 20, `${Math.round(a * 100)}% · ${row.shipped_median_s.toFixed(1)} s`, { size: 14, weight: 800 });
    body += `<rect x="${colB}" y="${y + 6}" width="${Math.max(4, b * barW)}" height="18" rx="3" fill="${COLOR.other}"/>`;
    body += text(colB + Math.max(4, b * barW) + 8, y + 20, `${Math.round(b * 100)}% · ${row.oneline_median_s.toFixed(1)} s`, { size: 14 });
  });
  body += barPanel(margin + leftW + gap, rowTop, rightW, leftH, data.panels[0]);

  // Row 2: the before/after panels, as many columns as there are panels.
  const second = data.panels.slice(1), count = Math.max(1, second.length);
  const row2 = rowTop + leftH + gap, panelW = (width - 2 * margin - (count - 1) * gap) / count, panelH = 200;
  second.forEach((panel, index) => {
    body += barPanel(margin + index * (panelW + gap), row2, panelW, panelH, { labelW: 110, ...panel });
  });

  // Row 3: measured facts that have no "without" bar.
  const row3 = row2 + panelH + gap + 24, tileGap = 16, tiles = data.tiles;
  body += text(margin, row3 - 6, data.tilesTitle, { size: 21, weight: 800 });
  const tileW = (width - 2 * margin - (tiles.length - 1) * tileGap) / tiles.length;
  tiles.forEach((tile, index) => {
    const tx = margin + index * (tileW + tileGap), ty = row3 + 10;
    body += `<rect x="${tx}" y="${ty}" width="${tileW}" height="126" rx="10" fill="#eef4fc" stroke="#c9dcf5"/>`;
    body += text(tx + 18, ty + 46, tile.value, { size: 30, weight: 800, fill: "#1c5cab" });
    body += text(tx + 18, ty + 76, tile.label, { size: 15, weight: 700 });
    body += text(tx + 18, ty + 100, tile.detail, { size: 13, fill: INK_2 });
  });
  const height = row3 + 10 + 126 + 84;
  body += text(margin, height - 50, data.footnote, { size: 14, fill: INK_2 });
  body += text(margin, height - 26, data.source, { size: 14, fill: MUTED });
  return { width, height, svg: `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">${body}</svg>\n` };
}

const chart = overview();
const [featuresFile] = values("--features");
const images = [["benchmark-overview.svg", chart]];
if (featuresFile) images.push(["benchmark-features.svg", features(JSON.parse(readFileSync(featuresFile, "utf8")))]);
for (const [name, image] of images) writeFileSync(resolve(out, name), image.svg);
writeFileSync(resolve(out, "benchmark-overview.json"),
  `${JSON.stringify(systems.map(({ id, name, stars, correct, p50, p95, ttft, tpm1, tpm4, cold }) => ({ id, name, stars, correct, p50, p95, ttft, tpm1, tpm4, cold })), null, 2)}\n`);
const chrome = ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/usr/bin/google-chrome", "/usr/bin/chromium"].find(existsSync);
for (const [name, image] of chrome ? images : []) {
  const file = resolve(out, name);
  const chart = image;
  // Headless Chrome sometimes exits early on a busy machine; a fresh profile and a retry fix it.
  for (let attempt = 1; ; attempt++) {
    const profile = mkdtempSync(join(tmpdir(), "freellama-charts-"));
    try {
      // Device scale 2 keeps the PNG sharp on high-density screens.
      execFileSync(chrome, ["--headless=new", "--disable-gpu", "--hide-scrollbars", `--user-data-dir=${profile}`, "--force-device-scale-factor=2",
        `--window-size=${chart.width},${chart.height}`, `--screenshot=${file.replace(/\.svg$/, ".png")}`, `file://${file}`], { stdio: "ignore" });
      break;
    } catch (error) {
      if (attempt === 3) throw error;
    } finally {
      rmSync(profile, { recursive: true, force: true });
    }
  }
  console.log(file.replace(/\.svg$/, ".png"));
}
