#!/usr/bin/env bash
# Pack a package, install it into clean consumers, and exercise its runtime
# exports. Optional peer entry points are tested with their declared peers,
# while the root and peer-free entry points are tested without optional peers.
# Non-JavaScript exports are checked in the tarball but never executed.
#
#   scripts/smoke-npm.sh <package dir> [dependency tarball ...]
set -euo pipefail
pkg="$(cd "$1" && pwd)"
shift
user_args=("$@")
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

(cd "$pkg" && npm pack --silent --pack-destination "$work" >/dev/null)
tgz="$(find "$work" -maxdepth 1 -name '*.tgz' -print -quit)"
[ -n "$tgz" ] || { echo "::error::npm pack produced no archive"; exit 1; }
mkdir "$work/archive"
tar -xzf "$tgz" -C "$work/archive"
manifest="$work/archive/package/package.json"
name="$(jq -r .name "$manifest")"
unset NODE_PATH

# A dependency archive supplied by the caller (for example the exact SDK
# archive used by the Next check) already satisfies a peer. Do not replace it
# with a registry install of the same peer.
provided_names=()
for arg in "${user_args[@]}"; do
  case "$arg" in
    *.tgz)
      if [ -f "$arg" ]; then
        provided="$(tar -xOzf "$arg" package/package.json 2>/dev/null | jq -r .name 2>/dev/null || true)"
        [ -n "$provided" ] && provided_names+=("$provided")
      fi
      ;;
  esac
done

peer_lines=()
while IFS=$'\t' read -r peer range optional; do
  [ -n "$peer" ] || continue
  skip=false
  for provided in "${provided_names[@]}"; do
    [ "$provided" = "$peer" ] && skip=true
  done
  [ "$skip" = true ] || peer_lines+=("$peer"$'\t'"$range"$'\t'"$optional")
done < <(node - "$manifest" <<'JS'
const manifest = JSON.parse(require('node:fs').readFileSync(process.argv[2], 'utf8'))
for (const [name, range] of Object.entries(manifest.peerDependencies ?? {})) {
  const optional = manifest.peerDependenciesMeta?.[name]?.optional === true
  process.stdout.write(`${name}\t${range}\t${optional}\n`)
}
JS
)
peer_specs=()
optional_peers=()
for line in "${peer_lines[@]}"; do
  IFS=$'\t' read -r peer range optional <<< "$line"
  peer_specs+=("$peer@$range")
  [ "$optional" = true ] && optional_peers+=("$peer")
done
printf 'smoke: %s; declared peers: %s\n' "$name" "${peer_specs[*]:-none}"

install_consumer() {
  local dir="$1"
  shift
  mkdir -p "$dir"
  printf '%s\n' '{"name":"sylphx-npm-smoke","private":true}' > "$dir/package.json"
  (cd "$dir" && npm install --silent --no-audit --no-fund --loglevel error "${user_args[@]}" "$tgz" "$@")
}

# Enumerate runtime exports from the installed archive. Every target must be a
# non-empty file in that archive. The output is TSV: kind, import specifier,
# target path (kind includes the loader mode). Assets are proof-only; JS files are exercised below.
enumerate_exports() {
  local dir="$1"
  node - "$name" "$work/archive/package" <<'JS'
const fs = require('node:fs')
const path = require('node:path')
const [name, root] = process.argv.slice(2)
const manifest = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
const declared = manifest.exports ?? (manifest.main ? { '.': manifest.main } : {})
const exportsMap = typeof declared === 'string' ? { '.': declared } : declared
if (!exportsMap || typeof exportsMap !== 'object') throw new Error('invalid exports')
function runtimeTarget(value, mode) {
  if (typeof value === 'string') return value
  if (!value || typeof value !== 'object') return null
  const conditions = mode === 'esm' ? ['node', 'import', 'default'] : ['node', 'require', 'default']
  for (const condition of Object.keys(value)) {
    if (conditions.includes(condition)) {
      const selected = runtimeTarget(value[condition], mode)
      if (selected) return selected
    }
  }
  return null
}
function walk(directory) {
  return fs.readdirSync(path.join(root, directory), { withFileTypes: true }).flatMap((entry) => {
    const relative = path.join(directory, entry.name)
    return entry.isDirectory() ? walk(relative) : [relative]
  })
}
function add(key, target, mode) {
  if (!target.startsWith('./') || target.includes('..')) throw new Error(`invalid export target ${target} for ${key}`)
  const wildcard = key.includes('*')
  if (!wildcard) {
    const full = path.join(root, target)
    if (!fs.existsSync(full) || !fs.statSync(full).isFile() || fs.statSync(full).size === 0)
      throw new Error(`export target missing or empty in tarball: ${key} -> ${target}`)
    const kind = /\.(?:js|mjs|cjs)$/i.test(target) ? 'js' : 'asset'
    console.log(`${kind}:${mode}\t${key === '.' ? name : name + '/' + key.slice(2)}\t${target}`)
    return
  }
  const [pre, post] = target.split('*')
  if (post === undefined) throw new Error(`invalid wildcard export target ${target}`)
  const matches = walk(path.dirname(pre)).filter((candidate) => {
    const rel = './' + candidate
    return rel.startsWith(pre) && rel.endsWith(post)
  })
  if (matches.length === 0) throw new Error(`wildcard export target missing in tarball: ${key} -> ${target}`)
  for (const candidate of matches) {
    const rel = './' + candidate
    const replacement = rel.slice(pre.length, rel.length - post.length)
    const entry = key.replace('*', replacement)
    const full = path.join(root, candidate)
    if (!fs.statSync(full).isFile() || fs.statSync(full).size === 0)
      throw new Error(`export target missing or empty in tarball: ${entry} -> ${rel}`)
    const kind = /\.(?:js|mjs|cjs)$/i.test(candidate) ? 'js' : 'asset'
    console.log(`${kind}:${mode}\t${name}/${entry.slice(2)}\t${rel}`)
  }
}
for (const [key, value] of Object.entries(exportsMap)) {
  for (const mode of ['esm', 'cjs']) {
    const target = runtimeTarget(value, mode)
    if (!target) throw new Error(`export has no ${mode} runtime target: ${key}`)
    add(key, target, mode)
  }
}
const bins = typeof manifest.bin === 'string' ? { [name.split('/').pop()]: manifest.bin } : manifest.bin ?? {}
for (const [bin, target] of Object.entries(bins)) {
  const full = path.join(root, target)
  if (!fs.existsSync(full) || !fs.statSync(full).isFile() || fs.statSync(full).size === 0)
    throw new Error(`bin target missing or empty in tarball: ${bin} -> ${target}`)
  console.log(`bin\t${bin}\t${target}`)
}
JS
}

