#!/usr/bin/env bash
# Exercise the CLI and MCP npm archives with this candidate's native binary,
# without depending on a version that has not reached npm yet.
#   scripts/check-cli-npm.sh <native sylphx binary> [package workspace root]
set -euo pipefail
scripts="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "${2:-$scripts/..}" && pwd)"
binary="$(realpath "$1")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
platform="$(node -p 'process.platform + "-" + process.arch')"
[ -d "$root/cli/npm/cli-$platform" ] || { echo "unsupported npm smoke platform: $platform" >&2; exit 1; }
mkdir -p "$work/platform/bin" "$work/archives"
cp "$root/cli/npm/cli-$platform/package.json" "$work/platform/"
install -m755 "$binary" "$work/platform/bin/sylphx"
(cd "$work/platform" && npm pack --silent --pack-destination "$work/archives" >/dev/null)
platform_archive="$(find "$work/archives" -name '*.tgz' -print -quit)"
(cd "$root/cli/npm/cli" && npm pack --silent --pack-destination "$work/archives" >/dev/null)
cli_archive="$(find "$work/archives" -name 'sylphx-cli-[0-9]*.tgz' -print -quit)"
[ -n "$cli_archive" ] || { echo "CLI archive missing" >&2; exit 1; }
# The CLI's optional platform dependency and MCP's exact CLI dependency must
# install from the same candidate archives in both PR and publication checks.
bash "$scripts/smoke-npm.sh" "$root/cli/npm/cli" "$platform_archive"
bash "$scripts/smoke-npm.sh" "$root/mcp/npm" "$platform_archive" "$cli_archive"
mkdir "$work/app"
(
  cd "$work/app"
  printf '%s\n' '{"name":"sylphx-mcp-smoke","private":true}' > package.json
  (cd "$root/mcp/npm" && npm pack --silent --pack-destination "$work/archives" >/dev/null)
  mcp_archive="$(find "$work/archives" -name 'sylphx-mcp-*.tgz' -print -quit)"
  npm install --silent --no-audit --no-fund "$platform_archive" "$cli_archive" "$mcp_archive"
  # Read the installed manifest back: an old registry CLI cannot stand in for
  # the candidate pinned by MCP, even when the wrapper still starts.
  node - <<'JS'
const fs = require('node:fs')
const cli = JSON.parse(fs.readFileSync('node_modules/@sylphx/cli/package.json', 'utf8'))
const mcp = JSON.parse(fs.readFileSync('node_modules/@sylphx/mcp/package.json', 'utf8'))
if (cli.version !== mcp.version || mcp.dependencies['@sylphx/cli'] !== cli.version)
  throw new Error(`MCP/CLI version mismatch: ${mcp.version}, ${mcp.dependencies['@sylphx/cli']}, ${cli.version}`)
console.log(`installed MCP ${mcp.version} pins CLI ${cli.version}`)
JS
  python3 -I "$scripts/smoke-mcp.py" -- "$work/app/node_modules/.bin/sylphx-mcp"
)
