import {
	type EntryConfig,
	isProvider,
	type Provider,
	providerDestination,
	providers,
	safeReturnTo,
} from './client.js'
import { emailEntry } from './email.js'
import {
	type AuthCookie,
	type AuthFetch,
	cookieName,
	type Data,
	discard,
	field,
	headers,
	type Identity,
	json,
	jsonBody,
	obj,
	problem,
	RANDOM,
	random,
	readCookie,
	redirect,
	reply,
	token,
	transport,
} from './http.js'

export type { AuthCookie, AuthFetch } from './http.js'
export { jsonBody as readAuthJson } from './http.js'
export { exchangeManagementSession } from './session.js'

export type IdentitySession = { token: string; expiresAt: unknown }
export type IssueSession = (session: IdentitySession) => Promise<readonly AuthCookie[] | undefined>

type SessionAdapter = {
	issue(session: IdentitySession, request: Request): ReturnType<IssueSession>
	logout(request: Request): Promise<Response>
}
export type SylphxAuthOptions = {
	/** Trusted public origin serving the application, never an unvalidated Host header. */
	origin: string
	/** Trusted Auth API origin. Only the handler sees this value. */
	authApiOrigin: string
	instance: { slug: string; id: string; environment: string }
	/** Pin the origin of the tenant's verified Account Portal for BYO OAuth. Auth discovers its mount path. */
	tenantAuthOrigin?: string
	providers?: readonly Provider[]
	/** Transitional alias for a preexisting client that posts action and provider to /auth/start. */
	legacyStart?: boolean
	loginPath?: string
	defaultReturnTo?: string
	allowReturnTo?: (path: string) => boolean
	flowCookie?: string
	sessionCookie?: string
	/** For an existing edge: check a trusted host binding on the internal request. */
	isPublicRequest?: (request: Request) => boolean
	/** Only for hosts with an existing authoritative session and revocation mechanism. */
	session?: SessionAdapter
	fetch?: AuthFetch
}

const FLOW_AGE = 600
const START = '/auth/start/'
const CALLBACK = '/auth/callback'

function secureUrl(value: unknown): URL | undefined {
	if (typeof value !== 'string') return
	try {
		const url = new URL(value)
		if (url.protocol === 'https:' && !url.username && !url.password && !url.hash) return url
	} catch {
		// Unusable URL.
	}
	return undefined
}

function requireOrigin(value: string) {
	const url = secureUrl(value)
	if (!url || url.pathname !== '/' || url.search)
		throw new TypeError('Expected a trusted HTTPS origin')
	return url.origin
}

function portalStart(
	provider: Provider,
	row: Data,
	discovery: Data,
	pin: string | undefined,
	slug: string,
) {
	// This is a server-side fetch target, not a URL forwarded from a request.
	const url = secureUrl(row.start_url)
	const mount = secureUrl(discovery.portal_url)
	const mountPath = mount?.pathname.replace(/\/$/, '')
	if (
		!pin ||
		discovery.portal_origin !== pin ||
		!url ||
		!mount ||
		mount.search ||
		url.origin !== pin ||
		mount.origin !== pin ||
		url.pathname !== `${mountPath}/oauth/${provider}/start` ||
		url.searchParams.size !== 1 ||
		url.searchParams.get('instance') !== slug
	)
		return
	return { url, redirectUri: `${mount.origin + mountPath}/oauth/callback` }
}

