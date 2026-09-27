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

describe('data plane: the full KV profile', () => {
	const DATA = 'https://api.data.sylphx.com'

	test('every KV call goes to its path with its body, snake_case, and an effect id', async () => {
		const ok = (body: unknown) => ({ status: 200, body })
		const f = fake([
			ok({
				values: [
					{ namespaceId: 'cache', key: 'a', value: 'YQ==', version: '1' },
					{ namespaceId: 'cache', key: 'gone' },
				],
			}),
			ok({ added: '2', version: '1' }),
			ok({ value: 'dg==' }),
			ok({ fields: [{ field: 'name', value: 'dg==' }] }),
			ok({ fields: [{ field: 'name', value: 'dg==' }, { field: 'absent' }] }),
			ok({ length: '2', version: '3' }),
			ok({ values: ['YQ==', 'Yg=='] }),
			ok({ values: ['YQ=='], length: '1', version: '4' }),
			ok({ length: '1' }),
			ok({ added: '1', version: '1' }),
			ok({ members: [{ member: 'kyle', score: 42.5 }] }),
			ok({ score: 42.5 }),
			ok({ length: '1' }),
			ok({ keys: ['user:1', 'user:2'], cursor: 'c2' }),
			ok({ applied: true, version: '1', expiresAtUnixMs: '1790000060000' }),
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch })
		const kv = sx.data.kv
		const many = await kv.getMany({ namespaceId: 'cache', keys: ['a', 'gone'] })
		const set = await kv.hashSet({
			namespaceId: 'cache',
			key: 'user:1',
			fields: [
				{ field: 'name', value: 'dg==' },
				{ field: 'plan', value: 'cA==' },
			],
		})
		const one = await kv.hashGet({ namespaceId: 'cache', key: 'user:1', field: 'name' })
		const all = await kv.hashGetAll({ namespaceId: 'cache', key: 'user:1' })
		const some = await kv.hashGetMany({
			namespaceId: 'cache',
			key: 'user:1',
			fields: ['name', 'absent'],
		})
		const pushed = await kv.listPush({ namespaceId: 'q', key: 'jobs', values: ['YQ==', 'Yg=='] })
		const range = await kv.listRange({ namespaceId: 'q', key: 'jobs', start: '0', stop: '-1' })
		const popped = await kv.listPop({ namespaceId: 'q', key: 'jobs', count: '1' })
		const llen = await kv.listLength({ namespaceId: 'q', key: 'jobs' })
		const zadd = await kv.zsetAdd({
			namespaceId: 'board',
			key: 'top',
			members: [{ member: 'kyle', score: 42.5 }],
		})
		const zrange = await kv.zsetRange({
			namespaceId: 'board',
			key: 'top',
			stop: '9',
			withScores: true,
		})
		const zscore = await kv.zsetScore({ namespaceId: 'board', key: 'top', member: 'kyle' })
		const zlen = await kv.zsetLength({ namespaceId: 'board', key: 'top' })
		const scan = await kv.scan({ namespaceId: 'cache', glob: 'user:*', count: 100 })
		const expired = await kv.expire({ namespaceId: 'cache', key: 'a', ttlSeconds: '0' })

		expect(f.sent.map((s) => `${s.method} ${s.url}`)).toEqual([
			`POST ${DATA}/v1/kv-mget`,
			`POST ${DATA}/v1/kv-hashes`,
			`POST ${DATA}/v1/kv-hash-gets`,
			`POST ${DATA}/v1/kv-hash-getall`,
			`POST ${DATA}/v1/kv-hash-mget`,
			`POST ${DATA}/v1/kv-list-pushes`,
			`POST ${DATA}/v1/kv-list-ranges`,
			`POST ${DATA}/v1/kv-list-pops`,
			`POST ${DATA}/v1/kv-list-lens`,
			`POST ${DATA}/v1/kv-zsets`,
			`POST ${DATA}/v1/kv-zset-ranges`,
			`POST ${DATA}/v1/kv-zset-scores`,
			`POST ${DATA}/v1/kv-zset-lens`,
			`POST ${DATA}/v1/kv-scans`,
			`POST ${DATA}/v1/kv-expiries`,
		])
		expect(f.sent.map((s) => s.body)).toEqual([
			{ namespace_id: 'cache', keys: ['a', 'gone'] },
			{
				namespace_id: 'cache',
				key: 'user:1',
				fields: [
					{ field: 'name', value: 'dg==' },
					{ field: 'plan', value: 'cA==' },
				],
			},
			{ namespace_id: 'cache', key: 'user:1', field: 'name' },
			{ namespace_id: 'cache', key: 'user:1' },
			{ namespace_id: 'cache', key: 'user:1', fields: ['name', 'absent'] },
			{ namespace_id: 'q', key: 'jobs', values: ['YQ==', 'Yg=='] },
			{ namespace_id: 'q', key: 'jobs', start: '0', stop: '-1' },
			{ namespace_id: 'q', key: 'jobs', count: '1' },
			{ namespace_id: 'q', key: 'jobs' },
			{ namespace_id: 'board', key: 'top', members: [{ member: 'kyle', score: 42.5 }] },
			{ namespace_id: 'board', key: 'top', stop: '9', with_scores: true },
			{ namespace_id: 'board', key: 'top', member: 'kyle' },
			{ namespace_id: 'board', key: 'top' },
			{ namespace_id: 'cache', glob: 'user:*', count: 100 },
			// An explicit zero TTL (make persistent) is sent, not dropped.
			{ namespace_id: 'cache', key: 'a', ttl_seconds: '0' },
		])
		for (const s of f.sent) {
			expect(s.headers.authorization).toBe('Bearer sylphx_sk_test')
			expect(s.headers['sylphx-effect-id']).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-/)
		}
		expect(new Set(f.sent.map((s) => s.headers['sylphx-effect-id'])).size).toBe(f.sent.length)

		// Answers decode, camelCase included; an absent key or field has no value.
		expect(many.values?.map((v) => v.value)).toEqual(['YQ==', undefined])
		expect(set.added).toBe('2')
		expect(one.value).toBe('dg==')
		expect(all.fields?.[0]).toEqual({ field: 'name', value: 'dg==' })
		expect(some.fields?.[1]?.value).toBeUndefined()
		expect(pushed.length).toBe('2')
		expect(range.values).toEqual(['YQ==', 'Yg=='])
		expect(popped.values).toEqual(['YQ=='])
		expect(llen.length).toBe('1')
		expect(zadd.added).toBe('1')
		expect(zrange.members?.[0]?.score).toBe(42.5)
		expect(zscore.score).toBe(42.5)
		expect(zlen.length).toBe('1')
		expect(scan).toEqual({ keys: ['user:1', 'user:2'], cursor: 'c2' })
		expect(expired.expiresAtUnixMs).toBe('1790000060000')
	})

	test('a KV write and a KV read are each retried with their own effect id', async () => {
		const busy = { status: 503, body: { error: { code: 'unavailable', message: 'retry' } } }
		const f = fake([
			busy,
			{ status: 200, body: { values: ['YQ=='], length: '0', version: '2' } },
			busy,
			{ status: 200, body: { keys: [], cursor: '' } },
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch, maxRetries: 1 })
		const popped = await sx.data.kv.listPop({ namespaceId: 'q', key: 'jobs' })
		const scan = await sx.data.kv.scan({ namespaceId: 'q' })
		expect(popped.values).toEqual(['YQ=='])
		expect(scan.cursor).toBe('')
		const ids = f.sent.map((s) => s.headers['sylphx-effect-id'])
		expect(ids[1]).toBe(ids[0])
		expect(ids[3]).toBe(ids[2])
		expect(ids[2]).not.toBe(ids[0])
		// No count: Data pops one.
		expect(f.sent[0]!.body).toEqual({ namespace_id: 'q', key: 'jobs' })
		expect(f.sent[1]!.body).toEqual(f.sent[0]!.body)
	})
})

