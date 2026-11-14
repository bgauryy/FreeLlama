#!/usr/bin/env sh
# One-line install for macOS and Linux:
#   curl -fsSL https://raw.githubusercontent.com/bgauryy/FreeLlama/main/scripts/install.sh | sh
# Installs FreeLlama, then checks for Ollama. When Ollama is missing it asks before running Ollama's
# official installer (https://ollama.com/install.sh). It never downloads a model.
set -eu

usage() {
  cat >&2 <<'EOF'
Usage: install.sh [--version vX.Y.Z] [--method npm|binary] [--bin-dir PATH] [--with-ollama | --no-ollama]
  --version      a release tag (default: the latest release)
  --method       npm: @octocodeai/freellama with the MCP server and agent commands (default when
                 npm is on PATH and no --version is given); binary: the checksum-verified
                 operator binary (serve, doctor, catalog...)
  --bin-dir      where the binary goes (default: $XDG_BIN_HOME or ~/.local/bin)
  --with-ollama  install Ollama without asking when it is missing
  --no-ollama    never install Ollama; print the command instead
EOF
}

main() {
  release_version=""
  method=""
  ollama_mode="ask"
  install_dir="${XDG_BIN_HOME:-${HOME}/.local/bin}"
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --version) release_version="${2:-}"; [ -n "$release_version" ] || { usage; exit 2; }; shift 2 ;;
      --method) method="${2:-}"; shift 2 ;;
      --bin-dir) install_dir="${2:-}"; [ -n "$install_dir" ] || { usage; exit 2; }; shift 2 ;;
      --with-ollama) ollama_mode="yes"; shift ;;
      --no-ollama) ollama_mode="no"; shift ;;
      -h|--help) usage; exit 0 ;;
      *) usage; exit 2 ;;
    esac
  done
  # A pinned --version keeps the verified-binary install it always meant.
  if [ -z "$method" ]; then
    if [ -z "$release_version" ] && command -v npm >/dev/null 2>&1; then method="npm"; else method="binary"; fi
  fi
  case "$method" in npm|binary) ;; *) usage; exit 2 ;; esac

  case "$(uname -s)" in
    Darwin) os="darwin" ;;
    Linux) os="linux" ;;
    *) echo "Unsupported operating system: $(uname -s)" >&2; exit 1 ;;
  esac

  temporary_dir="$(mktemp -d)"
  trap 'rm -rf "$temporary_dir"' EXIT HUP INT TERM
  if [ "$method" = "npm" ]; then install_npm; else install_binary; fi
  ensure_ollama
  echo "Next: freellama init (checks Ollama, serve and installed models; it never downloads a model)."
  [ "$method" != "npm" ] || echo "MCP: claude mcp add freellama -- npx -y @octocodeai/freellama-mcp-server"
}

install_npm() {
  command -v npm >/dev/null 2>&1 || { echo "npm is not on PATH; rerun with --method binary, or install Node.js 22+" >&2; exit 1; }
  package="@octocodeai/freellama"
  [ -z "$release_version" ] || package="${package}@${release_version#v}"
  echo "Installing ${package} with npm"
  npm install -g "$package" </dev/null
}

install_binary() {
  case "$(uname -m)" in
    arm64|aarch64) architecture="arm64" ;;
    x86_64|amd64) architecture="x64" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
  esac
  if [ "$os" = "linux" ]; then
    if ldd --version 2>&1 | grep -qi musl; then libc="musl"; else libc="gnu"; fi
    platform="${os}-${architecture}-${libc}"
  else
    platform="${os}-${architecture}"
  fi
  asset="freellama-${platform}"
  if [ -n "${FREELLAMA_RELEASE_BASE:-}" ]; then base="$FREELLAMA_RELEASE_BASE"
  elif [ -n "$release_version" ]; then base="https://github.com/bgauryy/FreeLlama/releases/download/${release_version}"
  else base="https://github.com/bgauryy/FreeLlama/releases/latest/download"; fi
  label="${release_version:-the latest release}"
  curl -fL --proto '=https' --tlsv1.2 "${base}/SHA256SUMS" -o "${temporary_dir}/SHA256SUMS"
  expected="$(awk -v file="$asset" '$2 == file {print $1}' "${temporary_dir}/SHA256SUMS")"
  [ -n "$expected" ] || { echo "${label} has no prebuilt binary for ${platform}; build from source with: cargo build --release" >&2; exit 1; }
  curl -fL --proto '=https' --tlsv1.2 "${base}/${asset}" -o "${temporary_dir}/${asset}"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "${temporary_dir}/${asset}" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "${temporary_dir}/${asset}" | awk '{print $1}')"
  else
    echo "Need sha256sum or shasum to verify ${asset}" >&2; exit 1
  fi
  [ "$actual" = "$expected" ] || { echo "Checksum mismatch for ${asset}" >&2; exit 1; }
  mkdir -p "$install_dir"
  install -m 0755 "${temporary_dir}/${asset}" "${install_dir}/freellama"
  echo "Installed verified ${label} to ${install_dir}/freellama"
  case ":${PATH}:" in *":${install_dir}:"*) ;; *) echo "Add ${install_dir} to PATH to run freellama." ;; esac
}

# The same places packages/mcp/src/ollama-presence.ts checks. FREELLAMA_OLLAMA_PATHS replaces them.
ollama_installed() {
  command -v ollama >/dev/null 2>&1 && return 0
  if [ -n "${FREELLAMA_OLLAMA_PATHS+set}" ]; then places="$FREELLAMA_OLLAMA_PATHS"
  elif [ "$os" = "darwin" ]; then places="/Applications/Ollama.app /usr/local/bin/ollama /opt/homebrew/bin/ollama"
  else places="/usr/local/bin/ollama /usr/bin/ollama"; fi
  for place in $places; do [ -e "$place" ] && return 0; done
  return 1
}

ensure_ollama() {
  ollama_installer="${FREELLAMA_OLLAMA_INSTALLER:-https://ollama.com/install.sh}"
  command_text="curl -fsSL ${ollama_installer} | sh"
  if ollama_installed; then echo "Ollama is installed."; return 0; fi
  answer="no"
  if [ "$ollama_mode" = "yes" ]; then answer="yes"
  elif [ "$ollama_mode" = "ask" ] && ( : </dev/tty ) 2>/dev/null; then
    printf "Ollama is not installed. Run its official installer now (%s)? [y/N] " "$command_text" >/dev/tty
    read -r reply </dev/tty || reply=""
    case "$reply" in y|Y|yes|YES) answer="yes" ;; esac
  fi
  if [ "$answer" = "yes" ]; then
    echo "Installing Ollama: ${command_text}"
    curl -fsSL --proto '=https' --tlsv1.2 "$ollama_installer" -o "${temporary_dir}/ollama-install.sh"
    sh "${temporary_dir}/ollama-install.sh" </dev/null
  else
    echo "Ollama is not installed. Install it with: ${command_text} (or https://ollama.com/download)"
  fi
}

# Called last, so a partial download runs nothing and the whole script is read before any step.
main "$@"