export function createSylphxAuthHandler(
	options: SylphxAuthOptions,
): (request: Request) => Promise<Response> {
	const origin = requireOrigin(options.origin)
	const api = requireOrigin(options.authApiOrigin)
	const tenantAuthOrigin =
		options.tenantAuthOrigin === undefined ? undefined : requireOrigin(options.tenantAuthOrigin)
	if (tenantAuthOrigin && tenantAuthOrigin === api)
		throw new TypeError('Use the tenant Auth origin, not the Auth API origin')
	const { slug, id, environment } = options.instance
	if (!/^[a-z0-9][a-z0-9-]{0,62}$/.test(slug) || !id || !environment)
		throw new TypeError('Expected an Auth instance')
	const instance = { instance: slug }
	const loginPath = safeReturnTo(options.loginPath, '/login')
	if (/[?#]/.test(loginPath)) throw new TypeError('Expected a login pathname')
	const fallback = safeReturnTo(options.defaultReturnTo)
	const nextPath = (value: unknown) => {
		const next = safeReturnTo(value, fallback)
		return options.allowReturnTo && !options.allowReturnTo(next) ? fallback : next
	}
	const flowName = cookieName(options.flowCookie ?? '__Host-auth_flow')
	const sessionName = cookieName(options.sessionCookie ?? '__Host-auth_session')
	if (
		flowName === sessionName ||
		![flowName, sessionName].every((name) => name.startsWith('__Host-'))
	)
		throw new TypeError('Use separate host-only cookie names')
	const clearFlow: AuthCookie = [flowName, '', 0]
	const callback = origin + CALLBACK
	const fetchUpstream = options.fetch ?? ((url, init) => fetch(url, init))
	const social = [...new Set(options.providers ?? Object.keys(providers).filter(isProvider))]

	async function discover(identity: Identity) {
		const res = await identity('/v1/client/config', { query: instance })
		const data = await json(res)
		const resolved = obj(data.instance)
		if (
			!res.ok ||
			resolved.id !== id ||
			resolved.slug !== slug ||
			resolved.environment !== environment
		)
			throw new Error('Unexpected authentication instance')
		const methods = obj(data.methods)
		const redirects = new Map<Provider, ReturnType<typeof portalStart> | null>()
		const rows = Array.isArray(data.social) ? data.social.map(obj) : []
		for (const provider of social) {
			const entries = rows.filter((row) => row.provider === provider)
			if (entries.length !== 1) continue
			const row = entries[0]!
			if (row.credentials === 'shared') redirects.set(provider, null)
			else if (row.credentials === 'custom') {
				const start = portalStart(provider, row, data, tenantAuthOrigin, slug)
				if (start) redirects.set(provider, start)
			}
		}
		const config: EntryConfig = {
			providers: [...redirects.keys()],
			password: methods.password === true,
			signup: methods.sign_up === true && methods.password === true,
			otp: methods.email_otp === true,
			passkey: methods.passkey === true,
		}
		return { config, redirects }
	}

	async function start(identity: Identity, provider: unknown, next: string) {
		if (!isProvider(provider) || !social.includes(provider)) return problem('unknown_provider', 400)
		const { redirects } = await discover(identity)
		const start = redirects.get(provider)
		if (start === undefined) return problem('method_unavailable', 403)
		const state = random()
		const target = new URL(callback)
		target.searchParams.set('flow', state)
		// Auth owns upstream PKCE S256, nonce and Apple's form_post. This
		// app's GET callback only receives Auth's one-use sign-in ticket.
		const res = await identity(start?.url ?? `/v1/client/oauth/${provider}/start`, {
			query: { ...instance, redirect_url: target.href, session_mode: 'browser' },
			manual: true,
		})
		const authorize =
			res.status === 303 && providerDestination(provider, res.headers.get('location'))
		discard(res)
		if (!authorize) return problem('method_unavailable', 503)
		if (start) {
			const callbacks = new URL(authorize).searchParams.getAll('redirect_uri')
			if (callbacks.length !== 1 || callbacks[0] !== start.redirectUri)
				return problem('method_unavailable', 503)
		}
		const expires = Math.floor(Date.now() / 1000) + FLOW_AGE
		const path = encodeURIComponent(next.slice(1)).replaceAll('.', '%2E')
		return reply({ authorize }, 200, [[flowName, [state, expires, path].join('.'), FLOW_AGE]])
	}

	function issue(request: Request): IssueSession {
		return async (session) => {
			const cookies = options.session
				? await options.session.issue(session, request)
				: sessionCookie(session)
			if (!cookies?.length) return
			for (const [name, value, age] of cookies) {
				if (cookieName(name) === flowName || !token(value) || !Number.isSafeInteger(age) || age < 1)
					return
			}
			return cookies
		}
	}

	function sessionCookie(session: IdentitySession): AuthCookie[] | undefined {
		const expires = typeof session.expiresAt === 'string' ? Date.parse(session.expiresAt) : NaN
		const age = Math.floor((expires - Date.now()) / 1000)
		return Number.isFinite(age) && age >= 1 ? [[sessionName, session.token, age]] : undefined
	}

	async function logout(request: Request, identity: Identity) {
		if (options.session) {
			const res = await options.session.logout(request)
			const out = new Headers(res.headers)
			if (res.ok) out.append('Set-Cookie', headers([clearFlow]).getSetCookie()[0]!)
			return new Response(res.body, { status: res.status, headers: out })
		}
		const credential = readCookie(request, sessionName)
		if (token(credential)) {
			const res = await identity('/v1/client/sign-out', { method: 'POST', bearer: credential })
			discard(res)
			if (res.status !== 204) return problem('unavailable', 503)
		}
		return new Response(null, { status: 204, headers: headers([clearFlow, [sessionName, '', 0]]) })
	}

	function readFlow(request: Request) {
		const parts = readCookie(request, flowName)?.split('.')
		if (parts?.length !== 3) return
		const [state, deadline, path] = parts
		const now = Math.floor(Date.now() / 1000)
		if (
			!state ||
			!RANDOM.test(state) ||
			!deadline ||
			!/^\d{10}$/.test(deadline) ||
			Number(deadline) <= now ||
			Number(deadline) > now + FLOW_AGE
		)
			return
		try {
			return { state, next: nextPath(`/${decodeURIComponent(path ?? '')}`) }
		} catch {
			return undefined
		}
	}

	async function complete(request: Request, url: URL, identity: Identity) {
		const flow = readFlow(request)
		const fail = (error = 'signin') =>
			redirect(`${loginPath}?${new URLSearchParams({ error, next: flow?.next ?? fallback })}`, [
				clearFlow,
			])
		const returned = url.searchParams.getAll('flow')
		const state = returned[0]
		if (
			!flow ||
			returned.length !== 1 ||
			!state ||
			!RANDOM.test(state) ||
			!sameState(flow.state, state)
		)
			return fail()
		const error = url.searchParams.get('auth_error')
		if (error)
			return fail(
				error === 'access_denied'
					? 'cancelled'
					: ['unverified_email', 'account_exists'].includes(error)
						? 'use_existing'
						: 'signin',
			)
		const tickets = url.searchParams.getAll('auth_ticket')
		const ticket = tickets[0]
		if (tickets.length !== 1 || !token(ticket)) return fail()
		try {
			const res = await identity('/v1/client/tickets:redeem', { query: instance, body: { ticket } })
			const data = await json(res)
			const session = obj(data.session)
			if (!res.ok || !token(session.token)) return fail()
			if (obj(data.user).email_verified !== true) return fail('use_existing')
			const cookies = await issue(request)({ token: session.token, expiresAt: session.expires_at })
			return cookies ? redirect(flow.next, [clearFlow, ...cookies]) : fail()
		} catch {
			return fail()
		}
	}

	return async (request) => {
		const url = new URL(request.url)
		if (!(options.isPublicRequest?.(request) ?? url.origin === origin))
			return problem('not_allowed', 403)
		const { pathname } = url
		const identity = transport(request, fetchUpstream, api)
		if (pathname === CALLBACK)
			return request.method === 'GET' ? complete(request, url, identity) : problem('not_found', 404)
		const legacy = options.legacyStart && pathname === '/auth/start'
		if (
			!legacy &&
			!['/auth/config', '/auth/email', '/auth/logout'].includes(pathname) &&
			!pathname.startsWith(START)
		)
			return problem('not_found', 404)
		const method = pathname === '/auth/config' ? 'GET' : 'POST'
		const requestOrigin = request.headers.get('origin')
		const site = request.headers.get('sec-fetch-site')
		if (
			request.method !== method ||
			(site && site !== 'same-origin') ||
			(method === 'POST' ? requestOrigin !== origin : requestOrigin && requestOrigin !== origin)
		)
			return problem('not_allowed', 403)
		try {
			if (pathname === '/auth/config') return reply((await discover(identity)).config)
			if (pathname === '/auth/logout') return await logout(request, identity)
			if (!/^application\/json(?:\s*;.*)?$/i.test(request.headers.get('content-type') ?? ''))
				return problem('invalid_input', 415)
			let input: Data
			try {
				input = await jsonBody(request)
			} catch {
				return problem('invalid_input', 400)
			}
			const next = nextPath(field(input, 'next', 1024))
			if (pathname.startsWith(START))
				return await start(identity, pathname.slice(START.length), next)
			if (legacy && input.action === 'social') return await start(identity, input.provider, next)
			return await emailEntry(identity, input, next, {
				origin,
				loginPath,
				instance,
				session: issue(request),
			})
		} catch {
			return problem('unavailable', 503)
		}
	}
}

function sameState(a: string, b: string) {
	let difference = 0
	for (let i = 0; i < 43; i++) difference |= a.charCodeAt(i) ^ b.charCodeAt(i)
	return difference === 0
}
