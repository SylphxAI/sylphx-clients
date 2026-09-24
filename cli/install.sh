#!/bin/sh
# Installs the `sylphx` CLI from its GitHub Release (SylphxAI/sylphx-clients):
#
#   curl -fsSL https://github.com/SylphxAI/sylphx-clients/releases/latest/download/install.sh | sh
#
# Environment:
#   SYLPHX_VERSION   a release version (0.24.0) instead of the latest
#   SYLPHX_BIN_DIR   install directory (default ~/.local/bin)
#
# The binary is checked against the release's SHA256SUMS. Its build provenance
# (Sigstore-signed, GitHub-built) verifies with:
#   gh attestation verify "$(command -v sylphx)" --repo SylphxAI/sylphx-clients
set -eu

repo="SylphxAI/sylphx-clients"
bin_dir="${SYLPHX_BIN_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *) echo "sylphx: unsupported OS $(uname -s); use: cargo install sylphx-cli" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x64 ;;
  aarch64|arm64) arch=arm64 ;;
  *) echo "sylphx: unsupported CPU $(uname -m); use: cargo install sylphx-cli" >&2; exit 1 ;;
esac
asset="sylphx-cli-${os}-${arch}"

if [ -n "${SYLPHX_VERSION:-}" ]; then
  base="https://github.com/${repo}/releases/download/v${SYLPHX_VERSION#v}"
else
  base="https://github.com/${repo}/releases/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fetch() {
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  else wget -q "$1" -O "$2"; fi
}
fetch "${base}/${asset}" "${tmp}/${asset}"
fetch "${base}/SHA256SUMS" "${tmp}/SHA256SUMS"

want="$(grep " ${asset}\$" "${tmp}/SHA256SUMS" | cut -d' ' -f1)"
if command -v sha256sum >/dev/null 2>&1; then got="$(sha256sum "${tmp}/${asset}" | cut -d' ' -f1)"
else got="$(shasum -a 256 "${tmp}/${asset}" | cut -d' ' -f1)"; fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
  echo "sylphx: checksum mismatch for ${asset}; not installed" >&2
  exit 1
fi

mkdir -p "$bin_dir"
install -m 0755 "${tmp}/${asset}" "${bin_dir}/sylphx"
echo "Installed $("${bin_dir}/sylphx" --version) to ${bin_dir}/sylphx"
case ":$PATH:" in
  *":${bin_dir}:"*) ;;
  *) echo "Add ${bin_dir} to your PATH." ;;
esac
echo "Sign in: sylphx login   (agents: SYLPHX_API_KEY=sylphx_sk_… or sylphx login --api-key -)"
