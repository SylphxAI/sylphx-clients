import { expect, test } from 'bun:test'
import { getObjectBytes, putObjectBytes } from '../src/data/objects.js'
import { Sylphx, SylphxError } from '../src/index.js'

const target = { bucketId: 'uploads', key: 'files/space +.bin' }
const origin = 'https://api.data.sylphx.com'

test('byte helpers round-trip a typed array view over the existing HTTP API', async () => {
	// Across a base64 chunk boundary, with a nonzero byte offset and padding.
	const data = Uint8Array.from({ length: 3 * 24 * 1024 + 5 }, (_, i) => (i * 73) % 256)
	const bytes = data.subarray(2, -2)
	let call = 0
	const server = Bun.serve({
		port: 0,
		async fetch(request) {
			call++
			const url = new URL(request.url)
			expect(url.pathname).toBe('/v1/objects/uploads/files/space%20%2B.bin')
			expect(request.headers.has('authorization')).toBe(true)
			expect(request.headers.has('sylphx-effect-id')).toBe(true)
			if (request.method === 'PUT') {
				expect(request.headers.get('content-type')).toBe('application/json')
				expect(await request.json()).toEqual({
					body: Buffer.from(bytes).toString('base64'),
					content_type: 'application/octet-stream',
					expected_version: '5',
				})
				return Response.json({ object: { key: target.key, version: '6' } })
			}
			expect(request.method).toBe('GET')
			expect(url.searchParams.get('version')).toBe('6')
			return Response.json({
				object: { key: target.key, version: '6' },
				body: Buffer.from(bytes).toString('base64'),
			})
		},
	})
	try {
		const {
			data: { objects },
		} = new Sylphx({
			apiKey: 'fixture-key',
			origins: { [origin]: server.url.origin },
			maxRetries: 0,
		})
		const request = {
			...target,
			body: bytes,
			contentType: 'application/octet-stream',
			expectedVersion: '5',
		}
		expect((await putObjectBytes(objects, request)).object?.version).toBe('6')
		const downloaded = await getObjectBytes(objects, { ...target, version: '6' })
		expect(downloaded.body).toEqual(bytes)
		expect(downloaded.object?.key).toBe(target.key)
		expect(call).toBe(2)
	} finally {
		server.stop(true)
	}
})

test('Blob media type is an overridable default and empty bodies stay empty', async () => {
	const put = async (request: { body?: string; contentType?: string }) => {
		expect(request.body).toBe('')
		return { object: { contentType: request.contentType } }
	}
	const body = new Blob([], { type: 'image/png' })
	expect((await putObjectBytes({ put }, { ...target, body })).object?.contentType).toBe('image/png')
	const override = await putObjectBytes(
		{ put },
		{ ...target, body, contentType: 'application/octet-stream' },
	)
	expect(override.object?.contentType).toBe('application/octet-stream')
	const empty = await getObjectBytes({ get: async () => ({ body: '' }) }, target)
	expect(empty.body).toEqual(new Uint8Array())
})

test('missing or malformed bytes are not reported as empty downloads', async () => {
	await expect(getObjectBytes({ get: async () => ({}) }, target)).rejects.toThrow(
		'missing its base64 body',
	)
	await expect(getObjectBytes({ get: async () => ({ body: '%%%%' }) }, target)).rejects.toThrow()
})

test('typed server denials remain actionable', async () => {
	const {
		data: { objects },
	} = new Sylphx({
		apiKey: 'fixture-key',
		maxRetries: 0,
		fetch: async () => new Response(null, { status: 403 }),
	})
	const failure = await getObjectBytes(objects, target).catch((error: unknown) => error)
	expect(failure).toBeInstanceOf(SylphxError)
	expect(failure).toMatchObject({ status: 403, code: 'PERMISSION_DENIED' })
})
