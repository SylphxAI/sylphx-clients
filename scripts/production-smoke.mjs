// Run only inside a fresh npm consumer, never the source checkout.
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import { createRequire } from 'node:module'
import path from 'node:path'
import { pathToFileURL } from 'node:url'

const root = process.cwd()
const require = createRequire(path.join(root, 'package.json'))
const cli = path.join(root, 'node_modules/.bin/sylphx')
const run = (args) => {
	const result = spawnSync(cli, args, {
		encoding: 'utf8',
		timeout: 30000,
		maxBuffer: 16 * 1024 * 1024,
	})
	if (result.error) throw result.error
	return result
}
const version = run(['--version'])
assert.equal(version.status, 0, version.stderr)
assert.equal(version.stdout.trim(), `sylphx ${process.env.CLI_VERSION}`)
// Empty explicit credential avoids the CLI's local "not signed in" shortcut.
// It sends no usable credential and must reach production's 401 error parser.
const denied = run(['--base-url', 'https://api.sylphx.com', '--api-key', '', 'whoami'])
assert.equal(denied.status, 1)
assert.match(denied.stderr, /UNAUTHENTICATED \(401\): missing_authorization/)
const schema = run([
	'--base-url',
	'https://api.sylphx.com',
	'--api-key',
	'',
	'api',
	'GET',
	'/openapi.json',
])
assert.equal(schema.status, 0, schema.stderr)
const document = JSON.parse(schema.stdout)
assert.equal(document.info.title, 'Sylphx API')
assert.ok(document.paths['/v1/whoami'])
console.info('production CLI: exact version, generated API schema, unauthenticated whoami 401')

const sdkRoot = path.join(root, 'node_modules/@sylphx/sdk')
const pkg = JSON.parse(fs.readFileSync(path.join(sdkRoot, 'package.json'), 'utf8'))
assert.equal(pkg.version, process.env.SDK_VERSION)
const walk = (dir) =>
	fs
		.readdirSync(path.join(sdkRoot, dir), { withFileTypes: true })
		.flatMap((e) => (e.isDirectory() ? walk(path.join(dir, e.name)) : [path.join(dir, e.name)]))
const entries = []
for (const [key, target] of Object.entries(pkg.exports)) {
	const file =
		typeof target === 'string' ? target : (target.default ?? target.import ?? target.require)
	if (file.endsWith('.css')) {
		assert.ok(fs.statSync(path.join(sdkRoot, file)).size > 0)
		continue // CSS is a browser asset, not a Node module.
	}
	if (!key.includes('*')) entries.push(key === '.' ? '@sylphx/sdk' : `@sylphx/sdk/${key.slice(2)}`)
	else {
		const [pre, post] = file.split('*')
		for (const f of walk(path.dirname(pre))) {
			const relative = `./${f}`
			if (relative.startsWith(pre) && relative.endsWith(post))
				entries.push(
					`@sylphx/sdk/${key.slice(2).replace('*', relative.slice(pre.length, relative.length - post.length))}`,
				)
		}
	}
}
assert.ok(entries.length > 0)
for (const entry of entries) {
	await import(pathToFileURL(require.resolve(entry)).href)
	require(entry)
}
const { Sylphx, SylphxError } = require('@sylphx/sdk')
try {
	await new Sylphx({
		baseUrl: 'https://api.sylphx.com',
		timeoutMs: 30000,
		maxRetries: 0,
	}).access.whoami({})
	assert.fail('production accepted unauthenticated whoami')
} catch (error) {
	assert.ok(error instanceof SylphxError, String(error))
	assert.equal(error.status, 401)
	assert.equal(error.code, 'UNAUTHENTICATED')
}
console.info(
	`production SDK: ${pkg.version}, ${entries.length} ESM/CJS entry points, unauthenticated whoami 401`,
)
