import { expect, mock, test } from 'bun:test'
import {
	type AuthFetch,
	createSylphxAuthHandler,
	readAuthSessionCookie,
	type SylphxAuthOptions,
} from './server.js'

const origin = 'https://app.example.test'
const revision = 'http://revision.namespace.svc.cluster.local'
const edgeHeaders = {
	'sylphx-edge': 'public',
	'x-forwarded-host': 'app.example.test',
	'x-forwarded-proto': 'https',
}
const api = 'https://identity.example.test'
const instance = { id: 'test-instance', slug: 'test-tenant', environment: 'test' }
const config = {
	instance,
	methods: { password: true, sign_up: true, email_otp: true, passkey: true },
	social: [{ provider: 'google', credentials: 'shared' }],
}
const publishableKey = ['sylphx', 'pk', 'test-fixture'].join('_')
const next = '/account/profile'
const expires_at = Math.floor(Date.now() / 1000) + 600
const result = (email_verified = true) => ({
	session: { token: 'fixture-session', expires_at },
	user: { id: 'fixture-subject', email_verified },
	is_new_user: true,
})
const input = { email: 'member@example.test', password: 'test-only-fixture' }
const verify = {
	challenge_id: 'challenge',
	credential_id: 'AQID',
	client_data_json: 'BAUG',
	authenticator_data: 'BwgJ',
	signature: 'CgsM',
	user_handle: 'DQ4P',
}

function create(fetch: AuthFetch, options: Partial<SylphxAuthOptions> = {}) {
	return createSylphxAuthHandler({
		origin,
		authApiOrigin: api,
		instance,
		fetch,
		...options,
	} as SylphxAuthOptions)
}
function post(body: object, route = 'email', extra?: Record<string, string>) {
	return new Request(`${origin}/auth/${route}`, {
		method: 'POST',
		headers: { origin, 'content-type': 'application/json', ...extra },
		body: JSON.stringify(body),
	})
}
const configRequest = () => new Request(`${origin}/auth/config`)

test('publishable-key-only discovery and passkey assertion use Identity bearer auth and strict browser payloads', async () => {
	const fetch = mock<AuthFetch>(async (url, init) => {
		expect(url.origin).toBe(api)
		expect(url.search).toBe('')
		expect(new Headers(init.headers).get('authorization')).toBe(`Bearer ${publishableKey}`)
		if (url.pathname === '/v1/client/config') return Response.json(config)
		expect(url.pathname).toBe('/v1/client/sign-in/passkey/verify')
		expect(JSON.parse(String(init.body))).toEqual({ ...verify, session_mode: 'browser' })
		return Response.json(result())
	})
	const auth = create(fetch, { publishableKey, instance: undefined })
	const discovery = await auth(configRequest())
	expect(discovery.status).toBe(200)
	expect(await discovery.json()).toEqual({
		password: true,
		signup: true,
		otp: true,
		passkey: true,
		providers: ['google'],
	})
	expect(discovery.headers.get('cache-control')).toBe('private, no-store')
	expect(
		(
			await auth(
				post({ action: 'passkey_verify', ...verify, signature: '', session_mode: 'server' }),
			)
		).status,
	).toBe(400)
	expect(
		(await auth(post({ action: 'passkey_verify', ...verify, session_mode: 'server' }))).status,
	).toBe(200)
	expect(fetch).toHaveBeenCalledTimes(2)
})

test('a rejected key or a different tuple is not retried as an anonymous slug', async () => {
	for (const [body, status] of [
		[{}, 401],
		[{ ...config, instance: { ...instance, environment: 'other' } }, 200],
	] as const) {
		const fetch = mock<AuthFetch>(async () => Response.json(body, { status }))
		const auth = create(fetch, { publishableKey })
		expect((await auth(configRequest())).status).toBe(503)
		expect(fetch).toHaveBeenCalledTimes(1)
	}
	expect(() =>
		create(async () => Response.json({}), { publishableKey: 'not-a-publishable-key' }),
	).toThrow()
})

