export type Data = Record<string, unknown>
export type AuthCookie = readonly [name: string, value: string, seconds: number]
export type AuthFetch = (input: URL, init: RequestInit) => Promise<Response>
export const RANDOM = /^[A-Za-z0-9_-]{43}$/
const TOKEN = /^[A-Za-z0-9._~-]{1,3500}$/

export function obj(value: unknown): Data {
	return value && typeof value === 'object' && !Array.isArray(value) ? (value as Data) : {}
}
export function token(value: unknown): value is string {
	return typeof value === 'string' && TOKEN.test(value)
}
export function field(data: Data, key: string, max: number) {
	const value = data[key]
	return typeof value === 'string' && value.length > 0 && value.length <= max ? value : undefined
}
export function headers(cookies: readonly AuthCookie[] = []) {
	const out = new Headers({
		'Cache-Control': 'private, no-store',
		'Referrer-Policy': 'no-referrer',
		'X-Content-Type-Options': 'nosniff',
	})
	for (const [name, value, seconds] of cookies) {
		out.append(
			'Set-Cookie',
			`${name}=${value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=${seconds}`,
		)
	}
	return out
}
export function reply(data: unknown, status = 200, cookies?: readonly AuthCookie[]) {
	return Response.json(data, { status, headers: headers(cookies) })
}
export function problem(error: string, status: number) {
	return reply({ error }, status)
}
export function redirect(location: string, cookies: readonly AuthCookie[]) {
	const out = headers(cookies)
	out.set('Location', location)
	return new Response(null, { status: 303, headers: out })
}
export async function json(response: Response) {
	if (!/^application\/(?:problem\+)?json\b/i.test(response.headers.get('content-type') ?? '')) {
		discard(response)
		throw new Error('Unexpected authentication response')
	}
	return obj(await response.json())
}
export function discard(response: Response) {
	void response.body?.cancel().catch(() => undefined)
}
export function readCookie(request: Request, name: string) {
	const candidates = (request.headers.get('cookie') ?? '')
		.split(';')
		.map((part) => part.trim())
		.filter((part) => part.startsWith(`${name}=`))
	return candidates.length === 1 ? candidates[0]?.slice(name.length + 1) : undefined
}
export function cookieName(value: string) {
	if (!/^[A-Za-z0-9_-]{1,128}$/.test(value)) throw new TypeError('Invalid cookie name')
	return value
}
export function random() {
	return base64url(crypto.getRandomValues(new Uint8Array(32)))
}
function base64url(bytes: Uint8Array) {
	return btoa(String.fromCharCode(...bytes))
		.replace(/\+/g, '-')
		.replace(/\//g, '_')
		.replace(/=+$/, '')
}
export async function pkceChallenge(verifier: string) {
	return base64url(
		new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier))),
	)
}

export function transport(request: Request, fetchUpstream: AuthFetch, api: string) {
	return (
		path: string | URL,
		options: {
			query?: Record<string, string>
			body?: Data | URLSearchParams
			bearer?: string
			method?: 'GET' | 'POST'
			manual?: boolean
		} = {},
	) => {
		const url = new URL(path, api)
		for (const [key, value] of Object.entries(options.query ?? {})) url.searchParams.set(key, value)
		const out = new Headers({ Accept: 'application/json' })
		const ua = request.headers.get('user-agent')
		if (ua) out.set('User-Agent', ua)
		if (options.bearer) out.set('Authorization', `Bearer ${options.bearer}`)
		const body =
			options.body instanceof URLSearchParams
				? options.body
				: options.body && JSON.stringify(options.body)
		if (typeof body === 'string') out.set('Content-Type', 'application/json')
		return fetchUpstream(url, {
			method: options.method ?? (options.body ? 'POST' : 'GET'),
			headers: out,
			body: body ?? null,
			credentials: 'omit',
			redirect: options.manual ? 'manual' : 'error',
			cache: 'no-store',
			signal: AbortSignal.any([request.signal, AbortSignal.timeout(10_000)]),
		})
	}
}
export type Identity = ReturnType<typeof transport>

const MAX_JSON = 32_768
export async function jsonBody(request: Request) {
	const reader = request.body?.getReader()
	if (!reader) throw new Error('Missing JSON body')
	const decoder = new TextDecoder('utf-8', { fatal: true })
	let body = ''
	let bytes = 0
	try {
		while (true) {
			const { done, value } = await reader.read()
			if (done) break
			bytes += value.byteLength
			if (bytes > MAX_JSON) throw new Error('JSON body too large')
			body += decoder.decode(value, { stream: true })
		}
		body += decoder.decode()
		const parsed: unknown = JSON.parse(body)
		if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed))
			throw new Error('Invalid JSON')
		return parsed as Record<string, unknown>
	} finally {
		void reader.cancel().catch(() => undefined)
		reader.releaseLock()
	}
}
