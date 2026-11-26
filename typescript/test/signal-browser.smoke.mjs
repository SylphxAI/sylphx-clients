// Optional HTTPS smoke. Build the SDK and install Playwright + Chromium first.
// Run: node test/signal-browser.smoke.mjs (or set CHROMIUM_PATH to a local Chromium).
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { createServer } from 'node:https'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const { chromium } = createRequire(import.meta.url)('playwright')
const tls = await mkdtemp(join(tmpdir(), 'signal-smoke-'))
const events = []
let server
let browser
try {
	const key = join(tls, 'key.pem')
	const cert = join(tls, 'cert.pem')
	execFileSync(
		'openssl',
		[
			'req',
			'-x509',
			'-newkey',
			'rsa:2048',
			'-nodes',
			'-days',
			'1',
			'-subj',
			'/CN=localhost',
			'-keyout',
			key,
			'-out',
			cert,
		],
		{ stdio: 'ignore' },
	)
	server = createServer(
		{ key: await readFile(key), cert: await readFile(cert) },
		async (req, res) => {
			const pathname = req.url
			if (pathname === '/v1/analytics/events:batch' && req.method === 'POST') {
				const chunks = []
				for await (const chunk of req) chunks.push(chunk)
				events.push(...JSON.parse(Buffer.concat(chunks).toString()).events)
				res.writeHead(202).end()
			} else if (pathname && /^\/sdk\/signal(?:\/[a-z]+)?\.js$/.test(pathname)) {
				res.writeHead(200, { 'Content-Type': 'text/javascript' })
				res.end(await readFile(new URL(`../dist/${pathname.slice(5)}`, import.meta.url)))
			} else {
				res
					.writeHead(200, { 'Content-Type': 'text/html' })
					.end('<!doctype html><title>Signal HTTPS smoke</title>')
			}
		},
	)
	await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
	const origin = `https://127.0.0.1:${server.address().port}`
	browser = await chromium.launch({ headless: true, executablePath: process.env.CHROMIUM_PATH })
	const context = await browser.newContext({ ignoreHTTPSErrors: true })
	const page = await context.newPage()
	const init = async (tab, gpc = false) =>
		tab.evaluate(async (gpc) => {
			const { createSignalBrowser } = await import('/sdk/signal.js')
			window.signalSmoke = createSignalBrowser({
				requireExplicitConsent: true,
				gpc,
				ingest: { apiKey: ['sylphx', 'pk', 'test'].join('_'), baseUrl: location.origin },
			})
		}, gpc)
	const view = (tab, path) => tab.evaluate((path) => window.signalSmoke.pageView(path), path)
	const storage = (tab) =>
		tab.evaluate(() => ({
			anon: localStorage.getItem('sylphx.signal.v1.anonymous'),
			session: sessionStorage.getItem('sylphx.signal.v1.session'),
			cookie: document.cookie,
		}))
	const empty = { anon: null, session: null, cookie: '' }
	await page.goto(origin)
	assert.equal(await page.evaluate(() => isSecureContext), true)
	await init(page)
	assert.equal(await view(page, '/pre-grant'), false)
	assert.deepEqual(await storage(page), empty)
	assert.equal(events.length, 0)
	await page.evaluate(() => window.signalSmoke.setConsent('analytics_only'))
	assert.equal(await view(page, '/first'), true)
	await page.reload()
	await init(page)
	assert.equal(await view(page, '/reload'), true)
	assert.equal(events[1].distinct_id, events[0].distinct_id)
	assert.equal(events[1].session_id, events[0].session_id)
	const tab = await context.newPage()
	await tab.goto(origin)
	await init(tab)
	assert.equal(await view(tab, '/other-tab'), true)
	assert.equal(events[2].distinct_id, events[0].distinct_id)
	assert.notEqual(events[2].session_id, events[0].session_id)
	await tab.evaluate(() => window.signalSmoke.setConsent('denied'))
	assert.deepEqual(await storage(tab), empty)
	await page.waitForFunction(() => !sessionStorage.getItem('sylphx.signal.v1.session'))
	assert.deepEqual(await storage(page), empty)
	assert.equal(await view(page, '/withdrawn'), false)
	await init(tab, true)
	await tab.evaluate(() => window.signalSmoke.setConsent('analytics_only'))
	assert.equal(await view(tab, '/gpc'), false)
	assert.deepEqual(await storage(tab), empty)
	assert.equal(events.length, 3)
	process.stdout.write(
		JSON.stringify({
			chromium: await browser.version(),
			https: true,
			reload: true,
			crossTab: true,
			withdrawal: true,
			gpc: true,
			acceptedEvents: events.length,
		}),
	)
} finally {
	await browser?.close()
	if (server?.listening) await new Promise((resolve) => server.close(resolve))
	await rm(tls, { recursive: true, force: true })
}
