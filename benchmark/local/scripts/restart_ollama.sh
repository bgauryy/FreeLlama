#!/usr/bin/env bash
# Restarts the local Ollama server, then ensures something is listening on 127.0.0.1:11435
# (existing `freellama serve`, or a passthrough `freellama proxy`). Adapters talk to 11435, not
# raw Ollama: the sidecar retries 500/502/504 load blips (not 503 busy) — see
# packages/rust-core/src/proxy.rs.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"

if pgrep -f "Ollama.app/Contents/Resources/ollama serve" >/dev/null 2>&1; then
  echo "stopping running Ollama app..."
  osascript -e 'quit app "Ollama"' 2>/dev/null || true
  pkill -f "Ollama.app/Contents/Resources/ollama serve" 2>/dev/null || true
  sleep 2
fi

echo "starting Ollama..."
open -a Ollama
for _ in $(seq 1 30); do
  if curl -sf http://127.0.0.1:11434/api/version >/dev/null 2>&1; then
    echo "Ollama is up: $(curl -s http://127.0.0.1:11434/api/version)"
    break
  fi
  sleep 1
done
if ! curl -sf http://127.0.0.1:11434/api/version >/dev/null 2>&1; then
  echo "Ollama did not come up within 30s" >&2
  exit 1
fi

if pgrep -f "target/release/freellama proxy" >/dev/null 2>&1; then
  echo "stopping running freellama proxy..."
  pkill -f "target/release/freellama proxy" 2>/dev/null || true
  sleep 1
fi

if curl -sf http://127.0.0.1:11435/api/version >/dev/null 2>&1; then
  echo "127.0.0.1:11435 already up (freellama serve or leftover proxy) — not starting another listener"
  exit 0
fi

echo "starting freellama proxy (127.0.0.1:11435 -> 127.0.0.1:11434)..."
(cd "$REPO_ROOT" && cargo build --release --quiet && nohup "$REPO_ROOT/target/release/freellama" proxy \
  >/tmp/freellama-proxy.log 2>&1 &)
for _ in $(seq 1 20); do
  if curl -sf http://127.0.0.1:11435/api/version >/dev/null 2>&1; then
    echo "freellama proxy is up: $(curl -s http://127.0.0.1:11435/api/version)"
    exit 0
  fi
  sleep 1
done

echo "freellama proxy did not come up within 20s (see /tmp/freellama-proxy.log)" >&2
exit 1
