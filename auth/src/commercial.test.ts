import { expect, test } from 'bun:test'
import {
	type AuthFetch,
	createSylphxAuthHandler,
	type SylphxAuthOptions,
	type VerifiedIdentitySession,
} from './server.js'

const origin = 'https://app.example.test'
const api = 'https://identity.example.test'
const authOrigin = 'https://auth.example.test'
const instance = { id: 'fixture-instance', slug: 'fixture-tenant', environment: 'production' }
const config = {
	instance,
	methods: { password: true, sign_up: true, email_otp: true, passkey: true },
	social: [{ provider: 'google', credentials: 'shared' }],
}
const secret = ['sylphx', 'sk', 'fixture'].join('_')
const ticket = 'fixture-one-time-ticket'
const credential = 'fixture-session'
const versions = { terms: 'v2', privacy: '2026-11-24' }
const signingKey = 'test-fixture-only-signing-key-not-for-deployment'
const otpFlow = 'a'.repeat(43)
const expires = Math.floor(Date.now() / 1000) + 600
const result = (expiry: unknown = expires, isNewUser = true) => ({
	session: { id: 'fixture-session-id', token: credential, expires_at: expiry },
	user: { id: 'fixture-user', email: 'person@example.test', email_verified: true },
	is_new_user: isNewUser,
})

function setup(fetch: AuthFetch, options: Partial<SylphxAuthOptions> = {}) {
	return createSylphxAuthHandler({
		origin,
		authApiOrigin: api,
		instance,
		fetch,
		...options,
	} as SylphxAuthOptions)
}
function post(path: string, input: object) {
	return new Request(origin + path, {
		method: 'POST',
		headers: { origin, 'content-type': 'application/json' },
		body: JSON.stringify(input),
	})
}
function bearer(init: RequestInit) {
	return new Headers(init.headers).get('authorization')
}

for (const [action, path, fields] of [
	[
		'password',
		'/v1/client/sign-in/password',
		{ email: 'person@example.test', password: 'fixture' },
	],
	['otp_verify', '/v1/client/continue/verify', { flow: otpFlow, code: '123456' }],
	[
		'passkey_verify',
		'/v1/client/sign-in/passkey/verify',
		{
			challenge_id: 'fixture',
			credential_id: 'fixture',
			client_data_json: 'fixture',
			authenticator_data: 'fixture',
			signature: 'fixture',
		},
	],
	['mfa', '/v1/client/mfa/totp/verify', { challenge: 'fixture', code: '123456', method: 'totp' }],
] as const) {
	test(`server-mode ${action} redeems server-side and emits only an app session`, async () => {
		let calls = 0
		let issued: VerifiedIdentitySession | undefined
		const auth = setup(
			async (url, init) => {
				calls++
				expect(url.origin).toBe(api)
				expect(url.searchParams.get('instance')).toBe(instance.slug)
				const body = JSON.parse(String(init.body))
				if (calls === 1) {
					expect(url.pathname).toBe(path)
					expect(bearer(init)).toBeNull()
					expect(body).not.toHaveProperty('acceptance')
					if (action === 'password' || action === 'passkey_verify')
						expect(body.session_mode).toBe('server')
					else expect(body).not.toHaveProperty('session_mode')
					return Response.json({ ticket })
				}
				expect(url.pathname).toBe('/v1/client/tickets:redeem')
				expect(bearer(init)).toBe(`Bearer ${secret}`)
				expect(body).toEqual({ ticket })
				return Response.json(result(expires, action === 'otp_verify'))
			},
			{
				sessionMode: 'server',
				sessionSecret: secret,
				clickwrap: {
					signingKey,
					validate: (v) => v.terms === versions.terms && v.privacy === versions.privacy,
				},
				session: {
					async issue(session, request) {
						expect(request.url).toBe(`${origin}/auth/email`)
						issued = session
						return [['__Host-app_session', 'fixture-app-session', 60]]
					},
					async logout() {
						return new Response(null, { status: 204 })
					},
				},
			},
		)
		const res = await auth(post('/auth/email', { ...fields, action, acceptance: versions }))
		expect(res.status).toBe(200)
		expect(await res.json()).toEqual({ next: '/' })
		expect(res.headers.getSetCookie()).toEqual([
			'__Host-app_session=fixture-app-session; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=60',
		])
		expect(issued).toEqual({
			token: credential,
			expiresAt: expires,
			sessionId: 'fixture-session-id',
			user: { id: 'fixture-user', email: 'person@example.test', emailVerified: true },
			isNewUser: action === 'otp_verify',
			acceptance: versions,
		})
		expect(calls).toBe(2)
	})
}