test('the same allowlist filters discovery, all dispatch paths, and proofs before dispatch', async () => {
	const fetch = mock<AuthFetch>(async (url) => {
		if (url.pathname === '/v1/client/config') return Response.json(config)
		expect(url.pathname).toBe('/v1/client/sign-in/passkey/verify')
		return Response.json(result())
	})
	const auth = create(fetch, { allowedMethods: ['passkey'], legacyStart: true })
	expect(await (await auth(configRequest())).json()).toEqual({
		providers: [],
		password: false,
		signup: false,
		otp: false,
		passkey: true,
	})
	for (const action of [
		'password',
		'signup',
		'verify_signup',
		'mfa',
		'otp_start',
		'otp_verify',
		'recovery_start',
	])
		expect((await auth(post({ ...input, action }))).status).toBe(403)
	expect((await auth(post({}, 'start/google'))).status).toBe(403)
	expect((await auth(post({ action: 'social', provider: 'google' }, 'start'))).status).toBe(403)
	expect(fetch).toHaveBeenCalledTimes(1)
	expect((await auth(post({ action: 'passkey_verify', ...verify }))).status).toBe(200)
	const disabled = create(async () => Response.json({ ...config, methods: {} }), {
		allowedMethods: ['passkey'],
	})
	expect((await disabled(post({ action: 'passkey_verify', ...verify }))).status).toBe(403)
})

test('unverified password sessions require the separate app-owned handoff and are never upgraded', async () => {
	const dispatch: AuthFetch = async () => Response.json(result(false))
	const password = () => post({ ...input, action: 'password', next })
	const denied = await create(dispatch)(password())
	expect(denied.status).toBe(403)
	expect(await denied.json()).toEqual({ error: 'verify_email' })
	expect(denied.headers.getSetCookie()).toEqual([])
	const issue = mock(async () => undefined)
	const unverified = mock<NonNullable<SylphxAuthOptions['issueUnverifiedPassword']>>(
		async (session) => {
			expect(session).toMatchObject({
				method: 'password',
				returnTo: next,
				user: { emailVerified: false },
			})
			return [['__Host-proof_pending', 'proof-handle-fixture', 60]]
		},
	)
	const auth = create(dispatch, {
		issueUnverifiedPassword: unverified,
		session: { issue, logout: async () => new Response(null, { status: 204 }) },
	})
	const response = await auth(password())
	expect(response.status).toBe(200)
	expect(await response.json()).toEqual({ next })
	expect(response.headers.getSetCookie()[0]).toMatch(
		/^__Host-proof_pending=proof-handle-fixture; Path=\/; Secure; HttpOnly/,
	)
	expect((await auth(post({ action: 'passkey_verify', ...verify }))).status).toBe(403)
	expect(unverified).toHaveBeenCalledTimes(1)
	expect(issue).not.toHaveBeenCalled()
	expect(
		(await create(dispatch, { issueUnverifiedPassword: async () => undefined })(password())).status,
	).toBe(403)
})

test('bounded legacy cookie reads prefer canonical, refuse ambiguity, and sign out through Identity', async () => {
	const request = (cookie: string) => post({}, 'logout', { cookie })
	const options = { legacyCookies: ['old_session'] }
	expect(readAuthSessionCookie(request('old_session=legacy-session'), options)).toEqual({
		name: 'old_session',
		value: 'legacy-session',
		legacy: true,
	})
	expect(
		readAuthSessionCookie(request('__Host-auth_session=canonical; old_session=legacy'), options)
			?.value,
	).toBe('canonical')
	expect(
		readAuthSessionCookie(
			request('__Host-auth_session=one; __Host-auth_session=two; old_session=legacy'),
			options,
		),
	).toBeUndefined()
	expect(
		readAuthSessionCookie(request('__Host-auth_session=; old_session=legacy'), options),
	).toBeUndefined()
	const fetch = mock<AuthFetch>(async (url, init) => {
		expect(url.pathname).toBe('/v1/client/sign-out')
		expect(new Headers(init.headers).get('authorization')).toBe('Bearer legacy-session')
		return new Response(null, { status: 204 })
	})
	const res = await create(fetch, options)(request('old_session=legacy-session'))
	expect(res.status).toBe(204)
	expect(res.headers.getSetCookie().find((x) => x.startsWith('old_session='))).toEndWith(
		'Max-Age=0',
	)
	expect(fetch).toHaveBeenCalledTimes(1)
})

