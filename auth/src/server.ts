import {
	type AcceptanceVersions,
	acceptanceSigner,
	type ClickwrapPolicy,
	decodeAcceptance,
	encodeAcceptance,
	readAcceptance,
} from './acceptance.js'
import {
	type AuthMethod,
	authRoutePrefix,
	isProvider,
	type Provider,
	providerDestination,
	providers,
	safeReturnTo,
} from './client.js'
import { bindInstance, emailMethods, entryConfig, type InstanceBinding } from './discovery.js'
import { emailEntry } from './email.js'
import { enrollPasskey } from './enrollment.js'
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
import { readAuthSessionCookie } from './session-cookie.js'

export type { AcceptanceVersions, ClickwrapPolicy } from './acceptance.js'
export type { AuthMethod } from './client.js'
export type { AuthCookie, AuthFetch } from './http.js'
export { jsonBody as readAuthJson } from './http.js'
export { exchangeManagementSession } from './session.js'
export { readAuthSessionCookie, type SessionCookieSource } from './session-cookie.js'

export type IdentitySession = { token: string; expiresAt: unknown }
export type IssueSession = (session: IdentitySession) => Promise<readonly AuthCookie[] | undefined>

export type VerifiedIdentity = {
	id: string
	email?: string
	name?: string
	username?: string
	emailVerified: true
}
export type AuthIssueContext = { method: AuthMethod; provider?: Provider; returnTo: string }
export type VerifiedIdentitySession = IdentitySession &
	AuthIssueContext & {
		sessionId?: string
		user: VerifiedIdentity
		isNewUser: boolean
		acceptance?: AcceptanceVersions
	}
