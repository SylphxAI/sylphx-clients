// @sylphx/sdk/auth/client: the browser client for /v1/client/*.

import { afterEach, describe, expect, test } from 'bun:test'
import {
	AuthClientError,
	AuthEnvironmentMismatchError,
	AuthLockedOutError,
	AuthMfaRequiredError,
	AuthNetworkError,
	AuthRateLimitedError,
	AuthUnauthorizedError,
	AuthUnavailableError,
	createAuthClient,
	withEnvironment,
} from '../src/auth/client.js'

const KEY = 'sylphx_pk_test_abc'
const g = globalThis as { location?: unknown }

interface Call {
	url: string
	init: RequestInit
}
function stub(respond: (call: Call) => Response) {
	const calls: Call[] = []
	const fetch = async (url: string, init: RequestInit = {}) => {
		const call = { url, init }
		calls.push(call)
		return respond(call)
	}
	return { calls, fetch }
}
const ok = (body: unknown, status = 200) =>
	new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
const err = (status: number, code: string, headers: Record<string, string> = {}) =>
	new Response(JSON.stringify({ code, error: `msg ${code}`, authority: 'identity' }), {
		status,
		headers,
	})
const userWire = { id: 'u1', email: 'a@b.c', name: 'A', email_verified: true }
const sessionWire = { id: 's1', token: 'identity_org_session_x', expires_at: 1800000000 }

afterEach(() => {
	delete g.location
})

describe('key parameter', () => {
	test('cross-origin browser call carries publishable_key in the query', async () => {
		g.location = { origin: 'https://app.example.com' }
		const s = stub(() =>
			ok({ instance: {}, methods: {}, social: [], branding: {}, localization: {} }),
		)
		await createAuthClient({ publishableKey: KEY, fetch: s.fetch })
			.config()
			.catch(() => {})
		expect(s.calls[0]?.url).toBe(`https://api.sylphx.com/v1/client/config?publishable_key=${KEY}`)
	})

	test('same-origin call carries it too, and never as a Bearer', async () => {
		g.location = { origin: 'https://api.sylphx.com' }
		const s = stub(() =>
			ok({ session: { id: 's', expires_at: 1, instance_id: 'i' }, user: userWire }),
		)
		const c = createAuthClient({
			publishableKey: KEY,
			sessionToken: 'identity_org_session_x',
			fetch: s.fetch,
		})
		await c.getSession()
		expect(s.calls[0]?.url).toContain(`publishable_key=${KEY}`)
		expect((s.calls[0]?.init.headers as Record<string, string>).authorization).toBe(
			'Bearer identity_org_session_x',
		)
	})

	test('server context (no location) also carries the key, no Authorization', async () => {
		delete g.location
		const s = stub(() =>
			ok({ instance: {}, methods: {}, social: [], branding: {}, localization: {} }),
		)
		await createAuthClient({ publishableKey: KEY, fetch: s.fetch })
			.config()
			.catch(() => {})
		expect(s.calls[0]?.url).toContain(`publishable_key=${KEY}`)
		expect((s.calls[0]?.init.headers as Record<string, string>).authorization).toBeUndefined()
	})

	test('instance slug goes in ?instance=', async () => {
		const s = stub(() =>
			ok({ instance: {}, methods: {}, social: [], branding: {}, localization: {} }),
		)
		await createAuthClient({ instance: 'acme', fetch: s.fetch })
			.config()
			.catch(() => {})
		expect(s.calls[0]?.url).toBe('https://api.sylphx.com/v1/client/config?instance=acme')
	})

	test('a secret key is refused, and so is a non-pk value or both/neither reference', () => {
		expect(() => createAuthClient({ publishableKey: 'sylphx_sk_live_x' })).toThrow(/secret key/)
		expect(() => createAuthClient({ publishableKey: 'identity_org_key_x' })).toThrow(/secret key/)
		expect(() => createAuthClient({ publishableKey: 'nope' })).toThrow(/sylphx_pk_/)
		expect(() => createAuthClient({})).toThrow(/exactly one/)
		expect(() => createAuthClient({ publishableKey: KEY, instance: 'a' })).toThrow(/exactly one/)
	})

	test('existing query is kept and the parameter is never duplicated', () => {
		expect(withEnvironment('https://x/y?a=1&publishable_key=old', 'publishable_key', KEY)).toBe(
			`https://x/y?a=1&publishable_key=${KEY}`,
		)
		expect(withEnvironment('https://x/y?a=1#f', 'publishable_key', KEY)).toBe(
			`https://x/y?a=1&publishable_key=${KEY}#f`,
		)
	})

	test('credentials default to omit; include is opt-in', async () => {
		const s = stub(() =>
			ok({ instance: {}, methods: {}, social: [], branding: {}, localization: {} }),
		)
		await createAuthClient({ publishableKey: KEY, fetch: s.fetch })
			.config()
			.catch(() => {})
		await createAuthClient({ publishableKey: KEY, fetch: s.fetch, credentials: 'include' })
			.config()
			.catch(() => {})
		expect(s.calls[0]?.init.credentials).toBe('omit')
		expect(s.calls[1]?.init.credentials).toBe('include')
	})

	test('baseUrl override', async () => {
		const s = stub(() =>
			ok({ instance: {}, methods: {}, social: [], branding: {}, localization: {} }),
		)
		await createAuthClient({ publishableKey: KEY, baseUrl: 'https://auth.test/', fetch: s.fetch })
			.config()
			.catch(() => {})
		expect(s.calls[0]?.url.startsWith('https://auth.test/v1/client/config?')).toBe(true)
	})
})

