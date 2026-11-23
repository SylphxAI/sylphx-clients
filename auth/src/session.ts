import {
	type AuthCookie,
	type AuthFetch,
	json,
	pkceChallenge,
	random,
	token,
	transport,
} from './http.js'

/** Exchange a verified Identity session via the canonical Management OAuth bridge. */
export async function exchangeManagementSession(options: {
	request: Request
	apiOrigin: string
	clientId: string
	redirectUri: string
	session: string
	sessionCookie: string
	refreshCookie: string
	fetch?: AuthFetch
}): Promise<AuthCookie[] | undefined> {
	const { request, apiOrigin, clientId, redirectUri, session, sessionCookie, refreshCookie } =
		options
	const identity = transport(request, options.fetch ?? ((url, init) => fetch(url, init)), apiOrigin)
	const verifier = random()
	const state = random()
	const authorize = await identity('/v1/oauth/authorize', {
		bearer: session,
		body: {
			client_id: clientId,
			redirect_uri: redirectUri,
			response_type: 'code',
			scope: 'openid profile email',
			state,
			nonce: random(),
			code_challenge: await pkceChallenge(verifier),
			code_challenge_method: 'S256',
		},
	})
	const grant = await json(authorize)
	if (
		!authorize.ok ||
		grant.state !== state ||
		grant.consent_required !== false ||
		!token(grant.code)
	)
		return
	const exchange = await identity('/v1/oauth/token', {
		body: new URLSearchParams({
			grant_type: 'authorization_code',
			client_id: clientId,
			redirect_uri: redirectUri,
			code: grant.code,
			code_verifier: verifier,
		}),
	})
	const proof = await json(exchange)
	if (!exchange.ok || proof.token_type !== 'Bearer' || !token(proof.access_token)) return
	const management = await identity('/v1/auth/identity-exchange', {
		body: { identity_access_token: proof.access_token, client_id: clientId },
	})
	const data = await json(management)
	const {
		access_token: access,
		refresh_token: refresh,
		expires_in: accessAge,
		refresh_expires_at: expiry,
	} = data
	const refreshAge =
		typeof expiry === 'string' ? Math.floor((Date.parse(expiry) - Date.now()) / 1000) : NaN
	if (
		!management.ok ||
		data.token_type !== 'Bearer' ||
		!token(access) ||
		!token(refresh) ||
		typeof accessAge !== 'number' ||
		!Number.isSafeInteger(accessAge) ||
		accessAge < 1 ||
		!Number.isFinite(refreshAge) ||
		refreshAge < 1
	)
		return
	return [
		[sessionCookie, access, Math.min(3600, accessAge)],
		[refreshCookie, refresh, Math.min(30 * 86400, refreshAge)],
	]
}
