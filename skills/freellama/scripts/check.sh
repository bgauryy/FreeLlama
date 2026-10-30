#!/usr/bin/env bash
# Smart, one-shot health check for Ollama + the FreeLlama proxy/serve layer in front of it.
# Read-only: never restarts, kills, or mutates anything. Exit 0 = all checks passed.
set -uo pipefail

# The skill is standalone: it does not assume it lives inside the FreeLlama checkout. The disk
# check runs against wherever it is invoked from; the optional binary-freshness check below only
# runs if you point FREELLAMA_REPO at a checkout.
CHECK_ROOT="${FREELLAMA_CHECK_ROOT:-$PWD}"
FREELLAMA_REPO="${FREELLAMA_REPO:-}"
OLLAMA_ENDPOINT="${OLLAMA_ENDPOINT:-http://127.0.0.1:11434}"
FREELLAMA_ENDPOINT="${FREELLAMA_ENDPOINT:-http://127.0.0.1:11435}"
FREELLAMA_AUTH_TOKEN_FILE="${FREELLAMA_AUTH_TOKEN_FILE:-}"
if [ -n "$FREELLAMA_AUTH_TOKEN_FILE" ]; then
  if [ ! -r "$FREELLAMA_AUTH_TOKEN_FILE" ]; then
    echo "  FAIL  FREELLAMA_AUTH_TOKEN_FILE is not readable: $FREELLAMA_AUTH_TOKEN_FILE"
    exit 1
  fi
  FREELLAMA_AUTH_TOKEN="$(tr -d '\r\n' < "$FREELLAMA_AUTH_TOKEN_FILE")"
fi
freellama_curl() {
  if [ -n "$FREELLAMA_AUTH_TOKEN_FILE" ]; then
    curl -sf -H "Authorization: Bearer $FREELLAMA_AUTH_TOKEN" "$@"
  else
    curl -sf "$@"
  fi
}
# Shared JSON/Python-compatible formatting for the `node -e` blocks below. Values print the way
# the health contract documents them: dicts and lists as Python-style literals, None/True/False.
IFS= read -r -d '' CHECK_JS_HELPERS <<'JSEOF'
const fs = require("fs");
const print = (text) => fs.writeSync(1, text + "\n");
const isDict = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const typeName = (value) => value === null ? "NoneType" : Array.isArray(value) ? "list" : isDict(value) ? "dict"
  : typeof value === "string" ? "str" : typeof value === "boolean" ? "bool" : Number.isInteger(value) ? "int" : "float";
function get(object, key, fallback = null) {
  if (!isDict(object)) throw new TypeError(`'${typeName(object)}' object has no attribute 'get'`);
  return Object.prototype.hasOwnProperty.call(object, key) ? object[key] : fallback;
}
function truthy(value) {
  if (value === null || value === false || value === 0 || value === "") return false;
  if (Array.isArray(value)) return value.length > 0;
  if (isDict(value)) return Object.keys(value).length > 0;
  return true;
}
function reprString(text) {
  const quote = text.includes("'") && !text.includes('"') ? '"' : "'";
  let out = quote;
  for (const char of text) {
    const code = char.codePointAt(0);
    if (char === "\\" || char === quote) out += "\\" + char;
    else if (char === "\n") out += "\\n";
    else if (char === "\r") out += "\\r";
    else if (char === "\t") out += "\\t";
    else if (char !== " " && /[\p{Cc}\p{Cf}\p{Cs}\p{Co}\p{Cn}\p{Zl}\p{Zp}\p{Zs}]/u.test(char)) {
      out += code < 0x100 ? "\\x" + code.toString(16).padStart(2, "0")
        : code < 0x10000 ? "\\u" + code.toString(16).padStart(4, "0") : "\\U" + code.toString(16).padStart(8, "0");
    } else out += char;
  }
  return out + quote;
}
function repr(value) {
  if (value === null) return "None";
  if (value === true) return "True";
  if (value === false) return "False";
  if (typeof value === "string") return reprString(value);
  if (typeof value === "number") {
    if (Number.isInteger(value) && Math.abs(value) < 1e16) return String(value);
    if (!Number.isFinite(value)) return Number.isNaN(value) ? "nan" : value > 0 ? "inf" : "-inf";
    return String(value).replace(/e([+-])(\d)$/, "e$10$2");
  }
  if (Array.isArray(value)) return "[" + value.map(repr).join(", ") + "]";
  return "{" + Object.entries(value).map(([key, item]) => reprString(key) + ": " + repr(item)).join(", ") + "}";
}
const str = (value) => typeof value === "string" ? value : repr(value);
function join(separator, value) {
  const items = typeof value === "string" ? [...value] : Array.isArray(value) ? value : isDict(value) ? Object.keys(value) : null;
  if (items === null) throw new TypeError("can only join an iterable");
  for (const item of items) if (typeof item !== "string") throw new TypeError(`sequence item: expected str instance, ${typeName(item)} found`);
  return items.join(separator);
}
// Python `format(x, ".Nf")`: exact binary value, round half to even.
function fixed(x, digits) {
  if (!Number.isFinite(x)) return Number.isNaN(x) ? "nan" : x > 0 ? "inf" : "-inf";
  const view = new DataView(new ArrayBuffer(8));
  view.setFloat64(0, Math.abs(x));
  const bits = view.getBigUint64(0);
  const exponent = Number((bits >> 52n) & 0x7ffn);
  let mantissa = bits & ((1n << 52n) - 1n);
  if (exponent !== 0) mantissa |= 1n << 52n;
  const shift = (exponent === 0 ? -1074 : exponent - 1075);
  let numerator = mantissa * 10n ** BigInt(digits);
  let denominator = 1n;
  if (shift >= 0) numerator <<= BigInt(shift); else denominator <<= BigInt(-shift);
  let quotient = numerator / denominator;
  const remainder = numerator % denominator;
  if (2n * remainder > denominator || (2n * remainder === denominator && quotient % 2n === 1n)) quotient += 1n;
  const text = quotient.toString().padStart(digits + 1, "0");
  const body = digits ? text.slice(0, -digits) + "." + text.slice(-digits) : text;
  return (x < 0 || Object.is(x, -0) ? "-" : "") + body;
}
JSEOF