describe('data plane: search documents', () => {
	const DOC = {
		indexId: 'articles',
		documentId: 'post 1',
		documentJson: btoa('{"title":"hello"}'),
		vector: [0.1, 0.2],
		sha256: 'sha256:cd',
		version: '1',
		updatedAtUnixMs: '1790000000000',
	}

	test('documents put, get, delete use their paths; the id is percent-encoded', async () => {
		const f = fake([
			{ status: 200, body: { document: DOC } },
			{ status: 200, body: { document: DOC } },
			{ status: 200, body: { deleted: true } },
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch })
		const put = await sx.data.documents.put({
			indexId: 'articles',
			documentId: 'post 1',
			documentJson: btoa('{"title":"hello"}'),
			vector: [0.1, 0.2],
			expectedVersion: '0',
		})
		const got = await sx.data.documents.get({ indexId: 'articles', documentId: 'post 1' })
		const del = await sx.data.documents.delete({
			indexId: 'articles',
			documentId: 'post 1',
			expectedVersion: '1',
		})
		expect(f.sent.map((s) => `${s.method} ${s.url}`)).toEqual([
			'PUT https://api.data.sylphx.com/v1/documents/articles/post%201',
			'GET https://api.data.sylphx.com/v1/documents/articles/post%201',
			'DELETE https://api.data.sylphx.com/v1/documents/articles/post%201?expected_version=1',
		])
		expect(f.sent[0]!.body).toEqual({
			document_json: btoa('{"title":"hello"}'),
			vector: [0.1, 0.2],
			expected_version: '0',
		})
		expect(f.sent[1]!.body).toBeUndefined()
		expect(put.document?.documentId).toBe('post 1')
		expect(atob(got.document?.documentJson ?? '')).toBe('{"title":"hello"}')
		expect(got.document?.updatedAtUnixMs).toBe('1790000000000')
		expect(del.deleted).toBe(true)
		for (const s of f.sent) expect(s.headers['sylphx-effect-id']).toBeDefined()
	})

	test('search.query posts the query and decodes hits best first', async () => {
		const f = fake([
			{ status: 503, body: { error: { code: 'unavailable', message: 'retry' } } },
			{ status: 200, body: { hits: [{ document: DOC, score: 1.9 }] } },
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch, maxRetries: 1 })
		const out = await sx.data.search.query({
			indexId: 'articles',
			query: 'hello',
			vector: [0.1, 0.2],
			limit: 5,
		})
		expect(f.sent).toHaveLength(2)
		expect(f.sent[0]!.method).toBe('POST')
		expect(f.sent[0]!.url).toBe('https://api.data.sylphx.com/v1/search/articles')
		expect(f.sent[0]!.body).toEqual({ query: 'hello', vector: [0.1, 0.2], limit: 5 })
		expect(f.sent[1]!.headers['sylphx-effect-id']).toBe(f.sent[0]!.headers['sylphx-effect-id'])
		expect(out.hits?.[0]?.score).toBe(1.9)
		expect(out.hits?.[0]?.document?.indexId).toBe('articles')
	})

	test('a search index id with a slash is refused before any call', async () => {
		const f = fake([])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch })
		await expect(sx.data.search.query({ indexId: 'a/b', query: 'x' })).rejects.toThrow(TypeError)
		await expect(sx.data.documents.get({ indexId: 'articles', documentId: 'a/b' })).rejects.toThrow(
			TypeError,
		)
		expect(f.sent).toHaveLength(0)
	})
})