test('passkey enrollment requires a live, verified, correctly scoped session and filters both upstream bodies', async () => {
	let scope = instance.id
	const fetch = mock<AuthFetch>(async (url, init) => {
		if (url.pathname === '/v1/client/config') return Response.json(config)
		expect(new Headers(init.headers).get('authorization')).toBe('Bearer fixture-session')
		if (url.pathname === '/v1/client/session')
			return Response.json({ session: { instance_id: scope }, user: { email_verified: true } })
		if (url.pathname.endsWith('/begin')) {
			expect(JSON.parse(String(init.body))).toEqual({ rp_id: 'app.example.test', origin })
			return Response.json({
				challenge_id: 'challenge',
				challenge: 'AQID',
				rp_id: 'app.example.test',
				user_id: 'AQID',
				user_name: 'member@example.test',
				public_key_algorithms: [-7],
			})
		}
		expect(url.pathname).toBe('/v1/client/me/passkeys/verify')
		expect(JSON.parse(String(init.body))).toEqual({
			challenge_id: 'challenge',
			credential_id: 'AQID',
			client_data_json: 'BAUG',
			attestation_object: 'CAkK',
			transports: ['internal'],
		})
		return Response.json({ passkey: { credential_id: 'AQID', rp_id: 'app.example.test', origin } })
	})
	const auth = create(fetch, { passkeyEnrollment: true, legacyCookies: ['old_session'] })
	const route = 'passkey/enroll'
	const cookie = { cookie: 'old_session=fixture-session' }
	expect((await auth(post({ action: 'begin', token: 'fixture-session' }, route))).status).toBe(403)
	expect((await auth(post({ action: 'begin' }, route, { ...cookie, origin: api }))).status).toBe(
		403,
	)
	expect(fetch).not.toHaveBeenCalled()
	scope = 'different-instance'
	expect((await auth(post({ action: 'begin' }, route, cookie))).status).toBe(403)
	expect(fetch).toHaveBeenCalledTimes(2)
	scope = instance.id
	expect(
		await (await auth(post({ action: 'begin', rp_id: 'ignored.example' }, route, cookie))).json(),
	).toEqual({
		challengeId: 'challenge',
		challenge: 'AQID',
		rpId: 'app.example.test',
		userId: 'AQID',
		userName: 'member@example.test',
		algorithms: [-7],
	})
	expect(
		(
			await auth(
				post({ action: 'verify', ...verify, attestation_object: 'bad input' }, route, cookie),
			)
		).status,
	).toBe(400)
	expect(
		await (
			await auth(
				post(
					{ action: 'verify', ...verify, attestation_object: 'CAkK', transports: ['internal'] },
					route,
					cookie,
				),
			)
		).json(),
	).toEqual({ enrolled: true })
})

test('recovery is opt-in, shares the password policy, and returns a generic accepted step', async () => {
	const recoverPassword = mock(async () => {})
	const auth = create(async () => Response.json(config), { recoverPassword })
	const action = { action: 'recovery_start', email: ' member@example.test ', next: '//bad.example' }
	expect((await (await auth(configRequest())).json()).recovery).toBe(true)
	expect((await auth(post({ ...action, email: 'missing-at' }))).status).toBe(400)
	expect(
		(
			await create(async () => Response.json(config), { recoverPassword, allowedMethods: ['otp'] })(
				post(action),
			)
		).status,
	).toBe(403)
	const res = await auth(post(action))
	expect(res.status).toBe(202)
	expect(await res.json()).toEqual({ step: 'recovery_sent' })
	expect(recoverPassword).toHaveBeenCalledTimes(1)
	expect(recoverPassword).toHaveBeenCalledWith(
		{ email: 'member@example.test', returnTo: '/' },
		expect.any(Request),
	)
})

test('edge trust rejects missing, mismatched and combined headers before dispatch', async () => {
	const fetch = mock<AuthFetch>(async () => Response.json(config))
	const auth = create(fetch, { trustSylphxEdge: true })
	const missing = (name: string) => {
		const headers = new Headers(edgeHeaders)
		headers.delete(name)
		return headers
	}
	const duplicate = (name: keyof typeof edgeHeaders) => {
		const headers = new Headers(edgeHeaders)
		headers.append(name, edgeHeaders[name])
		return headers
	}
	for (const headers of [
		missing('sylphx-edge'),
		missing('x-forwarded-host'),
		missing('x-forwarded-proto'),
		{ ...edgeHeaders, 'sylphx-edge': 'Public' },
		{ ...edgeHeaders, 'sylphx-edge': 'internal' },
		{ ...edgeHeaders, 'x-forwarded-host': 'other.example.test' },
		{ ...edgeHeaders, 'x-forwarded-host': 'app.example.test:443' },
		{ ...edgeHeaders, 'x-forwarded-host': 'https://app.example.test' },
		{ ...edgeHeaders, 'x-forwarded-host': 'app.example.test,other.example.test' },
		{ ...edgeHeaders, 'x-forwarded-proto': 'http' },
		duplicate('sylphx-edge'),
		duplicate('x-forwarded-host'),
		duplicate('x-forwarded-proto'),
	]) {
		for (const base of [origin, revision]) {
			const response = await auth(new Request(`${base}/auth/config`, { headers }))
			expect(response.status).toBe(403)
			expect(await response.json()).toEqual({ error: 'not_allowed' })
		}
	}
	const rewrite = new Request(`${revision}/auth/config`, { headers: edgeHeaders })
	expect((await create(fetch)(rewrite)).status).toBe(403)
	expect((await auth(new Request('http://app.example.test/auth/config'))).status).toBe(403)
	expect(
		(await create(fetch, { trustSylphxEdge: true, isPublicRequest: () => false })(rewrite)).status,
	).toBe(403)
	expect(fetch).not.toHaveBeenCalled()
	expect((await auth(configRequest())).status).toBe(200)
	expect((await auth(rewrite)).status).toBe(200)
	expect((await create(fetch, { isPublicRequest: () => true })(rewrite)).status).toBe(200)
})

