// Sylphx Data's object and KV data plane through the generated SDK: served at
// https://api.data.sylphx.com from SYLPHX_API_KEY alone, one Sylphx-Effect-Id
// per call (reused on retries), ProtoJSON (camelCase) answers decoded, and
// Data's error body mapped to a SylphxError.

import { afterEach, describe, expect, test } from 'bun:test'
import { type FetchLike, Sylphx, SylphxError } from '../src/index.js'

interface Sent {
	method: string
	url: string
	headers: Record<string, string>
	body: unknown
}

function fake(replies: { status: number; body: unknown }[]) {
	const sent: Sent[] = []
	const impl: FetchLike = async (input, init) => {
		sent.push({
			method: init?.method ?? 'GET',
			url: String(input),
			headers: (init?.headers ?? {}) as Record<string, string>,
			body: init?.body == null ? undefined : JSON.parse(String(init.body)),
		})
		const r = replies.shift()
		if (r === undefined) throw new Error('no reply queued')
		return new Response(JSON.stringify(r.body), { status: r.status })
	}
	return { sent, fetch: impl }
}

const OBJECT = {
	bucketId: 'uploads',
	key: 'org_1/cv 2026+final.pdf',
	sha256: 'sha256:ab',
	size: '5',
	contentType: 'application/pdf',
	version: '3',
	updatedAtUnixMs: '1790000000000',
}

const saved: Record<string, string | undefined> = {}
function setEnv(name: string, value: string | undefined): void {
	if (!(name in saved)) saved[name] = process.env[name]
	if (value === undefined) delete process.env[name]
	else process.env[name] = value
}
afterEach(() => {
	for (const [k, v] of Object.entries(saved)) {
		if (v === undefined) delete process.env[k]
		else process.env[k] = v
	}
})

