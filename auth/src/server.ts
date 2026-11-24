import {
	type AcceptanceVersions,
	acceptanceSigner,
	type ClickwrapPolicy,
	decodeAcceptance,
	encodeAcceptance,
	readAcceptance,
} from './acceptance.js'
import {
	authRoutePrefix,
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

export type { AcceptanceVersions, ClickwrapPolicy } from './acceptance.js'
export type { AuthCookie, AuthFetch } from './http.js'
export { jsonBody as readAuthJson } from './http.js'
export { exchangeManagementSession } from './session.js'

export type IdentitySession = { token: string; expiresAt: unknown }
export type IssueSession = (session: IdentitySession) => Promise<readonly AuthCookie[] | undefined>

export type VerifiedIdentity = {
	id: string
	email?: string
	name?: string
	username?: string
	emailVerified: true
}
export type VerifiedIdentitySession = IdentitySession & {
	sessionId?: string
	user: VerifiedIdentity
	isNewUser: boolean
	acceptance?: AcceptanceVersions
}
export type SessionAdapter = {
	issue(session: VerifiedIdentitySession, request: Request): ReturnType<IssueSession>
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
	/** Same-origin mount pathname, e.g. /account/auth. Defaults to /auth. */
	routePrefix?: string
	/** Disable new password registration, not password sign-in. */
	allowPasswordRegistration?: boolean
	/** App-owned policy. Ignored unless configured; when configured, fails closed. */
	clickwrap?: ClickwrapPolicy
	defaultReturnTo?: string
	allowReturnTo?: (path: string) => boolean
	flowCookie?: string
	sessionCookie?: string
	/** For an existing edge: check a trusted host binding on the internal request. */
	isPublicRequest?: (request: Request) => boolean
	/** Only for hosts with an existing authoritative session and revocation mechanism. */
	session?: SessionAdapter
	fetch?: AuthFetch
} & (
	| { sessionMode?: 'browser'; sessionSecret?: never }
	| { sessionMode: 'server'; sessionSecret: string }
)

const FLOW_AGE = 600

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
	const prefix = authRoutePrefix(options.routePrefix)
	const startRoute = `${prefix}/start`
	const callbackRoute = `${prefix}/callback`
	const configRoute = `${prefix}/config`
	const emailRoute = `${prefix}/email`
	const logoutRoute = `${prefix}/logout`
	const mode = options.sessionMode ?? 'browser'
	const secret = options.sessionSecret
	if (
		mode === 'server' &&
		(!token(secret) ||
			(!secret.startsWith('sylphx_sk_') && !secret.startsWith('identity_org_key_')))
	)
		throw new TypeError('Expected an instance secret key for server mode')
	const clickwrap = options.clickwrap
	const signer = clickwrap && acceptanceSigner(clickwrap.signingKey)
	const allowRegistration = options.allowPasswordRegistration !== false
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
	const callback = origin + callbackRoute
	const signable = (value: string) => JSON.stringify([callback, id, value])

	function accepted(versions: AcceptanceVersions | undefined, request: Request) {
		return clickwrap ? versions && clickwrap.validate(versions, request) : true
	}

	async function redeem(identity: Identity, ticket: string) {
		const res = await identity('/v1/client/tickets:redeem', {
			query: instance,
			body: { ticket },
			...(mode === 'server' && secret && { bearer: secret }),
		})
		if (!res.ok) {
			discard(res)
			return undefined
		}
		return json(res)
	}
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
			signup: allowRegistration && methods.sign_up === true && methods.password === true,
			otp: methods.email_otp === true,
			passkey: methods.passkey === true,
		}
		return { config, redirects }
	}

	async function start(
		identity: Identity,
		provider: unknown,
		next: string,
		acceptance: AcceptanceVersions | undefined,
	) {
		if (!isProvider(provider) || !social.includes(provider)) return problem('unknown_provider', 400)
		const { redirects } = await discover(identity)
		const start = redirects.get(provider)
		if (start === undefined) return problem('method_unavailable', 403)
		const state = random()
		const expires = Math.floor(Date.now() / 1000) + FLOW_AGE
		const path = encodeURIComponent(next.slice(1)).replaceAll('.', '%2E')
		let value = [state, expires, path].join('.')
		if (signer && acceptance) {
			value += `.${encodeAcceptance(acceptance)}`
			value += `.${await signer.sign(signable(value))}`
			if (value.length > 3600) return problem('invalid_input', 400)
		}
		const target = new URL(callback)
		target.searchParams.set('flow', state)
		// Auth owns upstream PKCE S256, nonce and Apple's form_post. This
		// app's GET callback only receives Auth's one-use sign-in ticket.
		const res = await identity(start?.url ?? `/v1/client/oauth/${provider}/start`, {
			query: { ...instance, redirect_url: target.href, session_mode: mode },
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
		return reply({ authorize }, 200, [[flowName, value, FLOW_AGE]])
	}

	function issue(request: Request) {
		return async (session: VerifiedIdentitySession) => {
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
		const age = Math.floor(expirySeconds(session.expiresAt) - Date.now() / 1000)
		return Number.isSafeInteger(age) && age >= 1 ? [[sessionName, session.token, age]] : undefined
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

	async function readFlow(request: Request) {
		const value = readCookie(request, flowName)
		if (!value || (signer && value.length > 3600)) return
		const parts = value.split('.')
		if (parts.length !== (signer ? 5 : 3)) return
		const [state, deadline, path, encoded, mac] = parts
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
			let acceptance: AcceptanceVersions | undefined
			if (signer) {
				if (
					!encoded ||
					!mac ||
					!(await signer.verify(signable(value.slice(0, -mac.length - 1)), mac))
				)
					return
				acceptance = decodeAcceptance(encoded)
				if (!acceptance) return
			}
			return { state, next: nextPath(`/${decodeURIComponent(path ?? '')}`), acceptance }
		} catch {
			return undefined
		}
	}

	async function complete(request: Request, url: URL, identity: Identity) {
		const flow = await readFlow(request)
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
			if ((await accepted(flow.acceptance, request)) !== true) return fail('acceptance_required')
			const data = await redeem(identity, ticket)
			if (!data) return fail()
			if (obj(data.user).email_verified !== true) return fail('use_existing')
			const session = verifiedSession(data, flow.acceptance)
			if (!session) return fail()
			if ((await accepted(flow.acceptance, request)) !== true) return fail('acceptance_required')
			const cookies = await issue(request)(session)
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
		if (pathname === callbackRoute)
			return request.method === 'GET' ? complete(request, url, identity) : problem('not_found', 404)
		const legacy = options.legacyStart && pathname === startRoute
		if (
			!legacy &&
			![configRoute, emailRoute, logoutRoute].includes(pathname) &&
			!pathname.startsWith(`${startRoute}/`)
		)
			return problem('not_found', 404)
		const method = pathname === configRoute ? 'GET' : 'POST'
		const requestOrigin = request.headers.get('origin')
		const site = request.headers.get('sec-fetch-site')
		if (
			request.method !== method ||
			(site && site !== 'same-origin') ||
			(method === 'POST' ? requestOrigin !== origin : requestOrigin && requestOrigin !== origin)
		)
			return problem('not_allowed', 403)
		try {
			if (pathname === configRoute) return reply((await discover(identity)).config)
			if (pathname === logoutRoute) return await logout(request, identity)
			if (!/^application\/json(?:\s*;.*)?$/i.test(request.headers.get('content-type') ?? ''))
				return problem('invalid_input', 415)
			let input: Data
			try {
				input = await jsonBody(request)
			} catch {
				return problem('invalid_input', 400)
			}
			const acceptance = clickwrap ? readAcceptance(input.acceptance) : undefined
			if ((await accepted(acceptance, request)) !== true) return problem('acceptance_required', 403)
			const next = nextPath(field(input, 'next', 1024))
			if (pathname.startsWith(`${startRoute}/`))
				return await start(identity, pathname.slice(startRoute.length + 1), next, acceptance)
			if (legacy && input.action === 'social')
				return await start(identity, input.provider, next, acceptance)
			return await emailEntry(identity, input, next, {
				origin,
				loginPath,
				instance,
				mode,
				allowRegistration,
				redeem: (ticket) => redeem(identity, ticket),
				session: async (data) => {
					if ((await accepted(acceptance, request)) !== true)
						return problem('acceptance_required', 403)
					const session = verifiedSession(data, acceptance)
					const cookies = session && (await issue(request)(session))
					return cookies ? reply({ next }, 200, cookies) : problem('session_unavailable', 502)
				},
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

function verifiedSession(
	data: Data,
	acceptance: AcceptanceVersions | undefined,
): VerifiedIdentitySession | undefined {
	const session = obj(data.session)
	const user = obj(data.user)
	if (!token(session.token) || !token(user.id) || user.email_verified !== true) return
	return {
		token: session.token,
		expiresAt: session.expires_at,
		...(token(session.id) && { sessionId: session.id }),
		user: {
			id: user.id,
			emailVerified: true,
			...(typeof user.email === 'string' && { email: user.email }),
			...(typeof user.name === 'string' && { name: user.name }),
			...(typeof user.username === 'string' && { username: user.username }),
		},
		isNewUser: data.is_new_user === true,
		...(acceptance && { acceptance }),
	}
}

function expirySeconds(value: unknown): number {
	if (typeof value === 'number' || (typeof value === 'string' && /^[0-9]+$/.test(value))) {
		const seconds = Number(value)
		// Limit Unix seconds to year 9999, matching the ISO envelope and rejecting millisecond epochs.
		return Number.isSafeInteger(seconds) && seconds <= 253_402_300_799 ? seconds : NaN
	}
	if (typeof value !== 'string') return NaN
	if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/i.test(value))
		return NaN
	return Date.parse(value) / 1000
}
