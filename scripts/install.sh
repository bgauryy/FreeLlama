#!/usr/bin/env sh
set -eu

usage() {
  echo "Usage: scripts/install.sh --version vX.Y.Z [--bin-dir PATH]" >&2
}

release_version=""
install_dir="${XDG_BIN_HOME:-${HOME}/.local/bin}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) release_version="${2:-}"; shift 2 ;;
    --bin-dir) install_dir="${2:-}"; shift 2 ;;
    *) usage; exit 2 ;;
  esac
done
[ -n "$release_version" ] || { usage; exit 2; }

case "$(uname -s)" in
  Darwin) platform="darwin" ;;
  Linux) platform="linux" ;;
  *) echo "Unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) architecture="arm64" ;;
  x86_64|amd64) architecture="x64" ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ "$platform" = "linux" ]; then
  if ldd --version 2>&1 | grep -qi musl; then
    libc="musl"
  else
    libc="gnu"
  fi
  platform="${platform}-${architecture}-${libc}"
else
  platform="${platform}-${architecture}"
fi

asset="freellama-${platform}"
base="https://github.com/bgauryy/FreeLlama/releases/download/${release_version}"
temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT HUP INT TERM
curl -fL --proto '=https' --tlsv1.2 "${base}/${asset}" -o "${temporary_dir}/${asset}"
curl -fL --proto '=https' --tlsv1.2 "${base}/SHA256SUMS" -o "${temporary_dir}/SHA256SUMS"
expected="$(awk -v file="$asset" '$2 == file {print $1}' "${temporary_dir}/SHA256SUMS")"
[ -n "$expected" ] || { echo "No checksum published for ${asset}" >&2; exit 1; }
actual="$(shasum -a 256 "${temporary_dir}/${asset}" | awk '{print $1}')"
[ "$actual" = "$expected" ] || { echo "Checksum mismatch for ${asset}" >&2; exit 1; }
mkdir -p "$install_dir"
install -m 0755 "${temporary_dir}/${asset}" "${install_dir}/freellama"
echo "Installed verified ${release_version} to ${install_dir}/freellama"
