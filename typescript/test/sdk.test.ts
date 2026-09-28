// The generated SDK speaks the wire of spec §3: camelCase in, snake_case on
// the wire, problem bodies as SylphxError, retries with one idempotency key.

import { describe, expect, test } from 'bun:test'
import { type FetchLike, METHODS, Sylphx, SylphxError } from '../src/index.js'

interface Sent {
	method: string
	url: URL
	headers: Record<string, string>
	body: unknown
}

function fake(replies: { status: number; body: unknown; headers?: Record<string, string> }[]) {
	const sent: Sent[] = []
	const impl: FetchLike = async (input, init) => {
		sent.push({
			method: init?.method ?? 'GET',
			url: new URL(String(input)),
			headers: (init?.headers ?? {}) as Record<string, string>,
			body: init?.body == null ? undefined : JSON.parse(String(init.body)),
		})
		const r = replies.shift()
		if (r === undefined) throw new Error('no reply queued')
		return new Response(JSON.stringify(r.body), {
			status: r.status,
			headers: { 'sylphx-request-id': `req_${r.status}`, ...(r.headers ?? {}) },
		})
	}
	return { sent, fetch: impl }
}

const ENV = 'orgs/org_a/projects/prj_a/envs/env_a'

describe('@sylphx/sdk', () => {
	test('create converts camelCase to the snake_case wire and back', async () => {
		const f = fake([
			{
				status: 200,
				body: {
					name: `${ENV}/api_keys/key_a`,
					meta: { display_name: 'CI', generation: '2' },
					spec: { kind: 'secret', scopes: ['data:read'] },
				},
			},
		])
		const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: f.fetch, baseUrl: 'https://api.test' })
		const key = await sx.access.apiKeys.create({
			parent: ENV,
			apiKey: { meta: { displayName: 'CI' }, spec: { kind: 'secret', scopes: ['data:read'] } },
		})
		expect(key.meta?.displayName).toBe('CI')
		const s = f.sent[0]!
		expect(s.method).toBe('POST')
		expect(s.url.pathname).toBe(`/v1/${ENV}/api_keys`)
		expect(s.body).toEqual({
			meta: { display_name: 'CI' },
			spec: { kind: 'secret', scopes: ['data:read'] },
		})
		expect(s.headers.authorization).toBe('Bearer sylphx_sk_test')
		expect(s.headers['idempotency-key']?.[14]).toBe('7')
	})

	test('whoami is a service-level GET', async () => {
		const f = fake([
			{ status: 200, body: { principal: 'principal_a', org: 'orgs/org_a', api_version: '' } },
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		const me = await sx.access.whoami({})
		expect(me.org).toBe('orgs/org_a')
		expect(f.sent[0]!.url.toString()).toBe('https://api.sylphx.com/v1/whoami')
		expect(f.sent[0]!.headers['idempotency-key']).toBeUndefined()
	})

	test('a retryable answer is retried with the same idempotency key', async () => {
		const f = fake([
			{
				status: 503,
				body: { code: 'UNAVAILABLE', status: 503, retryable: true },
				headers: { 'retry-after': '0' },
			},
			{ status: 200, body: { name: `${ENV}/api_keys/key_a` } },
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		await sx.access.apiKeys.revoke({ name: `${ENV}/api_keys/key_a`, etag: '"k3"' })
		expect(f.sent.length).toBe(2)
		expect(f.sent[0]!.headers['idempotency-key']).toBe(f.sent[1]!.headers['idempotency-key']!)
		expect(f.sent[0]!.body).toEqual({ etag: '"k3"' })
	})

	test('a problem body becomes a SylphxError', async () => {
		const f = fake([
			{
				status: 409,
				body: {
					code: 'ETAG_MISMATCH',
					status: 409,
					retryable: false,
					effect: 'none',
					detail: 'changed',
					grpc_status: 'ABORTED',
				},
			},
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		const err = await sx.access.envs
			.update({ environment: { name: ENV }, updateMask: 'meta.labels' })
			.catch((e) => e)
		expect(err).toBeInstanceOf(SylphxError)
		expect((err as SylphxError).code).toBe('ETAG_MISMATCH')
		expect((err as SylphxError).requestId).toBe('req_409')
		expect(f.sent[0]!.url.searchParams.get('update_mask')).toBe('meta.labels')
	})

	// A product-plane refusal names its reason in `error.status.message`; it
	// must reach `detail`, which the CLI prints as
	// `error: ETAG_MISMATCH (409): redeploy_refused: <reason>`.
	test('a refused protocol call keeps the reason from error.status.message', async () => {
		const f = fake([
			{
				status: 409,
				body: {
					ok: false,
					error: {
						status: { code: 6, message: 'commit_not_fully_built', details: [] },
						code: 'redeploy_refused',
					},
				},
			},
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		const err = await sx.access.envs
			.update({ environment: { name: ENV }, updateMask: 'meta.labels' })
			.catch((e) => e)
		expect(err).toBeInstanceOf(SylphxError)
		expect((err as SylphxError).code).toBe('ETAG_MISMATCH')
		expect((err as SylphxError).detail).toBe('redeploy_refused: commit_not_fully_built')
	})

	test('error.message still wins over error.status.message', async () => {
		const f = fake([
			{
				status: 404,
				body: {
					error: {
						code: 'workspace_not_found',
						message: 'no such workspace',
						status: { message: 'must not win' },
					},
				},
			},
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		const err = await sx.access.envs
			.update({ environment: { name: ENV }, updateMask: 'meta.labels' })
			.catch((e) => e)
		expect(err).toBeInstanceOf(SylphxError)
		expect((err as SylphxError).detail).toBe('workspace_not_found: no such workspace')
	})

	test('a refusal that names no message stays the code alone', async () => {
		const f = fake([{ status: 409, body: { error: { code: 'redeploy_refused' } } }])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		const err = await sx.access.envs
			.update({ environment: { name: ENV }, updateMask: 'meta.labels' })
			.catch((e) => e)
		expect(err).toBeInstanceOf(SylphxError)
		expect((err as SylphxError).detail).toBe('redeploy_refused')
	})

	test('a malformed name never leaves the client', async () => {
		const f = fake([])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		await expect(sx.access.projects.get({ name: 'projects/p' })).rejects.toThrow(TypeError)
		expect(f.sent.length).toBe(0)
	})

	test('listAll follows page tokens', async () => {
		const f = fake([
			{
				status: 200,
				body: { projects: [{ name: 'orgs/org_a/projects/a' }], next_page_token: 'p2' },
			},
			{ status: 200, body: { projects: [{ name: 'orgs/org_a/projects/b' }] } },
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		const names: string[] = []
		for await (const p of sx.access.projects.listAll({ parent: 'orgs/org_a', pageSize: 1 }))
			names.push(p.name ?? '')
		expect(names).toEqual(['orgs/org_a/projects/a', 'orgs/org_a/projects/b'])
		expect(f.sent[1]!.url.searchParams.get('page_token')).toBe('p2')
	})

	test('invoke calls any method by id with wire JSON', async () => {
		const f = fake([{ status: 200, body: { name: `${ENV}/api_keys/key_a` } }])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		const out = await sx.invoke('access.api_keys.revoke', {
			name: `${ENV}/api_keys/key_a`,
			validate_only: true,
		})
		expect(out).toEqual({ name: `${ENV}/api_keys/key_a` })
		expect(f.sent[0]!.url.pathname).toBe(`/v1/${ENV}/api_keys/key_a:revoke`)
		expect(f.sent[0]!.body).toEqual({ validate_only: true })
		await expect(sx.invoke('nope.nope.get', {})).rejects.toThrow(TypeError)
	})

	test('a multi-pattern resource takes either parent', async () => {
		const f = fake([
			{ status: 200, body: {} },
			{ status: 200, body: {} },
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		await sx.access.apiKeys.create({ parent: 'orgs/org_a', apiKey: {} })
		expect(f.sent[0]!.url.pathname).toBe('/v1/orgs/org_a/api_keys')
		await sx.invoke('access.api_keys.revoke', { name: 'orgs/org_a/api_keys/key_a' })
		expect(f.sent[1]!.url.pathname).toBe('/v1/orgs/org_a/api_keys/key_a:revoke')
		await expect(
			(async () => sx.invoke('access.api_keys.revoke', { name: 'orgs/o/projects/p/api_keys/k' }))(),
		).rejects.toThrow(TypeError)
		expect(f.sent.length).toBe(2)
	})

	test('the method table is complete and keyed by id', () => {
		for (const [id, spec] of Object.entries(METHODS)) expect(spec.id).toBe(id)
		expect(METHODS['access.whoami']?.template).toBe('/v1/whoami')
	})
})
