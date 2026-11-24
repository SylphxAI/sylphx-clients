import { expect, test } from 'bun:test'
import { safeReturnTo } from './client.js'
import { type AuthFetch, createSylphxAuthHandler } from './server.js'

const origin = 'https://app.example.test'
const api = 'https://identity.example.test'
const tenant = 'https://auth.example.test'
const mount = `${tenant}/account`
const instance = { id: 'instance-fixture', slug: 'tenant-fixture', environment: 'production' }
const config = {
	instance,
	portal_origin: tenant,
	portal_url: mount,
	methods: { password: true, sign_up: true },
	social: [
		{
			provider: 'apple',
			credentials: 'custom',
			start_url: `${mount}/oauth/apple/start?instance=${instance.slug}`,
		},
	],
}
function handler(fetch: AuthFetch, whiteLabel = true) {
	return createSylphxAuthHandler({
		origin,
		authApiOrigin: api,
		instance,
		fetch,
		...(whiteLabel && { tenantAuthOrigin: tenant }),
	})
}
function post(path: string, body: object = {}, sender = origin) {
	return new Request(origin + path, {
		method: 'POST',
		headers: { origin: sender, 'content-type': 'application/json' },
		body: JSON.stringify(body),
	})
}

test('redirects remain relative after URL canonicalization', () => {
	for (const refused of [
		'https://untrusted.example/',
		'//untrusted.example',
		'/\\untrusted.example',
		'/%2funtrusted.example',
		'/a/..//untrusted.example',
		'/\n/untrusted.example',
		'/%0dfoo',
	]) {
		expect(safeReturnTo(refused, '/home')).toBe('/home')
	}
	expect(safeReturnTo('/keys?page=2&next=%2Fhome#new')).toBe('/keys?page=2&next=%2Fhome#new')
})

test('white-label starts at the discovered pinned mount and exchanges only the app ticket', async () => {
	const calls: URL[] = []
	let appCallback = ''
	let redeemed = false
	const provider = new URL('https://appleid.apple.com/auth/authorize')
	provider.search = new URLSearchParams({
		redirect_uri: `${mount}/oauth/callback`,
		response_mode: 'form_post',
		nonce: 'fixture-nonce',
	}).toString()
	const auth = handler(async (url, init) => {
		calls.push(url)
		expect(new Headers(init.headers).has('cookie')).toBe(false)
		if (url.pathname === '/v1/client/config') return Response.json(config)
		if (url.origin === tenant) {
			expect(url.pathname).toBe('/account/oauth/apple/start')
			expect(init.redirect).toBe('manual')
			expect(url.searchParams.get('instance')).toBe(instance.slug)
			expect(url.searchParams.get('session_mode')).toBe('browser')
			appCallback = url.searchParams.get('redirect_url')!
			return Response.redirect(provider, 303)
		}
		expect(url.href).toBe(`${api}/v1/client/tickets:redeem?instance=${instance.slug}`)
		if (redeemed) return Response.json({}, { status: 401 })
		redeemed = true
		return Response.json({
			user: { id: 'principal-fixture', email_verified: true },
			session: {
				token: 'fixture-session',
				expires_at: new Date(Date.now() + 60_000).toISOString(),
			},
		})
	})
	const cfg = await auth(new Request(`${origin}/auth/config`))
	expect(await cfg.json()).toEqual({
		providers: ['apple'],
		password: true,
		signup: true,
		passkey: false,
		otp: false,
	})
	const start = await auth(
		post('/auth/start/apple', { next: '/account/profile?range=7d#overview' }),
	)
	expect(start.status).toBe(200)
	expect(await start.json()).toEqual({ authorize: provider.href })
	const [set] = start.headers.getSetCookie()
	expect(set).toEndWith('; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=600')
	const cookie = set!.split(';')[0]!
	const callback = new URL(appCallback)
	expect(callback.origin + callback.pathname).toBe(`${origin}/auth/callback`)
	callback.searchParams.set('auth_ticket', 'fixture-ticket')
	const refused = await auth(new Request(callback, { headers: { cookie: `${cookie}; ${cookie}` } }))
	expect(refused.headers.get('location')).toStartWith('/login?error=signin')
	expect(calls).toHaveLength(3)
	const good = await auth(new Request(callback, { headers: { cookie } }))
	expect(good.headers.get('location')).toBe('/account/profile?range=7d#overview')
	expect(good.headers.getSetCookie()[1]).toStartWith(
		'__Host-auth_session=fixture-session; Path=/; Secure; HttpOnly; SameSite=Lax',
	)
	expect(await good.text()).toBe('')
	const replay = await auth(new Request(callback, { headers: { cookie } }))
	expect(replay.headers.getSetCookie()).toHaveLength(1)
})

test('unverified or other-account discovery never causes an outbound portal request', async () => {
	for (const body of [
		{ ...config, portal_origin: 'https://untrusted.example' },
		{
			...config,
			social: [
				{
					...config.social[0],
					start_url: 'https://untrusted.example/account/oauth/apple/start?instance=tenant-fixture',
				},
			],
		},
		{ ...config, instance: { ...instance, id: 'other-fixture' } },
	]) {
		let count = 0
		const auth = handler(async () => {
			count++
			return Response.json(body)
		})
		expect((await auth(post('/auth/start/apple'))).status).toBe(
			body.instance.id === instance.id ? 403 : 503,
		)
		expect(count).toBe(1)
	}
})

test('a pinned provider response with another callback is refused', async () => {
	const auth = handler(async (url) =>
		url.origin === api
			? Response.json(config)
			: Response.redirect(
					`https://appleid.apple.com/auth/authorize?redirect_uri=${encodeURIComponent(`${api}/v1/oidc/callback`)}`,
					303,
				),
	)
	const res = await auth(post('/auth/start/apple'))
	expect(res.status).toBe(503)
	expect(res.headers.getSetCookie()).toEqual([])
})

test('logout requires the application origin and confirms revocation before clearing cookies', async () => {
	let calls = 0
	const auth = handler(async (url, init) => {
		calls++
		expect(url.href).toBe(`${api}/v1/client/sign-out`)
		expect(init.method).toBe('POST')
		expect(new Headers(init.headers).get('authorization')).toBe('Bearer fixture-session')
		return new Response(null, { status: calls === 1 ? 503 : 204 })
	}, false)
	expect((await auth(post('/auth/logout', {}, tenant))).status).toBe(403)
	expect(calls).toBe(0)
	const req = () =>
		new Request(`${origin}/auth/logout`, {
			method: 'POST',
			headers: { origin, cookie: '__Host-auth_session=fixture-session' },
		})
	const unavailable = await auth(req())
	expect(unavailable.status).toBe(503)
	expect(unavailable.headers.getSetCookie()).toEqual([])
	const signedOut = await auth(req())
	expect(signedOut.status).toBe(204)
	expect(signedOut.headers.getSetCookie()).toEqual([
		'__Host-auth_flow=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0',
		'__Host-auth_session=; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=0',
	])
})
