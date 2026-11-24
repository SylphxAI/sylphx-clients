import assert from 'node:assert/strict'
import { createSylphxAuthHandler } from '@sylphx/auth'
import { safeReturnTo } from '@sylphx/auth/client'

const origin = 'https://app.example.test'
const api = 'https://identity.example.test'
const portal = 'https://auth.example.test/account'
const instance = { id: 'fixture-id', slug: 'fixture-tenant', environment: 'production' }
const ticket = 'fixture-ticket'
const authorize = new URL('https://appleid.apple.com/auth/authorize')
authorize.searchParams.set('redirect_uri', `${portal}/oauth/callback`)
const calls = []
let callback
const handle = createSylphxAuthHandler({
	origin,
	authApiOrigin: api,
	tenantAuthOrigin: new URL(portal).origin,
	instance,
	fetch: async (url, init) => {
		calls.push([url.origin + url.pathname, init.method])
		assert.equal(url.searchParams.get('instance'), instance.slug)
		assert.equal(new Headers(init.headers).has('cookie'), false)
		switch (url.origin + url.pathname) {
			case `${api}/v1/client/config`:
				return Response.json({
					instance,
					portal_origin: new URL(portal).origin,
					portal_url: portal,
					methods: { email_otp: true },
					social: [
						{
							provider: 'apple',
							credentials: 'custom',
							start_url: `${portal}/oauth/apple/start?instance=${instance.slug}`,
						},
					],
				})
			case `${portal}/oauth/apple/start`:
				assert.equal(init.redirect, 'manual')
				callback = new URL(url.searchParams.get('redirect_url'))
				return Response.redirect(authorize, 303)
			case `${api}/v1/client/tickets:redeem`:
				assert.deepEqual(JSON.parse(init.body), { ticket })
				return Response.json({
					user: { id: 'principal-fixture', email_verified: true },
					session: {
						token: 'fixture-session',
						expires_at: new Date(Date.now() + 60_000).toISOString(),
					},
				})
			default:
				assert.fail(`Unexpected Auth request: ${url}`)
		}
	},
})

const next = '/records?mode=grid'
assert.equal(safeReturnTo('//foreign.example.test', next), next)
const start = (sender) =>
	handle(
		new Request(`${origin}/auth/start/apple`, {
			method: 'POST',
			headers: { origin: sender, 'content-type': 'application/json' },
			body: JSON.stringify({ next }),
		}),
	)
const refused = await start('https://foreign.example.test')
assert.equal(refused.status, 403)
assert.deepEqual(refused.headers.getSetCookie(), [])
assert.deepEqual(calls, [])

const config = await handle(new Request(`${origin}/auth/config`))
assert.equal(config.status, 200)
assert.deepEqual(await config.json(), {
	providers: ['apple'],
	password: false,
	signup: false,
	otp: true,
	passkey: false,
})
const started = await start(origin)
assert.equal(started.status, 200)
assert.deepEqual(await started.json(), { authorize: authorize.href })
const attrs = '; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age='
const [flow] = started.headers.getSetCookie()
assert.match(flow, /^__Host-auth_flow=/)
assert.ok(flow.endsWith(`${attrs}600`))
assert.equal(callback.origin + callback.pathname, `${origin}/auth/callback`)
callback.searchParams.set('auth_ticket', ticket)
const complete = await handle(new Request(callback, { headers: { cookie: flow.split(';')[0] } }))
assert.equal(complete.status, 303)
assert.equal(complete.headers.get('location'), next)
const [cleared, session] = complete.headers.getSetCookie()
assert.equal(cleared, `__Host-auth_flow=${attrs}0`)
assert.ok(session.startsWith(`__Host-auth_session=fixture-session${attrs}`))
assert.ok(Number(session.split(attrs)[1]) > 0)
assert.equal(await complete.text(), '')
assert.deepEqual(calls, [
	[`${api}/v1/client/config`, 'GET'],
	[`${api}/v1/client/config`, 'GET'],
	[`${portal}/oauth/apple/start`, 'GET'],
	[`${api}/v1/client/tickets:redeem`, 'POST'],
])
console.info(
	'Node Fetch: config, foreign-origin refusal, tenant OAuth start, ticket exchange and host-only cookies passed',
)
