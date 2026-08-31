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
disk_line=$(df -g "$CHECK_ROOT" 2>/dev/null | tail -1)
free_gb=$(echo "$disk_line" | awk '{print $4}')
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
  cli_version="$(ollama --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
  server_version="$(echo "$version_json" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("version",""))' 2>/dev/null)"
  if [ -n "$cli_version" ] && [ -n "$server_version" ] && [ "$cli_version" != "$server_version" ]; then
    warn "ollama CLI ($cli_version) and server ($server_version) versions differ — restart the app after an upgrade"
  elif [ -n "$cli_version" ] && [ -n "$server_version" ]; then
    ok "CLI/server version match ($cli_version)"
  else
    warn "could not determine CLI or server version — skipping cross-check"
  fi
else
  warn "ollama CLI not on PATH — skipping CLI/server version cross-check"
fi

echo
echo "== Resident models & memory =="
if ps_json=$(curl -sf "$OLLAMA_ENDPOINT/api/ps" 2>/dev/null); then
  python3 - "$ps_json" <<'PYEOF'
import json, sys
data = json.loads(sys.argv[1])
models = data.get("models", [])
if not models:
    print("  OK    no models currently resident (cold start)")
else:
    total = 0
    for m in models:
        size_gb = m.get("size_vram", m.get("size", 0)) / (1024**3)
        total += size_gb
        print(f"  INFO  resident: {m['name']:<28} ~{size_gb:.1f} GB  expires_at={m.get('expires_at','?')}")
    print(f"  INFO  total resident VRAM: ~{total:.1f} GB")
    if len(models) > 1:
        print(f"  WARN  {len(models)} models resident simultaneously — on unified-memory Macs, two large")
        print("        models can exceed physical RAM and crash the server (this is exactly what broke an")
        print("        earlier benchmark run here: qwen3.8:27b-mlx + qwen2.5:32b as a local judge = ~58GB")
        print("        against 48GB). Keep a judge/second model on a DIFFERENT machine, or don't run one")
        print("        locally at all — see references/ollama-config.md.")
        sys.exit(2)  # signal the warning back to the calling shell; see WARNED handling below
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
if curl -sf "$FREELLAMA_ENDPOINT/api/version" >/dev/null 2>&1; then
  ok "reachable at $FREELLAMA_ENDPOINT (passthrough works)"
  if curl -sf "$FREELLAMA_ENDPOINT/_freellama/v1/machine" >/dev/null 2>&1; then
    ok "control-plane routes present — this is 'freellama serve' (full platform)"
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