FAILED=0
WARNED=0
warn() { echo "  WARN  $1"; WARNED=1; }
ok()   { echo "  OK    $1"; }
fail() { echo "  FAIL  $1"; FAILED=1; }

echo "== Disk space =="
# Absolute free-GB threshold, not just percent-used — a dev machine sitting at 90%+ full from
# unrelated data is normal and shouldn't nag; genuinely low absolute headroom is what caused a
# real incident (188Mi free out of 926Gi during a benchmark run — see references/disk-cleanup.md).
MIN_FREE_GB="${MIN_FREE_GB:-15}"
disk_line=$(df -Pk "$CHECK_ROOT" 2>/dev/null | tail -1)
free_gb=$(echo "$disk_line" | awk '{if ($4 ~ /^[0-9]+$/) printf "%d", $4 / 1024 / 1024}')
if [ -n "$free_gb" ] && [ "$free_gb" -lt "$MIN_FREE_GB" ] 2>/dev/null; then
  fail "only ${free_gb}GB free on the volume containing $CHECK_ROOT (threshold ${MIN_FREE_GB}GB) — see references/disk-cleanup.md before running anything that copies large fixtures or pulls models"
else
  ok "${free_gb:-?}GB free on the volume containing $CHECK_ROOT"
fi

echo
echo "== Ollama server =="
version_json=""
if version_json=$(curl -sf "$OLLAMA_ENDPOINT/api/version" 2>/dev/null); then
  ok "reachable at $OLLAMA_ENDPOINT ($version_json)"
else
  fail "not reachable at $OLLAMA_ENDPOINT — is 'ollama serve' / the Ollama app running?"
fi

if [ -z "$version_json" ]; then
  warn "skipping CLI/server version cross-check — server unreachable, nothing to compare against"
elif command -v ollama >/dev/null 2>&1; then
  cli_output="$(ollama --version 2>&1)"
  cli_version="$(printf '%s\n' "$cli_output" | sed -nE 's/^Warning: client version is ([0-9]+\.[0-9]+\.[0-9]+).*$/\1/p' | head -1)"
  if [ -z "$cli_version" ]; then
    cli_version="$(printf '%s\n' "$cli_output" | sed -nE 's/^ollama version is ([0-9]+\.[0-9]+\.[0-9]+).*$/\1/p' | head -1)"
  fi
  server_version="$(echo "$version_json" | node -e "$CHECK_JS_HELPERS"'print(str(get(JSON.parse(fs.readFileSync(0, "utf8")), "version", "")));' 2>/dev/null)"
  if [ -n "$cli_version" ] && [ -n "$server_version" ] && [ "$cli_version" != "$server_version" ]; then
    warn "ollama CLI ($cli_version at $(command -v ollama)) and server ($server_version) versions differ — align PATH with the active Ollama installation"
  elif [ -n "$cli_version" ] && [ -n "$server_version" ]; then
    ok "CLI/server version match ($cli_version)"
  else
    warn "could not determine CLI or server version — skipping cross-check"
  fi
