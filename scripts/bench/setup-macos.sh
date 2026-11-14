#!/usr/bin/env bash
# Installs only the three selected competitors, sequentially. No models or daemons.
set -euo pipefail
cd "$(dirname "$0")/../.."
if [[ "${1:---plan}" != "--install" ]]; then
  cat <<'PLAN'
Usage: bash scripts/bench/setup-macos.sh --install
Comparison: Ollama alone, FreeLlama, llama.cpp, LocalAI, oMLX.
Install order: llama.cpp -> LocalAI -> oMLX. No models are downloaded or services started.
Requires normal network and installation permissions. See docs/BENCHMARK.md.
PLAN
  exit 0
fi
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo 'This setup targets Apple-silicon macOS.' >&2; exit 1; }
command -v brew >/dev/null || { echo 'Homebrew is required: https://brew.sh' >&2; exit 1; }
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1
mkdir -p .octocode/bench/setup
log=".octocode/bench/setup/$(date -u +%Y%m%dT%H%M%SZ)-$$.log"
{
  curl --fail --silent --show-error --head --max-time 15 https://formulae.brew.sh/api/formula/llama.cpp.json >/dev/null
  brew install llama.cpp
  brew install localai
  brew tap jundot/omlx https://github.com/jundot/omlx
  if brew help trust >/dev/null 2>&1; then brew trust --formula jundot/omlx/omlx; fi
  brew install jundot/omlx/omlx
  brew list --versions llama.cpp localai omlx
  echo 'Software installed. llama-swap comes from its GitHub release; configure every system and exact model artifact before benchmarking.'
} 2>&1 | tee "$log"
