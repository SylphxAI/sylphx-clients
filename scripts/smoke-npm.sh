#!/usr/bin/env bash
# Publish-time smoke test for an npm package: packs it, installs the tarball
# into a clean project, and loads every entry point with `import` (ESM) and
# `require` (CJS). A package that fails here is never published.
#   scripts/smoke-npm.sh <package dir>
set -euo pipefail
pkg="$(cd "$1" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
(cd "$pkg" && npm pack --silent --pack-destination "$work" >/dev/null)
tgz="$(ls "$work"/*.tgz)"
name="$(jq -r .name "$pkg/package.json")"
mkdir "$work/app"
cd "$work/app"
echo '{"name":"smoke","private":true}' > package.json
npm install --silent --no-audit --no-fund "$tgz"
# Entry points: every exported subpath; a `./*` pattern expands to each
# module file the tarball ships under its target directory.
node - "$name" <<'JS' > entries.txt
const [name] = process.argv.slice(2)
const fs = require('node:fs'), path = require('node:path')
const root = path.join('node_modules', name)
const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
const exp = pkg.exports ?? { '.': pkg.main }
for (const [key, target] of Object.entries(exp)) {
  const file = typeof target === 'string' ? target : (target.default ?? target.import ?? target.require)
  if (!key.includes('*')) { console.log(key === '.' ? name : `${name}/${key.slice(2)}`); continue }
  const [pre, post] = file.split('*')
  const walk = (d) => fs.readdirSync(path.join(root, d), { withFileTypes: true }).flatMap((e) =>
    e.isDirectory() ? walk(path.join(d, e.name)) : [path.join(d, e.name)])
  for (const f of walk(path.dirname(pre))) {
    const rel = `./${f}`
    if (rel.startsWith(pre) && rel.endsWith(post)) console.log(`${name}/${key.slice(2).replace('*', rel.slice(pre.length, rel.length - post.length))}`)
  }
}
JS
fail=0
while read -r entry; do
  node --input-type=module -e "await import('$entry')" 2>err.txt && echo "esm ok  $entry" || { echo "::error::esm import failed: $entry"; cat err.txt; fail=1; }
  node -e "require('$entry')" 2>err.txt && echo "cjs ok  $entry" || { echo "::error::cjs require failed: $entry"; cat err.txt; fail=1; }
done < entries.txt
[ -s entries.txt ] || { echo "::error::no entry points found"; exit 1; }
exit $fail