export type UnverifiedPasswordSession = Omit<VerifiedIdentitySession, 'user' | 'method'> & {
	method: 'password'
	user: Omit<VerifiedIdentity, 'emailVerified'> & { emailVerified: false }
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
	allowedMethods?: readonly AuthMethod[]
	/** Separate app-owned handoff for proof-pending sessions; never calls the verified issuer. */
	issueUnverifiedPassword?: (
		session: UnverifiedPasswordSession,
		request: Request,
	) => ReturnType<IssueSession>
	/** Host sends the generic recovery message and owns mailed proof completion. */
	recoverPassword?: (input: { email: string; returnTo: string }, request: Request) => Promise<void>
	/** Opt in to same-origin top-level GET /<provider>; clickwrap deployments use POST. */
	directOAuth?: boolean
	/** Non-sensitive marker emitted only for a new user after host consent and absent GPC. */
	signUpSignal?: (
		context: Readonly<AuthIssueContext>,
		request: Request,
	) => boolean | Promise<boolean>
	/** Fallback names when the canonical cookie is absent; issuer must still authenticate it. */
	legacyCookies?: readonly string[]
	/** Resolve an Identity session token from an authenticated app session for enrollment. */
	passkeyEnrollment?: true | ((request: Request) => Promise<string | undefined>)
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
} & InstanceBinding &
	(
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
	const binding = bindInstance(options)
	const allowed = options.allowedMethods && new Set(options.allowedMethods)
	const allows = (method: AuthMethod) => !allowed || allowed.has(method)
	const prefix = authRoutePrefix(options.routePrefix)
	const startRoute = `${prefix}/start`
	const callbackRoute = `${prefix}/callback`
	const configRoute = `${prefix}/config`
	const emailRoute = `${prefix}/email`
	const enrollmentRoute = `${prefix}/passkey/enroll`
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
	const signable = (value: string) =>
		JSON.stringify([callback, options.instance?.id ?? options.publishableKey, value])
	const legacyCookies = (options.legacyCookies ?? []).map(cookieName)
	if (new Set([flowName, sessionName, ...legacyCookies]).size !== legacyCookies.length + 2)
		throw new TypeError('Use distinct session and flow cookie names')
	const sessionSource = (request: Request) =>
		readAuthSessionCookie(request, { sessionCookie: sessionName, legacyCookies })

	function accepted(versions: AcceptanceVersions | undefined, request: Request) {
		return clickwrap ? versions && clickwrap.validate(versions, request) : true
	}

	async function redeem(identity: Identity, ticket: string) {
		const res = await identity('/v1/client/tickets:redeem', {
			...binding.scope,
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
		const { data, instance } = await binding.discover(identity)
		const redirects = new Map<Provider, ReturnType<typeof portalStart> | null>()
		const rows = Array.isArray(data.social) ? data.social.map(obj) : []
		for (const provider of allows('oauth') ? social : []) {
			const entries = rows.filter((row) => row.provider === provider)
			if (entries.length !== 1) continue
			const row = entries[0]!
			if (row.credentials === 'shared') redirects.set(provider, null)
			else if (row.credentials === 'custom') {
				const start = portalStart(provider, row, data, tenantAuthOrigin, instance.slug)
				if (start) redirects.set(provider, start)
			}
		}
		const config = entryConfig(obj(data.methods), [...redirects.keys()], allows, allowRegistration)
		if (options.recoverPassword) config.recovery = config.password
		if (options.passkeyEnrollment) config.passkeyEnrollment = config.passkey
		return { config, redirects, instance }
	}

	async function start(
		identity: Identity,
		provider: unknown,
		next: string,
		acceptance: AcceptanceVersions | undefined,
		navigate = false,
	) {
		if (!isProvider(provider) || !social.includes(provider)) return problem('unknown_provider', 400)
		if (!allows('oauth')) return problem('method_unavailable', 403)
		const { redirects, instance } = await discover(identity)
		const start = redirects.get(provider)
		if (start === undefined) return problem('method_unavailable', 403)
		const state = random()
		const expires = Math.floor(Date.now() / 1000) + FLOW_AGE
		const encode = (value: string) => encodeURIComponent(value).replaceAll('.', '%2E')
		const path = encode(next.slice(1))
		let value = [state, expires, path, provider, encode(instance.id)].join('.')
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
			...(start ? {} : binding.scope),
			query: {
				...(start ? { instance: instance.slug } : binding.scope.query),
				redirect_url: target.href,
				session_mode: mode,
			},
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
		const cookies: AuthCookie[] = [[flowName, value, FLOW_AGE]]
		return navigate ? redirect(authorize, cookies) : reply({ authorize }, 200, cookies)
	}

	function validatedCookies(cookies: readonly AuthCookie[] | undefined) {
		if (!cookies?.length) return
		const seen = new Set([flowName])
		for (const [name, value, age] of cookies) {
			if (seen.has(cookieName(name)) || !token(value) || !Number.isSafeInteger(age) || age < 1)
				return
			seen.add(name)
		}
		return cookies
	}

	async function issue(
		request: Request,
		session: VerifiedIdentitySession,
		context: AuthIssueContext,
	) {
		const cookies = validatedCookies(
			options.session ? await options.session.issue(session, request) : sessionCookie(session),
		)
		if (!cookies) return
		let signUp = false
		if (session.isNewUser && !request.headers.has('sec-gpc')) {
			try {
				signUp = (await options.signUpSignal?.(context, request)) === true
			} catch {
				// Optional analytics must not prevent sign-in.
			}
		}
		return { cookies, signUp }
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
		const credential = sessionSource(request)?.value
		if (token(credential)) {
			const res = await identity('/v1/client/sign-out', { method: 'POST', bearer: credential })
			discard(res)
			if (res.status !== 204) return problem('unavailable', 503)
		}
		return new Response(null, {
			status: 204,
			headers: headers([
				clearFlow,
				...[sessionName, ...legacyCookies].map((name): AuthCookie => [name, '', 0]),
			]),
		})
	}

	async function readFlow(request: Request) {
		const value = readCookie(request, flowName)
		if (!value || (signer && value.length > 3600)) return
		const parts = value.split('.')
		if (parts.length !== (signer ? 7 : 5)) return
		const [state, deadline, path, provider, instanceId, encoded, mac] = parts
		const now = Math.floor(Date.now() / 1000)
		if (
			!isProvider(provider) ||
			!instanceId ||
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
			return {
				state,
				provider,
				instanceId: decodeURIComponent(instanceId),
				next: nextPath(`/${decodeURIComponent(path ?? '')}`),
				acceptance,
			}
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
			if (!allows('oauth') || !social.includes(flow.provider)) return fail()
			const pinned = options.publishableKey
				? (await binding.discover(identity)).instance
				: options.instance!
			if (flow.instanceId !== pinned.id) return fail()
			const data = await redeem(identity, ticket)
			if (!data) return fail()
			const context: AuthIssueContext = {
				method: 'oauth',
				provider: flow.provider,
				returnTo: flow.next,
			}
			const session = identitySession(data, flow.acceptance, context)
			if (!session) return fail()
			if (!session.user.emailVerified) return fail('use_existing')
			if ((await accepted(flow.acceptance, request)) !== true) return fail('acceptance_required')
			const issued = await issue(
				request,
				{ ...session, user: { ...session.user, emailVerified: true } },
				context,
			)
			return issued
				? signUpHeader(redirect(flow.next, [clearFlow, ...issued.cookies]), issued.signUp)
				: fail()
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
		const direct =
			options.directOAuth &&
			pathname.startsWith(`${prefix}/`) &&
			isProvider(pathname.slice(prefix.length + 1))
		const legacy = options.legacyStart && pathname === startRoute
		if (
			!direct &&
			!legacy &&
			![configRoute, emailRoute, logoutRoute, enrollmentRoute].includes(pathname) &&
			!pathname.startsWith(`${startRoute}/`)
		)
			return problem('not_found', 404)
		const method = pathname === configRoute || direct ? 'GET' : 'POST'
		const requestOrigin = request.headers.get('origin')
		const site = request.headers.get('sec-fetch-site')
		if (
			request.method !== method ||
			(site && site !== 'same-origin') ||
			(direct &&
				(site !== 'same-origin' ||
					request.headers.get('sec-fetch-mode') !== 'navigate' ||
					request.headers.get('sec-fetch-dest') !== 'document')) ||
			(method === 'POST' ? requestOrigin !== origin : requestOrigin && requestOrigin !== origin)
		)
			return problem('not_allowed', 403)
		try {
			if (direct) {
				if (clickwrap) return problem('acceptance_required', 403)
				const next = nextPath(url.searchParams.get('returnTo') ?? url.searchParams.get('next'))
				return await start(identity, pathname.slice(prefix.length + 1), next, undefined, true)
			}
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
			if (pathname === enrollmentRoute) {
				if (!options.passkeyEnrollment || !allows('passkey'))
					return problem('method_unavailable', 403)
				const credential =
					options.passkeyEnrollment === true
						? sessionSource(request)?.value
						: await options.passkeyEnrollment(request)
				if (!token(credential)) return problem('not_allowed', 403)
				const { config, instance } = await discover(identity)
				if (!config.passkey) return problem('method_unavailable', 403)
				return await enrollPasskey(identity, credential, instance.id, origin, input)
			}
			const acceptance = clickwrap ? readAcceptance(input.acceptance) : undefined
			if ((await accepted(acceptance, request)) !== true) return problem('acceptance_required', 403)
			const next = nextPath(field(input, 'next', 1024))
			if (pathname.startsWith(`${startRoute}/`))
				return await start(identity, pathname.slice(startRoute.length + 1), next, acceptance)
			if (legacy && input.action === 'social')
				return await start(identity, input.provider, next, acceptance)
			if (typeof input.action !== 'string' || !Object.hasOwn(emailMethods, input.action))
				return problem('not_found', 404)
			const method = emailMethods[input.action as keyof typeof emailMethods]
			if (!allows(method) || (input.action === 'recovery_start' && !options.recoverPassword))
				return problem('method_unavailable', 403)
			if (
				allowed ||
				(options.instance && options.publishableKey) ||
				input.action === 'recovery_start'
			) {
				const { config } = await discover(identity)
				if (!config[method] || (input.action === 'signup' && !config.signup))
					return problem('method_unavailable', 403)
			}
			if (input.action === 'recovery_start') {
				if (!options.recoverPassword) return problem('method_unavailable', 403)
				const email = field(input, 'email', 254)?.trim()
				if (!email || !/^[^\s@]+@[^\s@]+$/.test(email)) return problem('invalid_input', 400)
				await options.recoverPassword({ email, returnTo: next }, request)
				return reply({ step: 'recovery_sent' }, 202)
			}
			return await emailEntry(identity, input, next, {
				origin,
				loginPath,
				mode,
				allowRegistration,
				scope: binding.scope,
				redeem: (ticket) => redeem(identity, ticket),
				session: async (data, method, legacyPassword) => {
					if ((await accepted(acceptance, request)) !== true)
						return problem('acceptance_required', 403)
					const context: AuthIssueContext = { method, returnTo: next }
					const session = identitySession(data, acceptance, context)
					if (!session) return problem('session_unavailable', 502)
					if (session.user.emailVerified === false) {
						if (!legacyPassword || !options.issueUnverifiedPassword)
							return problem('verify_email', 403)
						const cookies = validatedCookies(
							await options.issueUnverifiedPassword(
								{
									...session,
									method: 'password',
									user: { ...session.user, emailVerified: false },
								},
								request,
							),
						)
						return cookies ? reply({ next }, 200, cookies) : problem('verify_email', 403)
					}
					const issued = await issue(
						request,
						{ ...session, user: { ...session.user, emailVerified: true } },
						context,
					)
					return issued
						? signUpHeader(reply({ next }, 200, issued.cookies), issued.signUp)
						: problem('session_unavailable', 502)
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

function identitySession(
	data: Data,
	acceptance: AcceptanceVersions | undefined,
	context: AuthIssueContext,
) {
	const session = obj(data.session)
	const user = obj(data.user)
	if (!token(session.token) || !token(user.id) || typeof user.email_verified !== 'boolean') return
	return {
		...context,
		token: session.token,
		expiresAt: session.expires_at,
		...(token(session.id) && { sessionId: session.id }),
		user: {
			id: user.id,
			emailVerified: user.email_verified,
			...(typeof user.email === 'string' && { email: user.email }),
			...(typeof user.name === 'string' && { name: user.name }),
			...(typeof user.username === 'string' && { username: user.username }),
		},
		isNewUser: data.is_new_user === true,
		...(acceptance && { acceptance }),
	}
}

function signUpHeader(response: Response, emit: boolean) {
	if (emit) response.headers.set('X-Sylphx-Auth-Event', 'sign_up')
	return response
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
