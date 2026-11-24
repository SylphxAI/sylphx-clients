export const providers = {
	google: ['Google', 'https://accounts.google.com/o/oauth2/v2/auth'],
	github: ['GitHub', 'https://github.com/login/oauth/authorize'],
	apple: ['Apple', 'https://appleid.apple.com/auth/authorize'],
	facebook: ['Facebook', 'https://www.facebook.com/v25.0/dialog/oauth'],
	microsoft: ['Microsoft', 'https://login.microsoftonline.com/common/oauth2/v2.0/authorize'],
} as const

export type Provider = keyof typeof providers
export type AuthMethod = 'oauth' | 'password' | 'otp' | 'passkey'
export type EntryConfig = {
	providers: Provider[]
	password: boolean
	signup: boolean
	otp: boolean
	passkey: boolean
	recovery?: boolean
	passkeyEnrollment?: boolean
}
export function isProvider(id: unknown): id is Provider {
	return typeof id === 'string' && Object.hasOwn(providers, id)
}
export function providerDestination(id: Provider, location: unknown): string | undefined {
	if (typeof location !== 'string') return
	try {
		const url = new URL(location)
		if (
			url.origin + url.pathname === providers[id][1] &&
			!url.username &&
			!url.password &&
			!url.hash
		)
			return url.href
	} catch {
		// Invalid authorization location.
	}
	return undefined
}
function relativePath(value: unknown): string | undefined {
	if (
		typeof value !== 'string' ||
		value.length > 1024 ||
		!value.startsWith('/') ||
		value.startsWith('//') ||
		value.includes('\\') ||
		Array.from(value).some((c) => c.charCodeAt(0) <= 32 || c.charCodeAt(0) === 127) ||
		/%(?:00|0a|0d|2f|5c)/i.test(value.split(/[?#]/, 1)[0]!)
	)
		return undefined
	const url = new URL(value, 'https://return.invalid')
	if (url.origin !== 'https://return.invalid' || url.pathname.startsWith('//')) return undefined
	return url.pathname + url.search + url.hash
}
/** Never return an absolute or protocol-relative URL, even after normalization. */
export function safeReturnTo(value: unknown, fallback = '/'): string {
	return relativePath(value) ?? relativePath(fallback) ?? '/'
}

/** Resolve the canonical same-origin mount used by the handler and widget. */
export function authRoutePrefix(value = '/auth'): string {
	if (!/^\/(?:[A-Za-z0-9_-]+\/)*[A-Za-z0-9_-]+$/.test(value))
		throw new TypeError('Expected a canonical Auth route prefix')
	return value
}
