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
FREELLAMA_CURL_AUTH=()
if [ -n "$FREELLAMA_AUTH_TOKEN_FILE" ]; then
  if [ ! -r "$FREELLAMA_AUTH_TOKEN_FILE" ]; then
    echo "  FAIL  FREELLAMA_AUTH_TOKEN_FILE is not readable: $FREELLAMA_AUTH_TOKEN_FILE"
    exit 1
  fi
  FREELLAMA_AUTH_TOKEN="$(tr -d '\r\n' < "$FREELLAMA_AUTH_TOKEN_FILE")"
  FREELLAMA_CURL_AUTH=(-H "Authorization: Bearer $FREELLAMA_AUTH_TOKEN")
fi
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
  server_version="$(echo "$version_json" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("version",""))' 2>/dev/null)"
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
echo "== Ollama KV cache & concurrency =="
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
if [ -z "$kv_type" ]; then
  warn "OLLAMA_KV_CACHE_TYPE not visible here — effective f16 if it is also unset in the Ollama process; q8_0 uses about half the KV memory, but benchmark model quality before changing it"
else
  ok "OLLAMA_KV_CACHE_TYPE=$kv_type"
fi
echo "  INFO  OLLAMA_NUM_PARALLEL=${num_parallel:-1 (effective default if unset in Ollama)}"
if [ -z "$max_loaded" ]; then
  warn "OLLAMA_MAX_LOADED_MODELS not visible here — effective cap is 3 x GPU count (or 3 for CPU-only) if unset in Ollama; choose an explicit per-process cap from measured model and memory fit"
else
  ok "OLLAMA_MAX_LOADED_MODELS=$max_loaded"
fi
echo "  INFO  Config visibility is best effort: this shell environment, plus launchd on macOS; a separate service or remote Ollama can differ"

echo
echo "== Resident models & memory =="
if ps_json=$(curl -sf "$OLLAMA_ENDPOINT/api/ps" 2>/dev/null); then
  python3 - "$ps_json" <<'PYEOF'
import ctypes, json, os, platform, subprocess, sys

def total_memory_bytes():
    try:
        if platform.system() == "Darwin":
            return int(subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True).strip())
        if platform.system() == "Windows":
            class MemoryStatus(ctypes.Structure):
                _fields_ = [("length", ctypes.c_ulong), ("load", ctypes.c_ulong),
                            ("total", ctypes.c_ulonglong), ("available", ctypes.c_ulonglong),
                            ("total_page", ctypes.c_ulonglong), ("available_page", ctypes.c_ulonglong),
                            ("total_virtual", ctypes.c_ulonglong), ("available_virtual", ctypes.c_ulonglong),
                            ("available_extended", ctypes.c_ulonglong)]
            status = MemoryStatus()
            status.length = ctypes.sizeof(status)
            return status.total if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)) else None
        return os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE")
    except (AttributeError, OSError, ValueError, subprocess.SubprocessError):
        return None

data = json.loads(sys.argv[1])
models = data.get("models", [])
if not models:
    print("  OK    no models currently resident (cold start)")
else:
    total = 0.0
    for m in models:
        accelerator_bytes = m.get("size_vram", 0) or 0
        committed_bytes = accelerator_bytes or m.get("size", 0) or 0
        size_gb = committed_bytes / (1024**3)
        total += size_gb
        placement = "accelerator" if accelerator_bytes else "host RAM"
        print(f"  INFO  resident: {m['name']:<28} ~{size_gb:.1f} GiB {placement}  expires_at={m.get('expires_at','?')}")
    memory = total_memory_bytes()
    if memory:
        memory_gib = memory / (1024**3)
        fraction = total / memory_gib if memory_gib else 0
        print(f"  INFO  model-memory proxy: ~{total:.1f} GiB of {memory_gib:.1f} GiB host RAM ({fraction:.0%})")
        if fraction >= 0.8:
            print("  WARN  resident model sizes are at least 80% of physical RAM; leave headroom for")
            print("        KV caches, the OS, applications, and discrete-GPU staging. Reduce contexts,")
            print("        unload helpers, or lower the per-process loaded-model cap.")
            sys.exit(2)
    else:
        print(f"  INFO  model-memory proxy: ~{total:.1f} GiB; physical RAM unavailable, so fit is unverified")