test('server mode has no direct-session or failed-ticket fallback', async () => {
	for (const upstream of [result(), { ticket }]) {
		let requests = 0
		const auth = setup(
			async (url) => {
				requests++
				return url.pathname === '/v1/client/tickets:redeem'
					? Response.json(result(), { status: 401 })
					: Response.json(upstream)
			},
			{ sessionMode: 'server', sessionSecret: secret },
		)
		const res = await auth(
			post('/auth/email', {
				action: 'password',
				email: 'person@example.test',
				password: 'fixture',
			}),
		)
		expect(res.status).toBe(502)
		expect(res.headers.getSetCookie()).toEqual([])
		expect(await res.json()).toEqual({ error: 'session_unavailable' })
		expect(requests).toBe('ticket' in upstream ? 2 : 1)
	}
})

test('OTP start seals upstream session mode and rejects stale consent before a code is sent', async () => {
	let starts = 0
	const auth = setup(
		async (url, init) => {
			starts++
			expect(url.pathname).toBe('/v1/client/continue')
			expect(bearer(init)).toBeNull()
			expect(JSON.parse(String(init.body))).toMatchObject({
				session_mode: 'server',
				method: 'email_otp',
			})
			return Response.json({ step: 'email_otp', flow: otpFlow })
		},
		{
			sessionMode: 'server',
			sessionSecret: secret,
			clickwrap: { signingKey, validate: (v) => v.terms === 'v2' },
		},
	)
	for (const acceptance of [undefined, { terms: 'v1' }, { terms: true }]) {
		const res = await auth(
			post('/auth/email', { action: 'otp_start', email: 'person@example.test', acceptance }),
		)
		expect(res.status).toBe(403)
		expect(await res.json()).toEqual({ error: 'acceptance_required' })
	}
	expect(starts).toBe(0)
	const res = await auth(
		post('/auth/email', {
			action: 'otp_start',
			email: 'person@example.test',
			acceptance: versions,
		}),
	)
	expect(await res.json()).toEqual({ flow: otpFlow })
	expect(starts).toBe(1)
})

test('OAuth binds consent to the flow and rechecks current versions before confidential redemption', async () => {
	let current = 'v2'
	let starts = 0
	let redemptions = 0
	let callback: URL | undefined
	const authorize =
		'https://accounts.google.com/o/oauth2/v2/auth?redirect_uri=https%3A%2F%2Fauth.example.test%2Faccount%2Foauth%2Fcallback'
	const auth = setup(
		async (url, init) => {
			if (url.pathname === '/v1/client/config') {
				return Response.json({
					...config,
					portal_origin: authOrigin,
					portal_url: `${authOrigin}/account`,
					social: [
						{
							provider: 'google',
							credentials: 'custom',
							start_url: `${authOrigin}/account/oauth/google/start?instance=${instance.slug}`,
						},
					],
				})
			}
			if (url.pathname === '/account/oauth/google/start') {
				starts++
				expect(url.origin).toBe(authOrigin)
				expect(url.searchParams.get('session_mode')).toBe('server')
				expect(url.searchParams.size).toBe(3)
				expect(bearer(init)).toBeNull()
				callback = new URL(url.searchParams.get('redirect_url')!)
				expect(callback.pathname).toBe('/account/auth/callback')
				callback.searchParams.set('auth_ticket', ticket)
				return Response.redirect(authorize, 303)
			}
			redemptions++
			expect(url.pathname).toBe('/v1/client/tickets:redeem')
			expect(url.origin).toBe(api)
			expect(bearer(init)).toBe(`Bearer ${secret}`)
			return redemptions === 1 ? Response.json(result()) : Response.json({}, { status: 401 })
		},
		{
			sessionMode: 'server',
			sessionSecret: secret,
			tenantAuthOrigin: authOrigin,
			routePrefix: '/account/auth',
			clickwrap: { signingKey, validate: (v) => v.terms === current },
		},
	)
	const start = (acceptance?: object) =>
		auth(post('/account/auth/start/google', { next: '/welcome', acceptance }))
	expect((await start()).status).toBe(403)
	expect(starts).toBe(0)
	const started = await start(versions)
	expect(await started.json()).toEqual({ authorize })
	const cookie = started.headers.getSetCookie()[0]!.split(';')[0]!
	const callbackWith = (value: string) =>
		auth(new Request(callback!, { headers: { cookie: value } }))
	const parts = cookie.split('.')
	parts[3] = btoa(JSON.stringify({ terms: 'v3' })).replaceAll('=', '')
	current = 'v3'
	expect((await callbackWith(parts.join('.'))).headers.get('location')).toContain('error=signin')
	expect((await callbackWith(cookie)).headers.get('location')).toContain(
		'error=acceptance_required',
	)
	expect(redemptions).toBe(0)
	current = 'v2'
	const success = await callbackWith(cookie)
	expect(success.headers.get('location')).toBe('/welcome')
	expect(success.headers.getSetCookie()).toHaveLength(2)
	expect(await success.text()).toBe('')
	expect((await callbackWith(cookie)).headers.getSetCookie()).toHaveLength(1)
	expect(redemptions).toBe(2)
})