test('an admitted edge POST still requires the exact browser Origin and same-origin fetch context', async () => {
	const fetch = mock<AuthFetch>(async () => Response.json(config))
	const auth = create(fetch, { trustSylphxEdge: true })
	const logout = (sender: string, site: string) =>
		new Request(`${revision}/auth/logout`, {
			method: 'POST',
			headers: { ...edgeHeaders, origin: sender, 'sec-fetch-site': site },
		})
	for (const sender of ['', 'http://app.example.test', 'https://other.example.test']) {
		expect((await auth(logout(sender, 'same-origin'))).status).toBe(403)
	}
	expect((await auth(logout(origin, 'cross-site'))).status).toBe(403)
	expect((await auth(logout(origin, 'same-origin'))).status).toBe(204)
	expect(fetch).not.toHaveBeenCalled()
})

test('direct OAuth checks navigation context before start and associates callback metadata with the flow', async () => {
	let callback: URL | undefined
	const authorize = 'https://accounts.google.com/o/oauth2/v2/auth?scope=openid'
	const fetch = mock<AuthFetch>(async (url) => {
		if (url.pathname === '/v1/client/config') return Response.json(config)
		if (url.pathname === '/v1/client/oauth/google/start') {
			callback = new URL(url.searchParams.get('redirect_url')!)
			return Response.redirect(authorize, 303)
		}
		expect(url.pathname).toBe('/v1/client/tickets:redeem')
		return Response.json(result())
	})
	const signUpSignal = mock<NonNullable<SylphxAuthOptions['signUpSignal']>>(async (context) => {
		expect(context).toEqual({ method: 'oauth', provider: 'google', returnTo: next })
		return true
	})
	const auth = create(fetch, { directOAuth: true, signUpSignal, trustSylphxEdge: true })
	const start = (site: string) =>
		new Request(`${revision}/auth/google?returnTo=${encodeURIComponent(next)}`, {
			headers: {
				...edgeHeaders,
				'sec-fetch-site': site,
				'sec-fetch-mode': 'navigate',
				'sec-fetch-dest': 'document',
			},
		})
	expect((await auth(start('cross-site'))).status).toBe(403)
	expect((await auth(new Request(`${origin}/auth/google`))).status).toBe(403)
	const clickwrap = { signingKey: 'mock-signing-key-only-for-test-fixtures', validate: () => true }
	expect(
		(
			await create(fetch, { directOAuth: true, clickwrap, trustSylphxEdge: true })(
				start('same-origin'),
			)
		).status,
	).toBe(403)
	expect(fetch).not.toHaveBeenCalled()
	const res = await auth(start('same-origin'))
	expect(res.status).toBe(303)
	expect(res.headers.get('location')).toBe(authorize)
	const cookie = res.headers.getSetCookie()[0]!.split(';')[0]!
	expect(callback!.origin).toBe(origin)
	callback!.searchParams.set('auth_ticket', 'fixture-ticket')
	const edgeCallback = new URL(callback!.pathname + callback!.search, revision)
	expect(
		(await auth(new Request(edgeCallback, { headers: edgeHeaders }))).headers.get('location'),
	).toStartWith('/login?error=signin')
	expect(fetch).toHaveBeenCalledTimes(2)
	const complete = await auth(new Request(edgeCallback, { headers: { ...edgeHeaders, cookie } }))
	expect(complete.headers.get('location')).toBe(next)
	expect(complete.headers.get('x-sylphx-auth-event')).toBe('sign_up')
	expect(complete.headers.getSetCookie()[1]).toContain('HttpOnly')
	const gpc = await auth(new Request(callback!, { headers: { cookie, 'sec-gpc': '1' } }))
	expect(gpc.headers.get('x-sylphx-auth-event')).toBeNull()
	expect(signUpSignal).toHaveBeenCalledTimes(1)
})
