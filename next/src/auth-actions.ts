import { type AuthConfig, type AuthContinue, createAuthClient } from '@sylphx/sdk/auth/client'

export interface AuthOptions {
	/** Auth API origin. Never inferred from incoming request headers. */
	baseUrl: string
	publishableKey: string
	/** Server-only secret: never pass this options object to a Client Component. */
	secretKey: string
	/** Absolute, allowlisted app URL rendering SignIn after OAuth. */
	returnUrl: string
}
export type SignInState =
	| { step: 'identifier' }
	| ({ email: string } & AuthContinue)
	| { step: 'complete' }
	| { step: 'error'; message: string }
export type SignInAction = (previous: SignInState, form: FormData) => Promise<SignInState>
export interface AuthRuntime {
	userAgent: string
	fetch?: (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>
	setCookie(
		name: string,
		value: string,
		options: {
			httpOnly: true
			secure: true
			sameSite: 'lax'
			path: '/'
			expires: Date
		},
	): void | Promise<void>
	redirect(url: string): never
}

/** Server-side implementation; callers wrap this with a Next server action. */
export function createSignInActions(options: AuthOptions, runtime: AuthRuntime) {
	const base = new URL(options.baseUrl)
	const callback = new URL(options.returnUrl)
	if (base.protocol !== 'https:' || callback.protocol !== 'https:')
		throw new Error('Auth requires HTTPS origins')
	const client = createAuthClient({
		baseUrl: base.href.replace(/\/$/, ''),
		publishableKey: options.publishableKey,
		userAgent: runtime.userAgent,
		fetch: (input, init) => (runtime.fetch ?? fetch)(input, { ...init, cache: 'no-store' }),
	})
	const request = runtime.fetch ?? fetch
	async function post(
		path: string,
		body: unknown,
		secret = false,
	): Promise<{ ticket?: unknown; session?: { token?: unknown; expires_at?: unknown } }> {
		const url = new URL(path, base)
		if (!secret) url.searchParams.set('publishable_key', options.publishableKey)
		const response = await request(url, {
			method: 'POST',
			cache: 'no-store',
			headers: {
				'content-type': 'application/json',
				'user-agent': runtime.userAgent,
				...(secret ? { authorization: `Bearer ${options.secretKey}` } : {}),
			},
			body: JSON.stringify(body),
		})
		if (!response.ok) throw new Error('Sign-in failed')
		return response.json()
	}
	async function redeem(ticket: unknown): Promise<SignInState> {
		if (typeof ticket !== 'string' || !ticket || ticket.length > 8192)
			throw new Error('Invalid ticket')
		const result = await post('/v1/client/tickets:redeem', { ticket }, true)
		const session = result.session
		if (
			typeof session?.token !== 'string' ||
			!session.token ||
			typeof session.expires_at !== 'number' ||
			!Number.isFinite(session.expires_at) ||
			session.expires_at <= Date.now() / 1000
		)
			throw new Error('Invalid session')
		await runtime.setCookie('__Host-session', session.token, {
			httpOnly: true,
			secure: true,
			sameSite: 'lax',
			path: '/',
			expires: new Date(session.expires_at * 1000),
		})
		return { step: 'complete' }
	}
	const text = (form: FormData, key: string) => {
		const value = form.get(key)
		if (typeof value !== 'string' || !value || value.length > 8192) throw new Error('Invalid input')
		return value
	}
	return {
		config: (): Promise<AuthConfig> => client.config(),
		async submit(_previous: SignInState, form: FormData): Promise<SignInState> {
			try {
				if (!runtime.userAgent) throw new Error('Missing browser identity')
				const intent = text(form, 'intent')
				if (intent === 'ticket') return await redeem(text(form, 'ticket'))
				const config = await client.config()
				if (intent === 'code') {
					if (!config.methods.email_otp) throw new Error('Method disabled')
					const result = await client.continueVerify({
						flow: text(form, 'flow'),
						code: text(form, 'code'),
					})
					if (!('ticket' in result)) throw new Error('Expected server ticket')
					return await redeem(result.ticket)
				}
				const email = text(form, 'email').trim()
				if (intent === 'password') {
					if (!config.methods.password) throw new Error('Method disabled')
					return await redeem(
						(
							await post('/v1/client/sign-in/password', {
								email,
								password: text(form, 'password'),
								session_mode: 'server',
							})
						).ticket,
					)
				}
				if (intent !== 'continue' && intent !== 'email_otp') throw new Error('Invalid intent')
				if (!config.methods.password && !config.methods.email_otp)
					throw new Error('Method disabled')
				if (intent === 'email_otp' && !config.methods.email_otp) throw new Error('Method disabled')
				return {
					...(await client.continue({
						email,
						sessionMode: 'server',
						...(intent === 'email_otp' ? { method: 'email_otp' as const } : {}),
					})),
					email,
				}
			} catch {
				// Never serialize upstream errors: they may contain provider URLs or secrets.
				return { step: 'error', message: 'Unable to sign in. Please try again.' }
			}
		},
		async social(form: FormData): Promise<void> {
			const provider = text(form, 'provider')
			const config = await client.config()
			if (
				!['google', 'apple'].includes(provider) ||
				!config.social.some(
					(p) => p.provider === provider && ['custom', 'shared'].includes(p.credentials),
				)
			) {
				throw new Error('Sign-in method unavailable')
			}
			// Construct from the trusted API origin, never from user input or config.startUrl.
			const url = new URL(`/v1/client/oauth/${provider}/start`, base)
			url.searchParams.set('publishable_key', options.publishableKey)
			url.searchParams.set('redirect_url', callback.href)
			url.searchParams.set('session_mode', 'server')
			runtime.redirect(url.href)
		},
	}
}
