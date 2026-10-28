// Optional real-browser integration check. Build the SDK first, install playwright
// and its Chromium browser, then run: node test/browser-upload.smoke.mjs
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { createRequire } from 'node:module'

const { chromium } = createRequire(import.meta.url)('playwright')

const parts = new Map()
const puts = []
let fail = true
let completedHash
const server = createServer(async (request, response) => {
	const url = new URL(request.url, 'http://localhost')
	if (url.pathname === '/helper.js') {
		response.setHeader('Content-Type', 'text/javascript')
		response.end(await readFile(new URL('../dist/browser-upload.js', import.meta.url)))
	} else if (request.method === 'PUT') {
		const number = Number(url.pathname.slice(1))
		puts.push(number)
		const chunks = []
		for await (const chunk of request) chunks.push(chunk)
		if (number === 2 && fail) {
			fail = false
			response.writeHead(503).end()
		} else {
			parts.set(number, Buffer.concat(chunks))
			response.setHeader('ETag', `"part-${number}"`)
			response.end()
		}
	} else if (url.pathname === '/parts') {
		response.setHeader('Content-Type', 'application/json')
		response.end(
			JSON.stringify(
				[...parts].map(([partNumber, body]) => ({
					partNumber,
					size: body.length,
					etag: `"part-${partNumber}"`,
				})),
			),
		)
	} else if (url.pathname === '/complete') {
		completedHash = createHash('sha256')
			.update(Buffer.concat([...parts].sort(([a], [b]) => a - b).map(([, body]) => body)))
			.digest('hex')
		response.end(completedHash)
	} else {
		response.setHeader('Content-Type', 'text/html')
		response.end('<!doctype html><title>Upload integration</title>')
	}
})
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
const origin = `http://127.0.0.1:${server.address().port}`
let browser
try {
	browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] })
	const page = await browser.newPage()
	await page.goto(origin)
	const run = async () =>
		page.evaluate(async () => {
			const { uploadBrowserFile, localUploadCheckpoint } = await import('/helper.js')
			const checkpoint = localUploadCheckpoint('test-upload')
			const file = new Blob([
				new Uint8Array(5 * 1024 * 1024).fill(17),
				new Uint8Array(193).fill(23),
			])
			const progress = []
			const backend = {
				put: async () => ({ url: `${location.origin}/0` }),
				create: async () => 'upload-id',
				part: async ({ partNumber }) => ({ url: `${location.origin}/${partNumber}` }),
				listParts: async () => (await fetch('/parts')).json(),
				complete: async () => {
					await fetch('/complete')
				},
				abort: async () => {},
			}
			try {
				const result = await uploadBrowserFile({
					key: 'file',
					file,
					backend,
					checkpoint,
					onProgress: (p) => progress.push(p),
				})
				return { result, progress, checkpoint: await checkpoint.load() }
			} catch (error) {
				return { error: error.message, progress, checkpoint: await checkpoint.load() }
			}
		})
	const first = await run()
	assert.match(first.error, /503/)
	assert.equal(first.checkpoint.parts.length, 1)
	// A real navigation destroys JS state; only localStorage and remote parts survive.
	await page.reload()
	const second = await run()
	assert.equal(second.error, undefined)
	assert.equal(second.checkpoint, null)
	assert.deepEqual(puts, [1, 2, 2])
	assert.equal(second.result.sha256, completedHash)
	assert.equal(second.result.size, 5 * 1024 * 1024 + 193)
	assert.ok(second.progress.some((p) => p.resumed && p.loaded === 5 * 1024 * 1024))
	assert.ok(first.progress.some((p) => p.loaded > 0))
	process.stdout.write(
		JSON.stringify({
			browser: await browser.version(),
			puts,
			checksum: completedHash,
			bytes: second.result.size,
			progressEvents: first.progress.length + second.progress.length,
			reloadResume: true,
		}),
	)
} finally {
	await browser?.close()
	await new Promise((resolve) => server.close(resolve))
}
