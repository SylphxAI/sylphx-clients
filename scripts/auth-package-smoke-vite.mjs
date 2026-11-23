import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { readFile, writeFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { promisify } from 'node:util'
import { generateHydrationScript, renderToString } from 'solid-js/web'
import { build } from 'vite'
import solid from 'vite-plugin-solid'

const widget = 'auth-package-smoke-solid.tsx'
const config = { providers: ['apple'], password: false, signup: false, otp: true, passkey: false }
const common = () => ({ configFile: false, plugins: [solid({ ssr: true })] })
await build({
	...common(),
	build: {
		ssr: widget,
		outDir: 'out-ssr',
		rollupOptions: { output: { entryFileNames: 'entry.mjs' } },
	},
})
const { page } = await import(pathToFileURL(resolve('out-ssr/entry.mjs')).href)
const rendered = renderToString(page)
assert.match(rendered, /<section[^>]*class="auth-sign-in-panel"/)
assert.match(rendered, /<h1[^>]*>Package sign-in fixture<\/h1>/)

// Hydrate the very markup emitted by the installed server entry and read the
// installed stylesheet through its public subpath, with no resolver aliases.
await writeFile(
	'index.html',
	`<!doctype html><html><head><meta charset="utf-8">
${generateHydrationScript()}</head><body><main id="root">${rendered}</main>
<script type="module">
import { hydrate } from 'solid-js/web'
import { page } from './${widget}'
import '@sylphx/auth-solid/sign-in.css'
const root = document.getElementById('root')
const ssr = root.querySelector('section')
const observer = new MutationObserver(() => {
	const form = root.querySelector('input[type=email]')
	if (form) {
		if (ssr !== root.querySelector('section') || getComputedStyle(ssr).display !== 'grid') {
			throw new Error('SSR markup was replaced or package stylesheet is inactive')
		}
		observer.disconnect()
		document.body.dataset.authPackageSmoke = 'passed'
	}
})
observer.observe(root, { childList: true, subtree: true })
hydrate(page, root)
</script></body></html>`,
)
const bundle = await build({ ...common(), build: { outDir: 'out-browser' } })
const output = [bundle].flatMap((result) => result.output)
const manifest = JSON.parse(await readFile('node_modules/@sylphx/auth-solid/package.json', 'utf8'))
const widgetDir = 'node_modules/@sylphx/auth-solid'
const modules = new Set(
	output.flatMap((result) => (result.type === 'chunk' ? result.moduleIds : [])),
)
assert.ok(
	modules.has(resolve(widgetDir, manifest.exports['.'].browser)),
	'Vite did not select browser export',
)
assert.ok(
	!modules.has(resolve(widgetDir, manifest.exports['.'].default)),
	'Vite bundled the SSR export',
)
assert.ok(
	output.some(
		(file) => file.fileName.endsWith('.css') && String(file.source).includes('.auth-sign-in-panel'),
	),
	'CSS missing from production bundle',
)
console.info(
	'Solid: Vite SSR rendered in Node; browser export and CSS included in production build',
)

const chromium = process.argv[2]
if (chromium) {
	const route = new Map([
		['/', ['text/html; charset=utf-8', await readFile('out-browser/index.html')]],
		['/auth/config', ['application/json', JSON.stringify(config)]],
	])
	for (const file of output) {
		if (file.type === 'chunk' || file.fileName.endsWith('.css')) {
			route.set(`/${file.fileName}`, [
				file.type === 'chunk' ? 'text/javascript' : 'text/css',
				await readFile(`out-browser/${file.fileName}`),
			])
		}
	}
	const server = createServer((request, response) => {
		const file = route.get(request.url)
		if (!file) return response.writeHead(404).end()
		response.writeHead(200, { 'Content-Type': file[0], 'Cache-Control': 'no-store' }).end(file[1])
	})
	await new Promise((listen, reject) => server.once('error', reject).listen(0, '127.0.0.1', listen))
	try {
		const { stdout } = await promisify(execFile)(
			chromium,
			[
				'--headless',
				'--disable-background-networking',
				'--no-first-run',
				`--user-data-dir=${resolve('chromium-profile')}`,
				'--virtual-time-budget=10000',
				'--dump-dom',
				`http://127.0.0.1:${server.address().port}/`,
			],
			{ timeout: 30_000 },
		)
		assert.match(stdout, /<body data-auth-package-smoke="passed">/)
		console.info(
			'Chromium: hydrated the SSR element, fetched config, rendered email input and applied package CSS',
		)
	} finally {
		server.closeAllConnections()
		await new Promise((close) => server.close(close))
	}
}