else
  warn "ollama CLI not on PATH — skipping CLI/server version cross-check"
fi

echo
echo "== Ollama configuration =="
read_ollama_env() {
  local name="$1"
  local value="${!name-}"
  if [ -z "$value" ] && command -v launchctl >/dev/null 2>&1; then
    value=$(launchctl getenv "$name" 2>/dev/null || true)
  fi
  printf '%s' "$value"
}
kv_type=$(read_ollama_env OLLAMA_KV_CACHE_TYPE)
num_parallel=$(read_ollama_env OLLAMA_NUM_PARALLEL)
max_loaded=$(read_ollama_env OLLAMA_MAX_LOADED_MODELS)
max_queue=$(read_ollama_env OLLAMA_MAX_QUEUE)
ollama_host=$(read_ollama_env OLLAMA_HOST)
no_cloud=$(read_ollama_env OLLAMA_NO_CLOUD)
flash_attention=$(read_ollama_env OLLAMA_FLASH_ATTENTION)

case "$ollama_host" in
  ""|127.0.0.1:*|localhost:*|\[::1\]:*)
    ok "OLLAMA_HOST=${ollama_host:-127.0.0.1:11434 (effective loopback default if unset in Ollama)}"
    ;;
  *)
    warn "OLLAMA_HOST=$ollama_host is not an obvious loopback binding — require an authenticated, TLS-protected ingress before exposing Ollama"
    ;;
esac
case "$no_cloud" in
  1|true|TRUE|True)
    ok "OLLAMA_NO_CLOUD=$no_cloud (cloud inference and web search disabled)"
    ;;
  0|false|FALSE|False)
    warn "OLLAMA_NO_CLOUD=$no_cloud — cloud features remain available; use 1 when the deployment contract is local-only"
    ;;
  *)
    warn "OLLAMA_NO_CLOUD not visible here — local-only operation is not proven; set 1 in the Ollama service environment when required"
    ;;
esac
if [ -z "$kv_type" ]; then
  warn "OLLAMA_KV_CACHE_TYPE not visible here — effective f16 if it is also unset in the Ollama process; q8_0 uses about half the KV memory, but benchmark model quality before changing it"
else
  ok "OLLAMA_KV_CACHE_TYPE=$kv_type"
fi
echo "  INFO  OLLAMA_NUM_PARALLEL=${num_parallel:-1 (effective default if unset in Ollama)}"
echo "  INFO  OLLAMA_FLASH_ATTENTION=${flash_attention:-auto (backend/device dependent if unset in Ollama)}"
echo "  INFO  OLLAMA_MAX_QUEUE=${max_queue:-512 (effective default if unset in Ollama)} — this is separate from FreeLlama admission waiting"
if [ -z "$max_loaded" ]; then
  warn "OLLAMA_MAX_LOADED_MODELS not visible here — effective cap is 3 x GPU count (or 3 for CPU-only) if unset in Ollama; choose an explicit per-process cap from measured model and memory fit"
else
  ok "OLLAMA_MAX_LOADED_MODELS=$max_loaded"
fi
echo "  INFO  Config visibility is best effort: this shell environment, plus launchd on macOS; a separate service or remote Ollama can differ"

echo
echo "== Resident models & memory =="
if ps_json=$(curl -sf "$OLLAMA_ENDPOINT/api/ps" 2>/dev/null); then
  IFS= read -r -d '' RESIDENT_JS <<'JSEOF'
const os = require("os");
const { execFileSync } = require("child_process");

function totalMemoryBytes() {
  try {
    if (process.platform === "darwin") {
      const text = execFileSync("sysctl", ["-n", "hw.memsize"], { encoding: "utf8", stdio: ["inherit", "pipe", "inherit"] })
        .replace(/^[\s\x1c-\x1f\x85\xa0]+|[\s\x1c-\x1f\x85\xa0]+$/g, "");
      if (!/^[+-]?\d+(_\d+)*$/.test(text)) return null;
      return Number(BigInt(text.replace(/_/g, "")));
    }
    return os.totalmem();
  } catch {
    return null;
  }
}

function gib(value) {
  if (typeof value === "boolean") return Number(value) / 1024 ** 3;
  if (typeof value !== "number") throw new TypeError(`unsupported operand type(s) for /: '${typeName(value)}' and 'int'`);
  return value / 1024 ** 3;
}

function left(value, width) {
  if (typeof value === "boolean") value = Number(value);
  if (typeof value !== "string" && typeof value !== "number") throw new TypeError(`unsupported format string passed to ${typeName(value)}.__format__`);
  const text = typeof value === "string" ? value : repr(value);
  return text + " ".repeat(Math.max(0, width - [...text].length));
}

