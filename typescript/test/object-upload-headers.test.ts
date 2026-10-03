import { expect, test } from 'bun:test'
import * as sdk from '../src/index.js'

test('object PUT uses authenticated JSON, never a typed Blob against a presigned URL', async () => {
	let captured: { url: URL; method: string; headers: Headers; body: unknown } | undefined
	const server = Bun.serve({
		port: 0,
		fetch: async (request) => {
			captured = {
				url: new URL(request.url),
				method: request.method,
				headers: request.headers,
				body: await request.json(),
			}
			return Response.json({ object: { bucketId: 'uploads', key: 'image.png' } })
		},
	})
	try {
		const client = new sdk.Sylphx({
			apiKey: 'test-api-key',
			origins: { 'https://api.data.sylphx.com': server.url.origin },
			maxRetries: 0,
		})
		const file = new Blob(['payload'], { type: 'image/png' })
		await client.data.objects.put({
			bucketId: 'uploads',
			key: 'image.png',
			body: Buffer.from(await file.arrayBuffer()).toString('base64'),
			contentType: file.type,
		})
		expect(captured?.method).toBe('PUT')
		expect(captured?.url.pathname).toBe('/v1/objects/uploads/image.png')
		expect(captured?.url.search).toBe('')
		expect(captured?.headers.get('authorization')).toBe('Bearer test-api-key')
		expect(captured?.headers.get('content-type')).toBe('application/json')
		expect(captured?.headers.has('sylphx-effect-id')).toBe(true)
		expect(captured?.body).toEqual({ body: 'cGF5bG9hZA==', content_type: 'image/png' })
		// The retired 0.27 storage.uploads API is not a drop-in upgrade surface.
		expect('storage' in sdk).toBe(false)
		expect('storage' in client).toBe(false)
	} finally {
		server.stop(true)
	}
})
