import { cookieName, token } from './http.js'

export type SessionCookieSource = { name: string; value: string; legacy: boolean }

/** Read a credential candidate, not an authenticated session. Always validate it with its issuer. */
export function readAuthSessionCookie(
	request: Request,
	options: {
		sessionCookie?: string
		legacyCookies?: readonly string[]
	} = {},
): SessionCookieSource | undefined {
	const canonical = cookieName(options.sessionCookie ?? '__Host-auth_session')
	const names = [canonical, ...(options.legacyCookies ?? []).map(cookieName)]
	const parts = (request.headers.get('cookie') ?? '').split(';').map((part) => part.trim())
	for (const name of names) {
		const matches = parts.filter((part) => part.startsWith(`${name}=`))
		if (!matches.length) continue
		const value = matches[0]?.slice(name.length + 1)
		// An invalid or ambiguous higher-priority cookie never falls back.
		return matches.length === 1 && token(value)
			? { name, value, legacy: name !== canonical }
			: undefined
	}
	return undefined
}