const data = JSON.parse(process.argv[1]);
const models = get(data, "models", []);
if (!truthy(models)) {
  print("  OK    no models currently resident (cold start)");
} else {
  let total = 0;
  const entries = Array.isArray(models) ? models : typeof models === "string" ? [...models] : isDict(models) ? Object.keys(models) : null;
  if (entries === null) throw new TypeError(`'${typeName(models)}' object is not iterable`);
  for (const model of entries) {
    const acceleratorBytes = truthy(get(model, "size_vram", 0)) ? model.size_vram : 0;
    const committedBytes = truthy(acceleratorBytes) ? acceleratorBytes : (truthy(get(model, "size", 0)) ? model.size : 0);
    const sizeGb = gib(committedBytes);
    total += sizeGb;
    const placement = truthy(acceleratorBytes) ? "accelerator" : "host RAM";
    if (!Object.prototype.hasOwnProperty.call(model, "name")) throw new Error("KeyError: 'name'");
    print(`  INFO  resident: ${left(model.name, 28)} ~${fixed(sizeGb, 1)} GiB ${placement}  expires_at=${str(get(model, "expires_at", "?"))}`);
  }
  const memory = totalMemoryBytes();
  if (truthy(memory)) {
    const memoryGib = memory / 1024 ** 3;
    const fraction = memoryGib ? total / memoryGib : 0;
    print(`  INFO  model-memory proxy: ~${fixed(total, 1)} GiB of ${fixed(memoryGib, 1)} GiB host RAM (${fixed(fraction * 100, 0)}%)`);
    if (fraction >= 0.8) {
      print("  WARN  resident model sizes are at least 80% of physical RAM; leave headroom for");
      print("        KV caches, the OS, applications, and discrete-GPU staging. Reduce contexts,");
      print("        unload helpers, or lower the per-process loaded-model cap.");
      process.exitCode = 2;
    }
  } else {
    print(`  INFO  model-memory proxy: ~${fixed(total, 1)} GiB; physical RAM unavailable, so fit is unverified`);
  }
}
JSEOF
  node -e "$CHECK_JS_HELPERS$RESIDENT_JS" -- "$ps_json"
py_status=$?
if [ "$py_status" -eq 2 ]; then
  WARNED=1
elif [ "$py_status" -ne 0 ]; then
  fail "resident-model memory check failed unexpectedly (exit $py_status)"
fi
else
  fail "could not query $OLLAMA_ENDPOINT/api/ps"
fi

echo
echo "== FreeLlama proxy/serve =="
if freellama_curl "$FREELLAMA_ENDPOINT/api/version" >/dev/null 2>&1; then
  ok "reachable at $FREELLAMA_ENDPOINT (passthrough works)"
  if health_json=$(freellama_curl "$FREELLAMA_ENDPOINT/_freellama/v1/health" 2>/dev/null); then
    ok "control-plane routes present — this is 'freellama serve' (full platform)"
    IFS= read -r -d '' HEALTH_JS <<'JSEOF'
