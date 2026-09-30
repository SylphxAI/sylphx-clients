import { describe, expect, test } from 'bun:test'
import { type SylphxDataClient, sdkKv, sdkObjects } from '../src/index.js'

const notFound = Object.assign(new Error('nf'), { code: 'RESOURCE_NOT_FOUND', status: 404 })

describe('sdk adapters', () => {
	test('kv maps base64, ttl, batches of 1000, and treats not-found as absent', async () => {
		const calls: unknown[] = []
		const client = {
			data: {
				kv: {
					get: async () => {
						throw notFound
					},
					getMany: async (r: { keys: string[] }) => {
						calls.push(r.keys.length)
						return { values: r.keys.map((k) => (k === 'k0' ? { value: btoa('hi') } : {})) }
					},
					put: async (r: unknown) => void calls.push(r),
					delete: async () => {
						throw notFound
					},
				},
				objects: {},
			},
		} as unknown as SylphxDataClient
		const kv = sdkKv(client, 'ns')
		expect(await kv.get('a')).toBeUndefined()
		await kv.delete('a')
		await kv.set('k', new TextEncoder().encode('hi'), 90)
		expect(calls[0]).toEqual({ namespaceId: 'ns', key: 'k', value: 'aGk=', ttlSeconds: '90' })
		const keys = Array.from({ length: 1500 }, (_, i) => `k${i}`)
		const out = await kv.getMany(keys)
		expect(calls.slice(1)).toEqual([1000, 500])
		expect(out.length).toBe(1500)
		expect(new TextDecoder().decode(out[0])).toBe('hi')
		expect(out[1]).toBeUndefined()
	})

	test('objects maps base64 and not-found', async () => {
		let put: unknown
		const client = {
			data: {
				kv: {},
				objects: {
					get: async (r: { key: string }) => {
						if (r.key === 'gone') throw notFound
						return { body: btoa('bytes') }
					},
					put: async (r: unknown) => void (put = r),
					delete: async () => ({}),
				},
			},
		} as unknown as SylphxDataClient
		const o = sdkObjects(client, 'b')
		expect(await o.get('gone')).toBeUndefined()
		expect(new TextDecoder().decode(await o.get('x'))).toBe('bytes')
		await o.put('k', new TextEncoder().encode('bytes'))
		expect(put).toEqual({
			bucketId: 'b',
			key: 'k',
			body: btoa('bytes'),
			contentType: 'application/octet-stream',
		})
	})
})