is_optional_missing() {
  local err="$1"
  [ "${#optional_peers[@]}" -gt 0 ] || return 1
  node - "$err" "${optional_peers[@]}" <<'JS'
const fs = require('node:fs')
const text = fs.readFileSync(process.argv[2], 'utf8')
const peers = process.argv.slice(3)
const missing = [...text.matchAll(/Cannot find (?:package|module) ['"]([^'"\n]+)['"]/g)].map((match) => match[1])
process.exit(missing.some((specifier) => peers.some((peer) =>
  specifier === peer || specifier.startsWith(peer + '/')
)) ? 0 : 1)
JS
}

run_entry() {
  local dir="$1" mode="$2" entry="$3" err="$4"
  (
    cd "$dir"
    if [ "$mode" = esm ]; then
      ENTRY="$entry" node --input-type=module -e "await import(process.env.ENTRY)" 2>"$err"
    else
      ENTRY="$entry" node -e "require(process.env.ENTRY)" 2>"$err"
    fi
  )
}

check_consumer() {
  local dir="$1" allow_optional_missing="$2"
  local entries
  entries="$work/$(basename "$dir")-entries.tsv"
  enumerate_exports "$dir" > "$entries"
  [ -s "$entries" ] || { echo "::error::no export entries found"; return 1; }
  local kind entry target err mode
  while IFS=$'\t' read -r kind entry target; do
    mode="${kind#*:}"
    kind="${kind%%:*}"
    if [ "$kind" = asset ]; then
      if [ "$mode" = esm ]; then
        (cd "$dir" && ENTRY="$entry" node --input-type=module -e 'import { fileURLToPath } from "node:url"; import { statSync } from "node:fs"; const file = fileURLToPath(import.meta.resolve(process.env.ENTRY)); if (!statSync(file).isFile() || !statSync(file).size) process.exit(1)')
      else
        (cd "$dir" && ENTRY="$entry" node -e 'const fs = require("node:fs"); const file = require.resolve(process.env.ENTRY); if (!fs.statSync(file).isFile() || !fs.statSync(file).size) process.exit(1)')
      fi
      echo "$mode asset ok $entry -> $target"
      continue
    fi
    if [ "$kind" = bin ]; then
      (cd "$dir" && "node_modules/.bin/$entry" --version)
      echo "bin ok $entry"
      continue
    fi
    err="$work/error-${RANDOM}-${RANDOM}.txt"
    if run_entry "$dir" "$mode" "$entry" "$err"; then
      echo "$mode ok  $entry"
    elif [ "$allow_optional_missing" = true ] && [ "$entry" != "$name" ] && is_optional_missing "$err"; then
      echo "$mode expected optional peer miss  $entry"
    else
      echo "::error::$mode import failed: $entry"
      cat "$err"
      return 1
    fi
  done < "$entries"
}

# First prove the server-safe surface without any optional peer. The root is
# always required to pass; only a subpath whose actual error names a declared
# optional peer may defer to the full-peer consumer below.
no_peer="$work/no-peer"
install_consumer "$no_peer"
check_consumer "$no_peer" true

# Install every declared peer at its manifest range and prove every JS export.
# This is a separate clean consumer so npm's resolver cannot hide the negative
# case above by retaining an optional peer from another install.
with_peers="$work/with-peers"
install_consumer "$with_peers" "${peer_specs[@]}"
check_consumer "$with_peers" false