describe('routes', () => {
	test('GET config', async () => {
		const s = stub(() =>
			ok({
				instance: { id: 'i', name: 'N', environment: 'production' },
				methods: { password: true },
				social: [
					{
						provider: 'google',
						name: 'Google',
						credentials: 'shared',
						start_url: 'https://x/start',
					},
				],
				branding: { name: 'N', logo_url: null, primary_color: null },
				localization: { default_locale: 'en', locales: ['en'] },
				api_origin: 'https://api.sylphx.com',
				ticket_param: 'auth_ticket',
				error_param: 'auth_error',
			}),
		)
		const c = await createAuthClient({ publishableKey: KEY, fetch: s.fetch }).config()
		expect(s.calls[0]?.init.method).toBe('GET')
		expect(c.social[0]?.startUrl).toBe('https://x/start')
		expect(c.localization.defaultLocale).toBe('en')
		expect(c.ticketParam).toBe('auth_ticket')
	})

	test('POST sign-in/password sends session_mode browser and remembers the token', async () => {
		const s = stub(() => ok({ session: sessionWire, user: userWire }))
		const auth = createAuthClient({ publishableKey: KEY, fetch: s.fetch })
		const r = await auth.signInWithPassword({
			email: 'a@b.c',
			password: 'pw',
			factorProofs: [{ factorType: 'totp', factorId: 'f', response: '123456' }],
		})
		expect(s.calls[0]?.url).toContain('/v1/client/sign-in/password?')
		expect(JSON.parse(s.calls[0]?.init.body as string)).toEqual({
			email: 'a@b.c',
			password: 'pw',
			factor_proofs: [{ factor_type: 'totp', factor_id: 'f', response: '123456' }],
			captcha_token: '',
			session_mode: 'browser',
		})
		expect(r.session.expiresAt).toBe(1800000000)
		expect(r.user.emailVerified).toBe(true)
		expect(auth.sessionToken).toBe('identity_org_session_x')
	})

	test('POST sign-up', async () => {
		const s = stub(() => ok({ accepted: true }, 202))
		const r = await createAuthClient({ publishableKey: KEY, fetch: s.fetch }).signUp({
			email: 'a@b.c',
			password: 'a-long-password',
			redirectUrl: 'https://app/x',
		})
		expect(r).toEqual({ accepted: true })
		expect(JSON.parse(s.calls[0]?.init.body as string).redirect_url).toBe('https://app/x')
	})

	test('oauth start is a URL with redirect_url, session_mode browser and the key', () => {
		const u = new URL(
			createAuthClient({ publishableKey: KEY }).oauthStartUrl('google', {
				redirectUrl: 'https://app/cb?x=1',
			}),
		)
		expect(u.pathname).toBe('/v1/client/oauth/google/start')
		expect(u.searchParams.get('redirect_url')).toBe('https://app/cb?x=1')
		expect(u.searchParams.get('session_mode')).toBe('browser')
		expect(u.searchParams.getAll('publishable_key')).toEqual([KEY])
	})

	test('POST tickets:redeem', async () => {
		const s = stub(() => ok({ session: sessionWire, user: userWire }))
		const auth = createAuthClient({ publishableKey: KEY, fetch: s.fetch })
		await auth.redeemTicket('tk')
		expect(s.calls[0]?.url).toContain('/v1/client/tickets:redeem?')
		expect(JSON.parse(s.calls[0]?.init.body as string)).toEqual({ ticket: 'tk' })
		expect(auth.sessionToken).toBe('identity_org_session_x')
	})

	test('GET session needs and sends the bearer', async () => {
		const s = stub(() =>
			ok({ session: { id: 's', expires_at: 5, instance_id: 'i' }, user: userWire }),
		)
		const auth = createAuthClient({ publishableKey: KEY, fetch: s.fetch })
		await expect(auth.getSession()).rejects.toBeInstanceOf(AuthUnauthorizedError)
		expect(s.calls.length).toBe(0)
		auth.setSessionToken('identity_org_session_y')
		const r = await auth.getSession()
		expect(r.session.instanceId).toBe('i')
		expect((s.calls[0]?.init.headers as Record<string, string>).authorization).toBe(
			'Bearer identity_org_session_y',
		)
	})

	test('POST sign-out is 204 and forgets the token', async () => {
		const s = stub(() => new Response(null, { status: 204 }))
		const auth = createAuthClient({
			publishableKey: KEY,
			fetch: s.fetch,
			sessionToken: 'identity_org_session_y',
		})
		await auth.signOut()
		expect(s.calls[0]?.init.method).toBe('POST')
		expect(auth.sessionToken).toBeUndefined()
		await auth.signOut()
		expect(s.calls.length).toBe(1)
	})

	const pr = {
		request_id: 'r1',
		state: 'pending',
		organization_id: 'o',
		principal_id: 'p',
		request_type: 'export',
		requested_at_unix_seconds: 1,
		completed_at_unix_seconds: 0,
		store_rows: [{ store: 'sessions', rows: 2 }],
		exempt_stores: [],
		export_expires_at_unix_seconds: 0,
		attempts: 0,
		last_error: '',
	}

	test('privacy requests: create, get, export', async () => {
		const s = stub((call) =>
			call.url.includes('/export?')
				? ok({ data: 1 })
				: call.init.method === 'POST'
					? ok({ privacy_request: pr }, 202)
					: ok({ privacy_request: pr, export_json: '{}' }),
		)
		const auth = createAuthClient({
			publishableKey: KEY,
			fetch: s.fetch,
			sessionToken: 'identity_org_session_y',
		})
		const created = await auth.privacyRequests.create({
			idempotencyKey: 'k',
			principalId: 'p',
			requestType: 'export',
			organizationId: 'o',
		})
		expect(JSON.parse(s.calls[0]?.init.body as string)).toEqual({
			idempotency_key: 'k',
			principal_id: 'p',
			request_type: 'export',
			organization_id: 'o',
		})
		expect(created.storeRows[0]?.rows).toBe(2)
		const got = await auth.privacyRequests.get('r1', { organizationId: 'o', includeExport: true })
		expect(s.calls[1]?.url).toContain('/v1/client/privacy-requests/r1?')
		expect(s.calls[1]?.url).toContain('organization_id=o')
		expect(s.calls[1]?.url).toContain('include_export=true')
		expect(got.exportJson).toBe('{}')
		expect(await auth.privacyRequests.export('r1', { organizationId: 'o' })).toEqual({ data: 1 })
		expect(s.calls[2]?.url).toContain('/v1/client/privacy-requests/r1/export?')
		// The server rejects unknown query fields on these two routes (400).
		for (const i of [1, 2]) {
			const q = new URL(s.calls[i]?.url ?? '').searchParams
			expect([...q.keys()].every((k) => k === 'organization_id' || k === 'include_export')).toBe(
				true,
			)
		}
	})

	test('privacy GET routes carry no environment field with an instance slug either', async () => {
		const s = stub(() => ok({ data: 1 }))
		const auth = createAuthClient({
			instance: 'acme',
			fetch: s.fetch,
			sessionToken: 'identity_org_session_y',
		})
		await auth.privacyRequests.export('r1', { organizationId: 'o' })
		expect(new URL(s.calls[0]?.url ?? '').search).toBe('?organization_id=o')
	})
})

