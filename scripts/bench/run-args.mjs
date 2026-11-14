// run.mjs argument parsing. Semantic checks live in lib.mjs validateRunOptions.
import { parseHogGb } from "./memory-hog.mjs";

const VALUED = ["--config", "--out", "--vendor", "--samples", "--warmups", "--max-load-per-cpu", "--concurrency", "--cold-samples", "--hog-gb"];
const FLAGS = { "--allow-smoke": "allowSmoke", "--keep-loaded": "keepLoaded", "--strict-load": "strictLoad", "--cold": "cold", "--force-hog": "forceHog" };
const integer = (value, flag) => { if (!/^\d+$/.test(value)) throw new Error(`${flag} must be an integer.`); return Number(value); };

export function parseRunArgs(argv) {
  const raw = {}, options = { keepLoaded: false, strictLoad: false, cold: false, forceHog: false, allowSmoke: false };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") return { help: true };
    if (FLAGS[arg]) { options[FLAGS[arg]] = true; continue; }
    if (!VALUED.includes(arg)) throw new Error(`Unknown argument: ${arg}`);
    const value = argv[++index];
    if (!value || value.startsWith("--")) throw new Error(`Missing value for ${arg}.`);
    raw[arg.slice(2)] = value;
  }
  if (!raw.config) throw new Error("--config is required.");
  Object.assign(options, { config: raw.config, out: raw.out, vendor: raw.vendor });
  options.samples = raw.samples === undefined ? 30 : Number(raw.samples);
  options.warmups = raw.warmups === undefined ? 2 : Number(raw.warmups);
  if (raw["max-load-per-cpu"] !== undefined) {
    options.maxLoadPerCpu = Number(raw["max-load-per-cpu"]);
    if (!Number.isFinite(options.maxLoadPerCpu) || options.maxLoadPerCpu <= 0) throw new Error("--max-load-per-cpu must be a positive number.");
  }
  options.concurrency = raw.concurrency === undefined ? [] : raw.concurrency.split(",").map(level => integer(level.trim(), "--concurrency"));
  if (raw["cold-samples"] !== undefined) {
    if (!options.cold) throw new Error("--cold-samples requires --cold.");
    options.coldSamples = integer(raw["cold-samples"], "--cold-samples");
  }
  if (raw["hog-gb"] !== undefined) options.hogGb = parseHogGb(raw["hog-gb"]);
  if (options.hogGb !== undefined && !options.cold) throw new Error("--hog-gb requires --cold.");
  if (options.forceHog && options.hogGb === undefined) throw new Error("--force-hog requires --hog-gb.");
  if (options.strictLoad && options.maxLoadPerCpu === undefined) throw new Error("--strict-load requires --max-load-per-cpu.");
  return options;
}