test('disable only new password registration and preserve direct browser-mode sign-in', async () => {
	let passwordCalls = 0
	const auth = setup(
		async (url, init) => {
			if (url.pathname === '/v1/client/config') return Response.json(config)
			passwordCalls++
			expect(url.pathname).toBe('/v1/client/sign-in/password')
			expect(bearer(init)).toBeNull()
			expect(JSON.parse(String(init.body)).session_mode).toBe('browser')
			return Response.json(result())
		},
		{ routePrefix: '/account/auth', allowPasswordRegistration: false },
	)
	expect((await auth(new Request(`${origin}/auth/config`))).status).toBe(404)
	expect(await (await auth(new Request(`${origin}/account/auth/config`))).json()).toMatchObject({
		password: true,
		signup: false,
	})
	const input = { email: 'person@example.test', password: 'fixture-password' }
	expect((await auth(post('/account/auth/email', { ...input, action: 'signup' }))).status).toBe(403)
	expect(passwordCalls).toBe(0)
	expect((await auth(post('/account/auth/email', { ...input, action: 'password' }))).status).toBe(
		200,
	)
	expect(passwordCalls).toBe(1)
})

test('default cookie accepts Unix seconds, numeric seconds and ISO, and refuses invalid expiry', async () => {
	for (const expiry of [expires, String(expires), new Date(expires * 1000).toISOString()]) {
		const auth = setup(async () => Response.json(result(expiry)))
		const res = await auth(
			post('/auth/email', {
				action: 'password',
				email: 'person@example.test',
				password: 'fixture',
			}),
		)
		expect(res.status).toBe(200)
		const age = Number(res.headers.getSetCookie()[0]!.split('Max-Age=')[1])
		expect(age).toBeGreaterThan(0)
		expect(age).toBeGreaterThanOrEqual(Math.floor(expires - Date.now() / 1000))
		expect(age).toBeLessThanOrEqual(600)
	}
	for (const expiry of [
		42,
		'42',
		'0x40',
		'9e9',
		'not-a-date',
		'9999999999999999',
		expires * 1000,
	]) {
		const auth = setup(async () => Response.json(result(expiry)))
		const res = await auth(
			post('/auth/email', {
				action: 'password',
				email: 'person@example.test',
				password: 'fixture',
			}),
		)
		expect(res.status).toBe(502)
		expect(res.headers.getSetCookie()).toEqual([])
	}
})

test('a policy change during email verification prevents app session issuance', async () => {
	let current = versions.terms
	const auth = setup(
		async () => {
			current = 'v3'
			return Response.json(result())
		},
		{ clickwrap: { signingKey, validate: (v) => v.terms === current } },
	)
	const res = await auth(
		post('/auth/email', {
			action: 'password',
			email: 'person@example.test',
			password: 'fixture',
			acceptance: versions,
		}),
	)
	expect(res.status).toBe(403)
	expect(res.headers.getSetCookie()).toEqual([])
	expect(await res.json()).toEqual({ error: 'acceptance_required' })
})

test('server mode requires an instance secret and mounts only a canonical route prefix', () => {
	const fetch: AuthFetch = async () => Response.json({})
	expect(() => setup(fetch, { sessionMode: 'server' })).toThrow('instance secret key')
	expect(() => setup(fetch, { sessionMode: 'server', sessionSecret: 'public-key' })).toThrow(
		'instance secret key',
	)
	for (const routePrefix of [
		'/',
		'https://other.example',
		'//other.example',
		'/auth/../outside',
		'/auth/',
	]) {
		expect(() => setup(fetch, { routePrefix })).toThrow('route prefix')
	}
})