describe('data plane', () => {
	test('objects.put goes to Data with the key and an effect id, the key percent-encoded', async () => {
		const f = fake([{ status: 200, body: { object: OBJECT } }])
		setEnv('SYLPHX_API_KEY', 'sylphx_sk_live_env')
		const sx = new Sylphx({ fetch: f.fetch })
		const out = await sx.data.objects.put({
			bucketId: 'uploads',
			key: 'org_1/cv 2026+final.pdf',
			body: btoa('hello'),
			contentType: 'application/pdf',
		})
		const s = f.sent[0]!
		expect(s.method).toBe('PUT')
		expect(s.url).toBe('https://api.data.sylphx.com/v1/objects/uploads/org_1/cv%202026%2Bfinal.pdf')
		expect(s.headers.authorization).toBe('Bearer sylphx_sk_live_env')
		expect(s.headers['sylphx-effect-id']).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-/)
		expect(s.headers['idempotency-key']).toBeUndefined()
		expect(s.body).toEqual({ body: 'aGVsbG8=', content_type: 'application/pdf' })
		// Data answers ProtoJSON: camelCase names decode like snake_case ones.
		expect(out.object?.contentType).toBe('application/pdf')
		expect(out.object?.updatedAtUnixMs).toBe('1790000000000')
	})

	test('a retried call reuses its effect id; each call gets a new one', async () => {
		const f = fake([
			{ status: 503, body: { error: { code: 'unavailable', message: 'retry' } } },
			{ status: 200, body: { object: OBJECT, body: 'aGVsbG8=' } },
			{ status: 200, body: { object: OBJECT, body: 'aGVsbG8=' } },
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch, maxRetries: 1 })
		const got = await sx.data.objects.get({ bucketId: 'uploads', key: 'a/b.txt' })
		expect(atob(got.body ?? '')).toBe('hello')
		await sx.data.objects.get({ bucketId: 'uploads', key: 'a/b.txt' })
		const [first, retry, next] = f.sent.map((s) => s.headers['sylphx-effect-id'])
		expect(retry).toBe(first)
		expect(next).not.toBe(first)
		expect(f.sent[0]!.url).toBe('https://api.data.sylphx.com/v1/objects/uploads/a/b.txt')
	})

	test('a mutation on the data plane is retried with the same effect id', async () => {
		const f = fake([
			{ status: 503, body: { error: { code: 'unavailable', message: 'retry' } } },
			{
				status: 200,
				body: { value: { namespaceId: 'ratelimit', key: 'ip:1', value: btoa('1'), version: '1' } },
			},
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch, maxRetries: 1 })
		const out = await sx.data.kv.increment({ namespaceId: 'ratelimit', key: 'ip:1' })
		expect(atob(out.value?.value ?? '')).toBe('1')
		expect(f.sent).toHaveLength(2)
		expect(f.sent[1]!.headers['sylphx-effect-id']).toBe(f.sent[0]!.headers['sylphx-effect-id'])
		expect(f.sent[0]!.url).toBe('https://api.data.sylphx.com/v1/kv-increments')
		expect(f.sent[0]!.body).toEqual({ namespace_id: 'ratelimit', key: 'ip:1' })
	})

	test('kv put, get, list, delete use their paths; list pages decode camelCase', async () => {
		const f = fake([
			{ status: 200, body: { value: { namespaceId: 'cache', key: 'k' } } },
			{ status: 200, body: { value: { namespaceId: 'cache', key: 'k', value: 'dg==' } } },
			{ status: 200, body: { values: [{ namespaceId: 'cache', key: 'k' }], nextPageToken: 'p2' } },
			{ status: 200, body: { deleted: true } },
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch })
		await sx.data.kv.put({ namespaceId: 'cache', key: 'k', value: 'dg==', ttlSeconds: '0' })
		await sx.data.kv.get({ namespaceId: 'cache', key: 'k' })
		const page = await sx.data.kv.list({ namespaceId: 'cache', prefix: 'k', pageSize: 10 })
		const del = await sx.data.kv.delete({ namespaceId: 'cache', key: 'k' })
		expect(f.sent.map((s) => `${s.method} ${s.url}`)).toEqual([
			'PUT https://api.data.sylphx.com/v1/kv/cache/k',
			'GET https://api.data.sylphx.com/v1/kv/cache/k',
			'GET https://api.data.sylphx.com/v1/kv/cache?prefix=k&page_size=10',
			'DELETE https://api.data.sylphx.com/v1/kv/cache/k',
		])
		// An explicit zero TTL (never expire) is sent, not dropped.
		expect(f.sent[0]!.body).toEqual({ value: 'dg==', ttl_seconds: '0' })
		expect(page.nextPageToken).toBe('p2')
		expect(page.values?.[0]?.namespaceId).toBe('cache')
		expect(del.deleted).toBe(true)
		for (const s of f.sent) expect(s.headers['sylphx-effect-id']).toBeDefined()
	})

	test("Data's error body becomes a SylphxError with a registry code", async () => {
		const f = fake([
			{
				status: 404,
				body: { error: { code: 'not_found', message: 'object not found', occurrenceId: 'occ_1' } },
			},
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch })
		const err = await sx.data.objects.get({ bucketId: 'uploads', key: 'missing' }).catch((e) => e)
		expect(err).toBeInstanceOf(SylphxError)
		expect(err.code).toBe('RESOURCE_NOT_FOUND')
		expect(err.status).toBe(404)
		expect(err.detail).toBe('not_found: object not found')
	})

	test('a key with an empty segment is refused before any call', async () => {
		const f = fake([])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch })
		await expect(sx.data.objects.get({ bucketId: 'uploads', key: 'a//b' })).rejects.toThrow(
			TypeError,
		)
		await expect(sx.data.objects.get({ bucketId: 'a/b', key: 'k' })).rejects.toThrow(TypeError)
		expect(f.sent).toHaveLength(0)
	})

	test('a retired per-project host is ignored with a warning; SYLPHX_URL is never read', async () => {
		const warnings: string[] = []
		const warn = console.warn
		console.warn = (m: string) => warnings.push(m)
		try {
			setEnv('SYLPHX_BASE_URL', 'https://gold-time-8ea5.api.sylphx.com')
			setEnv('SYLPHX_URL', 'sylphx://pk_x:sk_y@gold-time-8ea5.api.sylphx.com')
			const f = fake([
				{ status: 200, body: { apiKeys: [] } },
				{ status: 200, body: { object: OBJECT } },
			])
			const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch })
			await sx.access.apiKeys.list({ parent: 'orgs/org_a/projects/prj_a/envs/env_a' })
			await sx.data.objects.get({ bucketId: 'uploads', key: 'k' })
			expect(new URL(f.sent[0]!.url).origin).toBe('https://api.sylphx.com')
			expect(new URL(f.sent[1]!.url).origin).toBe('https://api.data.sylphx.com')
			expect(warnings.some((w) => w.includes('gold-time-8ea5.api.sylphx.com'))).toBe(true)
			expect(warnings.some((w) => w.includes('SYLPHX_URL is retired'))).toBe(true)
		} finally {
			console.warn = warn
		}
	})
})