PYEOF
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
if curl -sf "${FREELLAMA_CURL_AUTH[@]}" "$FREELLAMA_ENDPOINT/api/version" >/dev/null 2>&1; then
  ok "reachable at $FREELLAMA_ENDPOINT (passthrough works)"
  if health_json=$(curl -sf "${FREELLAMA_CURL_AUTH[@]}" "$FREELLAMA_ENDPOINT/_freellama/v1/health" 2>/dev/null); then
    ok "control-plane routes present — this is 'freellama serve' (full platform)"
    if python3 - "$health_json" <<'PYEOF'
import json, sys
health = json.loads(sys.argv[1])
contracts = health.get("contracts", {})
backends = health.get("backends", {})
gpu = backends.get("gpu", {})
cpu = backends.get("cpu")
security = health.get("security", {})
feedback = health.get("feedback", {})
print(f"  INFO  primary backend: {gpu.get('upstream', '?')} admission={gpu.get('admission', {})}")
if cpu:
    print(f"  INFO  CPU backend: {cpu.get('upstream', '?')} models={','.join(cpu.get('models', []))} admission={cpu.get('admission', {})}")
else:
    print("  INFO  CPU backend: not configured")
ok = (
    contracts.get("hardware_fit") == "sent_num_ctx"
    and contracts.get("machine_profile") == "portable_host_memory_v2"
    and contracts.get("model_backends") == "explicit_cpu_assignment"
    and contracts.get("placement_preference") == "guarded_hint"
    and contracts.get("placement_observation") == "ollama_api_ps_after_execution"
    and contracts.get("placement_evidence_gate") == "configured_or_observed"
    and contracts.get("placement_feedback") == "three_sample_runtime"
    and contracts.get("placement_feedback_metric") == "normalized_work_unit_10_percent"
    and contracts.get("placement_feedback_persistence") == "versioned_atomic_snapshot_v1"
    and contracts.get("authentication") == "optional_bearer_all_routes"
    and contracts.get("immediate_unload_observation") == "observe_then_unload"
    and bool(gpu.get("upstream"))
    and bool(gpu.get("admission", {}).get("slots_total"))
    and feedback.get("persistence", {}).get("enabled") is True
    and security.get("remote_access") is False
)
sys.exit(0 if ok else 1)
PYEOF
    then
      ok "serve contracts are current (auth, persisted feedback, observation, evidence gate, unload)"
    else
      fail "serve is stale or incomplete — rebuild and restart; health lacks the current hardware/backend contracts"
    fi
  else
    ok "no control-plane routes — this is 'freellama proxy' (passthrough + retry only, by design)"
  fi
else
  warn "nothing listening at $FREELLAMA_ENDPOINT — agents/clients are likely hitting Ollama directly,"
  warn "  which means no retry/backoff/timeout protection (proxy.rs send_with_retries). Start it: freellama proxy"
fi

echo
echo "== Installed models (informational — never auto-deleted) =="
if command -v ollama >/dev/null 2>&1; then
  stale_count=0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    if echo "$line" | grep -qE '[0-9]+ (month|year)s? ago'; then
      echo "  INFO  stale candidate: $line"
      stale_count=$((stale_count + 1))
    fi
  done < <(ollama list 2>/dev/null | tail -n +2)
  if [ "$stale_count" -gt 0 ]; then
    echo "  INFO  $stale_count model(s) not modified in 3+ months — review with 'ollama list', remove"
    echo "        with 'ollama rm <name>' if unused. Never delete files under ~/.ollama/models"
    echo "        directly — the blob store is content-addressed and manifest-tracked; only"
    echo "        'ollama rm' updates the manifest safely. See references/disk-cleanup.md."
  else
    echo "  OK    no models older than 3 months"
  fi
else
  warn "ollama CLI not on PATH — skipping stale-model report"
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
  echo "All required checks passed, but see WARN lines above — nothing blocking, something worth fixing."
else
  echo "All checks passed cleanly, no warnings."
fi
exit "$FAILED"
