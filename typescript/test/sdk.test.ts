// The generated SDK speaks the wire of spec §3: camelCase in, snake_case on
// the wire, problem bodies as SylphxError, retries with one idempotency key.

import { describe, expect, spyOn, test } from 'bun:test'
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
	test('Build body disconnect reconnects the same request and key', async () => {
		const f = fake([
			{ status: 200, body: {} },
			{ status: 200, body: { name: 'orgs/org_a/operations/op_a' } },
		])
		const sx = new Sylphx({
			apiKey: 'k',
			maxRetries: 1,
			fetch: async (input, init) => {
				const response = await f.fetch(input, init)
				return f.sent.length === 1
					? new Response(
							new ReadableStream({
								start(controller) {
									controller.error(new Error('disconnected'))
								},
							}),
						)
					: response
			},
		})
		const result = (await sx.invoke('build.builds.run', {
			parent: 'orgs/org_a/projects/prj_a',
			command: ['true'],
		})) as Record<string, unknown>
		expect(result.name).toBe('orgs/org_a/operations/op_a')
		expect(f.sent).toHaveLength(2)
		expect(f.sent[0]).toEqual(f.sent[1])
	})

	test('Build observes more than three pending responses without spending transient retries', async () => {
		for (const maxRetries of [0, 3]) {
			const f = fake([
				...Array.from({ length: 6 }, () => ({
					status: 409,
					headers: { 'retry-after': '0' },
					body: { code: 'IDEMPOTENCY_IN_PROGRESS', retryable: true },
				})),
				{ status: 200, body: { name: 'orgs/org_a/operations/op_a', done: false } },
			])
			const sx = new Sylphx({ apiKey: 'k', maxRetries, fetch: f.fetch })
			const result = (await sx.invoke('build.builds.run', {
				parent: 'orgs/org_a/projects/prj_a',
				command: ['true'],
			})) as Record<string, unknown>
			expect(result.name).toBe('orgs/org_a/operations/op_a')
			expect(f.sent).toHaveLength(7)
			for (const sent of f.sent) {
				expect(sent).toEqual(f.sent[0]!)
				expect(sent.headers['idempotency-key']).toBeDefined()
				expect(sent.headers['sylphx-request-timeout']).toBeUndefined()
			}
		}
	})

	test('pending does not reset or spend the transient retry budget', async () => {
		const transient = {
			status: 503,
			headers: { 'retry-after': '0' },
			body: { code: 'UNAVAILABLE', retryable: true },
		}
		const f = fake([
			transient,
			...Array.from({ length: 6 }, () => ({
				status: 409,
				headers: { 'retry-after': '0' },
				body: { code: 'IDEMPOTENCY_IN_PROGRESS', retryable: true },
			})),
			transient,
		])
		const sx = new Sylphx({ apiKey: 'k', maxRetries: 1, fetch: f.fetch })
		await expect(
			sx.invoke('build.builds.run', { parent: 'orgs/org_a/projects/prj_a' }),
		).rejects.toMatchObject({ code: 'UNAVAILABLE' })
		expect(f.sent).toHaveLength(8)
	})

	test('pending requires exact status/code/retryable and honors an explicit deadline', async () => {
		for (const [status, code, retryable, headers] of [
			[409, 'OUTCOME_UNKNOWN', false, {}],
			[409, 'IDEMPOTENCY_IN_PROGRESS', false, {}],
			[409, 'ETAG_MISMATCH', true, {}],
			[503, 'IDEMPOTENCY_IN_PROGRESS', true, {}],
			[409, 'IDEMPOTENCY_IN_PROGRESS', true, { 'retry-after': '30' }],
		] as const) {
			const f = fake([{ status, body: { code, retryable }, headers }])
			const sx = new Sylphx({ apiKey: 'k', maxRetries: 0, timeoutMs: 100, fetch: f.fetch })
			await expect(
				sx.invoke('build.builds.run', { parent: 'orgs/org_a/projects/prj_a' }),
			).rejects.toMatchObject({ code })
			expect(f.sent).toHaveLength(1)
		}
	})

	test('Retry-After waits without refreshing the original pending deadline', async () => {
		const f = fake(
			Array.from({ length: 2 }, () => ({
				status: 409,
				headers: { 'retry-after': '1' },
				body: { code: 'IDEMPOTENCY_IN_PROGRESS', retryable: true },
			})),
		)
		const sx = new Sylphx({ apiKey: 'k', maxRetries: 0, timeoutMs: 1800, fetch: f.fetch })
		const start = Date.now()
		await expect(
			sx.invoke('build.builds.run', { parent: 'orgs/org_a/projects/prj_a' }),
		).rejects.toMatchObject({ code: 'IDEMPOTENCY_IN_PROGRESS' })
		expect(Date.now() - start).toBeGreaterThanOrEqual(1000)
		expect(Date.now() - start).toBeLessThan(1800)
		expect(f.sent).toHaveLength(2)
		expect(Number(f.sent[1]!.headers['sylphx-request-timeout']) + 900).toBeLessThan(
			Number(f.sent[0]!.headers['sylphx-request-timeout']),
		)
	})

	test('the default socket timeout does not become a whole-call pending deadline', async () => {
		const clock = spyOn(Date, 'now')
		const timeout = spyOn(globalThis, 'setTimeout')
		const unsupported = spyOn(AbortSignal, 'timeout').mockImplementation(() => {
			throw new Error('not available in this WebView')
		})
		let now = 1_000_000
		clock.mockImplementation(() => now)
		try {
			const f = fake([
				{
					status: 409,
					headers: { 'retry-after': '0' },
					body: { code: 'IDEMPOTENCY_IN_PROGRESS', retryable: true },
				},
				{ status: 200, body: { name: 'orgs/org_a/operations/op_a' } },
			])
			const sx = new Sylphx({
				apiKey: 'k',
				maxRetries: 0,
				fetch: async (input, init) => {
					const response = await f.fetch(input, init)
					now += 121_000
					return response
				},
			})
			const result = (await sx.invoke('build.builds.run', {
				parent: 'orgs/org_a/projects/prj_a',
			})) as Record<string, unknown>
			expect(result.name).toBe('orgs/org_a/operations/op_a')
			expect(f.sent).toHaveLength(2)
			expect(timeout.mock.calls.filter((call) => call[1] === 120_000)).toHaveLength(2)
		} finally {
			clock.mockRestore()
			timeout.mockRestore()
			unsupported.mockRestore()
		}
	})

	test('explicit deadlines cancel stalled headers and bodies on every origin', async () => {
		for (const protocol of [false, true])
			for (const body of [false, true]) {
				let count = 0
				const sx = new Sylphx({
					apiKey: 'k',
					timeoutMs: 30,
					maxRetries: 3,
					fetch: async (_input, init) => {
						count++
						expect(init.signal).toBeDefined()
						expect(
							(init.headers as Record<string, string>)['sylphx-request-timeout'] === undefined,
						).toBe(protocol)
						if (body)
							return new Response(
								new ReadableStream({
									start(controller) {
										init.signal!.addEventListener('abort', () =>
											controller.error(init.signal!.reason),
										)
									},
								}),
							)
						return new Promise((_resolve, reject) => {
							init.signal!.addEventListener('abort', () => reject(init.signal!.reason))
						})
					},
				})
				await expect(
					protocol
						? sx.data.kv.get({ namespaceId: 'n', key: 'k' })
						: sx.invoke('build.builds.run', { parent: 'orgs/org_a/projects/prj_a' }),
				).rejects.toBeDefined()
				expect(count).toBe(1)
			}
	})
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

	test('a multi-pattern resource takes every declared parent', async () => {
		const f = fake([
			{ status: 200, body: {} },
			{ status: 200, body: {} },
			{ status: 200, body: {} },
		])
		const sx = new Sylphx({ apiKey: 'k', fetch: f.fetch })
		await sx.access.apiKeys.create({ parent: 'orgs/org_a', apiKey: {} })
		expect(f.sent[0]!.url.pathname).toBe('/v1/orgs/org_a/api_keys')
		await sx.invoke('access.api_keys.revoke', { name: 'orgs/org_a/api_keys/key_a' })
		expect(f.sent[1]!.url.pathname).toBe('/v1/orgs/org_a/api_keys/key_a:revoke')
		await sx.invoke('access.api_keys.revoke', { name: 'orgs/org_a/projects/prj_a/api_keys/key_a' })
		expect(f.sent[2]!.url.pathname).toBe('/v1/orgs/org_a/projects/prj_a/api_keys/key_a:revoke')
		await expect(
			(async () => sx.invoke('access.api_keys.revoke', { name: 'orgs/o/projects/p/api_keys' }))(),
		).rejects.toThrow(TypeError)
		expect(f.sent.length).toBe(3)
	})

	test('the method table is complete and keyed by id', () => {
		for (const [id, spec] of Object.entries(METHODS)) expect(spec.id).toBe(id)
		expect(METHODS['access.whoami']?.template).toBe('/v1/whoami')
	})
})