describe('errors', () => {
	async function reject(res: Response) {
		const s = stub(() => res)
		return createAuthClient({ publishableKey: KEY, fetch: s.fetch })
			.signInWithPassword({ email: 'a', password: 'b' })
			.then(
				() => undefined,
				(e: unknown) => e as AuthClientError,
			)
	}

	test('maps code to a typed class with status and message', async () => {
		const e = await reject(err(401, 'mfa_required'))
		expect(e).toBeInstanceOf(AuthMfaRequiredError)
		expect(e?.status).toBe(401)
		expect(e?.code).toBe('mfa_required')
		expect(e?.message).toBe('msg mfa_required')
		expect(e?.authority).toBe('identity')
		expect(await reject(err(401, 'unauthorized'))).toBeInstanceOf(AuthUnauthorizedError)
	})

	test('parses Retry-After on 429', async () => {
		const e = await reject(err(429, 'rate_limited', { 'retry-after': '7' }))
		expect(e).toBeInstanceOf(AuthRateLimitedError)
		expect(e?.retryAfterSeconds).toBe(7)
		expect(e?.retryable).toBe(true)
		expect(await reject(err(429, 'locked_out', { 'retry-after': '60' }))).toBeInstanceOf(
			AuthLockedOutError,
		)
	})

	test('503 is unavailable and retryable', async () => {
		const e = await reject(err(503, 'store_unavailable'))
		expect(e).toBeInstanceOf(AuthUnavailableError)
		expect(e?.retryable).toBe(true)
	})

	test('environment mismatch has its own class', async () => {
		expect(await reject(err(403, 'environment_mismatch'))).toBeInstanceOf(
			AuthEnvironmentMismatchError,
		)
	})

	test('unknown code and non-JSON body fall back to the base error', async () => {
		const e = await reject(new Response('bad gateway', { status: 502 }))
		expect(e).toBeInstanceOf(AuthClientError)
		expect(e?.constructor).toBe(AuthClientError)
		expect(e?.code).toBe('http_502')
		expect(e?.status).toBe(502)
	})

	test('a rejected fetch is a network error', async () => {
		const auth = createAuthClient({
			publishableKey: KEY,
			fetch: async () => {
				throw new TypeError('Failed to fetch')
			},
		})
		await expect(auth.config()).rejects.toBeInstanceOf(AuthNetworkError)
	})
})