const health = JSON.parse(process.argv[1]);
const contracts = get(health, "contracts", {});
const backends = get(health, "backends", {});
const gpu = get(backends, "gpu", {});
const cpu = get(backends, "cpu");
const security = get(health, "security", {});
const feedback = get(health, "feedback", {});
print(`  INFO  primary backend: ${str(get(gpu, "upstream", "?"))} admission=${str(get(gpu, "admission", {}))}`);
if (truthy(cpu)) {
  print(`  INFO  CPU backend: ${str(get(cpu, "upstream", "?"))} models=${join(",", get(cpu, "models", []))} admission=${str(get(cpu, "admission", {}))}`);
} else {
  print("  INFO  CPU backend: not configured");
}
const ok = (
  get(contracts, "hardware_fit") === "sent_num_ctx"
  && get(contracts, "machine_profile") === "portable_host_memory_v2"
  && get(contracts, "model_backends") === "explicit_cpu_assignment"
  && get(contracts, "placement_preference") === "guarded_hint"
  && get(contracts, "placement_observation") === "ollama_api_ps_after_execution"
  && get(contracts, "placement_evidence_gate") === "configured_or_observed"
  && get(contracts, "placement_feedback") === "three_sample_runtime"
  && get(contracts, "placement_feedback_metric") === "normalized_work_unit_10_percent"
  && get(contracts, "placement_feedback_persistence") === "versioned_atomic_snapshot_v1"
  && get(contracts, "authentication") === "optional_bearer_all_routes"
  && get(contracts, "immediate_unload_observation") === "observe_then_unload"
  && truthy(get(gpu, "upstream"))
  && truthy(get(get(gpu, "admission", {}), "slots_total"))
);
if (!ok) process.exit(1);
const remote = get(security, "remote_access");
const authentication = get(security, "authentication");
if (typeof remote !== "boolean" || !["none", "bearer"].includes(authentication) || (remote === true && authentication !== "bearer")) process.exit(3);
let warned = false;
const resources = get(get(health, "admission", {}), "resources", {});
if (get(resources, "holding") === true) {
  print(`  WARN  managed task execution is held: ${join(",", get(resources, "reasons", [])) || "host resource policy"}`);
  warned = true;
}
const persistence = get(feedback, "persistence", {});
if (get(persistence, "enabled") === false) {
  print("  WARN  feedback is intentionally ephemeral; learned placement feedback will not survive restart");
  warned = true;
} else if (get(persistence, "enabled") !== true) {
  print("  WARN  feedback persistence state is unavailable; persistence is not verified");
  warned = true;
} else if (truthy(get(persistence, "last_error"))) {
  print("  WARN  feedback persistence reports an error; inspect doctor for details");
  warned = true;
}
if (remote === true) {
  print("  WARN  remote access uses bearer authentication; require TLS-protected ingress before exposing the service");
  warned = true;
}
process.exitCode = warned ? 2 : 0;
JSEOF
    node -e "$CHECK_JS_HELPERS$HEALTH_JS" -- "$health_json"
    health_status=$?
    case "$health_status" in
      0|2)
        ok "serve contracts are current (auth, feedback persistence policy, observation, evidence gate, unload)"
        if [ "$health_status" -eq 2 ]; then WARNED=1; fi
        ;;
      3)
        fail "remote access is unauthenticated or security configuration is unavailable — require bearer authentication before exposing the service"
        ;;
      *)
        fail "serve is stale or incomplete — rebuild and restart; health lacks the current hardware/backend contracts"
        ;;
    esac
  else
    ok "no control-plane routes — this is 'freellama proxy' (passthrough + retry only, by design)"
  fi
else
  warn "nothing listening at $FREELLAMA_ENDPOINT — agents/clients are likely hitting Ollama directly,"
  warn "  which means no retry/backoff/timeout protection (proxy/retry.rs send_with_retries). Start it: freellama proxy"
fi

echo
echo "== Installed models (informational — never auto-deleted) =="
if command -v ollama >/dev/null 2>&1; then
  old_metadata_count=0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    if echo "$line" | awk '{for (i = 1; i <= NF - 2; i++) if ($i ~ /^[0-9]+$/ && $(i+2) == "ago" && (($(i+1) ~ /^months?$/ && $i >= 3) || ($(i+1) ~ /^years?$/ && $i >= 1))) found=1} END {exit !found}'; then
      echo "  INFO  old model metadata: $line"
      old_metadata_count=$((old_metadata_count + 1))
    fi
  done < <(ollama list 2>/dev/null | tail -n +2)
  if [ "$old_metadata_count" -gt 0 ]; then
    echo "  INFO  $old_metadata_count model(s) have metadata timestamps at least 3 months old."
    echo "        Metadata age does not establish when a model was last used."
  else
    echo "  OK    no model metadata timestamps at least 3 months old"
  fi
else
  warn "ollama CLI not on PATH — skipping model metadata-age report"
fi

echo
echo "== Binary freshness (only when FREELLAMA_REPO points at a checkout) =="
if [ -z "$FREELLAMA_REPO" ]; then
  echo "  INFO  FREELLAMA_REPO unset — skipping. Set it to a FreeLlama checkout to check the binary."
elif [ -x "$FREELLAMA_REPO/target/release/freellama" ]; then
  src_newest=$(find "$FREELLAMA_REPO/packages" -name '*.rs' -newer "$FREELLAMA_REPO/target/release/freellama" 2>/dev/null | head -1)
  if [ -n "$src_newest" ]; then
    warn "Rust sources are newer than target/release/freellama ($src_newest) — rebuild: cargo build --release"
  else
    ok "release binary is up to date with packages/**/*.rs"
  fi
else
  warn "no release binary at $FREELLAMA_REPO/target/release/freellama — build one: cargo build --release"
fi

echo
if [ "$FAILED" -ne 0 ]; then
  echo "One or more required checks FAILED — see above."
elif [ "$WARNED" -ne 0 ]; then
  echo "Required service checks passed; review WARN lines before task execution."
else
  echo "All checks passed cleanly, no warnings."
fi
exit "$FAILED"
