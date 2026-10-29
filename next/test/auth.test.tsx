import { describe, expect, test } from 'bun:test'
import { SignIn } from '@sylphx/next/auth'
import { createContinueMock } from '@sylphx/sdk/auth/continue-mock'
import { renderToStaticMarkup } from 'react-dom/server'
import { type AuthRuntime, createSignInActions, type SignInState } from '../src/auth-actions.js'
import { SignInForm } from '../src/auth-client.js'

const options = {
	baseUrl: 'https://auth.example.com',
	publishableKey: 'sylphx_pk_test_example',
	secretKey: 'sylphx_sk_test_server_only',
	returnUrl: 'https://shop.example.com/sign-in',
}
const config = {
	instance: { id: 'shop', name: 'Shop', environment: 'test' },
	methods: { password: true, email_otp: true },
	social: ['google', 'apple'].map((provider) => ({
		provider,
		name: provider,
		credentials: 'custom',
		startUrl: `https://auth.example.com/v1/client/oauth/${provider}/start`,
	})),
	branding: {
		name: 'Customer Shop',
		logoUrl: 'https://shop.example.com/logo.png',
		primaryColor: '#123456',
	},
	localization: { defaultLocale: 'en', locales: ['en'] },
	apiOrigin: options.baseUrl,
	ticketParam: 'auth_ticket',
	errorParam: 'auth_error',
}
const wireConfig = {
	...config,
	branding: {
		name: config.branding.name,
		logo_url: config.branding.logoUrl,
		primary_color: config.branding.primaryColor,
	},
	social: config.social.map((p) => ({ ...p, start_url: p.startUrl })),
	api_origin: config.apiOrigin,
	ticket_param: config.ticketParam,
	error_param: config.errorParam,
	localization: { default_locale: 'en', locales: ['en'] },
}
const form = (values: Record<string, string | undefined>) => {
	const f = new FormData()
	for (const [k, v] of Object.entries(values)) {
		if (v !== undefined) f.set(k, v)
	}
	return f
}
const initial: SignInState = { step: 'identifier' }
function harness() {
	const mock = createContinueMock({
		publishableKey: options.publishableKey,
		accounts: [{ email: 'password@example.com', password: true }],
	})
	const requests: Request[] = []
	const cookies: { name: string; value: string; options: unknown }[] = []
	const tickets = new Set<string>()
	const fetcher: NonNullable<AuthRuntime['fetch']> = async (input, init) => {
		const req = new Request(input, init)
		requests.push(req.clone())
		const path = new URL(req.url).pathname
		if (path === '/v1/client/config') return Response.json(wireConfig)
		if (path === '/v1/client/tickets:redeem') {
			const { ticket } = (await req.json()) as { ticket: string }
			if (tickets.has(ticket) || ticket === 'invalid') return Response.json({}, { status: 401 })
			tickets.add(ticket)
			expect(req.headers.get('authorization')).toBe(`Bearer ${options.secretKey}`)
			expect(req.headers.get('user-agent')).toBe('Browser under test')
			return Response.json({
				session: {
					token: 'private-session-bearer',
					expires_at: Math.floor(Date.now() / 1000) + 3600,
				},
			})
		}
		if (path === '/v1/client/sign-in/password') {
			expect(((await req.json()) as any).session_mode).toBe('server')
			return Response.json({ ticket: 'password-ticket' })
		}
		return mock.fetch(req.url, init)
	}
	const runtime: AuthRuntime = {
		userAgent: 'Browser under test',
		fetch: fetcher,
		setCookie: (name, value, options) => {
			cookies.push({ name, value, options })
		},
		redirect: (url) => {
			throw new Error(`REDIRECT:${url}`)
		},
	}
	return { ...mock, actions: createSignInActions(options, runtime), requests, cookies }
}
const html = (state: SignInState = initial, overrides = config) =>
	renderToStaticMarkup(
		<SignInForm
			config={overrides}
			action={async () => state}
			socialAction={async () => {}}
			initialState={state}
			appearance={{
				variables: {
					colorPrimary: '#abcdef',
					colorText: '#112233',
					colorBackground: '#fafafa',
					colorDanger: '#ff0000',
					borderRadius: '12px',
					fontFamily: 'Georgia',
				},
				elements: {
					root: 'customer-root',
					button: 'customer-button',
					input: 'customer-input',
					socialButton: 'customer-social',
				},
			}}
		/>,
	)

describe('public Next auth', () => {
	test('public export exists; identifier-first markup has customer brand and configured providers, accessible appearance', () => {
		expect(typeof SignIn).toBe('function')
		const markup = html()
		for (const value of [
			'Customer Shop',
			'Continue with Google',
			'Continue with Apple',
			'username webauthn',
			'customer-root',
			'customer-input',
			'customer-button',
			'customer-social',
			'#abcdef',
			'#112233',
			'#fafafa',
			'#ff0000',
			'12px',
			'Georgia',
			'role="alert"',
			'aria-live="assertive"',
			'<label',
		])
			expect(markup).toContain(value)
		expect(markup).not.toContain('name="password"')
		expect(markup).not.toMatch(/sylphx|sylphx\.[a-z]+/i)
		expect(markup.indexOf('name="email"')).toBeLessThan(markup.indexOf('>Continue</button>'))
	})
	test('disabled methods and unconfigured providers do not render; platform branding does not leak', () => {
		const markup = html(initial, {
			...config,
			methods: { password: false, email_otp: false },
			social: [{ ...config.social[0]!, credentials: 'missing' }],
			branding: { ...config.branding, name: 'Sylphx', logoUrl: 'https://auth.sylphx.com/logo.png' },
		})
		expect(markup).not.toMatch(/sylphx|name="email"|Continue with/)
	})
	test('Continue replays generated contract mock and code completion redeems on server only', async () => {
		const h = harness()
		const state = await h.actions.submit(
			initial,
			form({ intent: 'continue', email: 'new@example.com' }),
		)
		expect(state.step).toBe('email_otp')
		if (state.step !== 'email_otp') throw new Error('Expected code step')
		expect(html(state)).toContain('autoComplete="one-time-code"')
		expect(html(state)).toContain(`value="${state.flow}"`)
		const continueReq = h.requests.find((r) => new URL(r.url).pathname === '/v1/client/continue')!
		expect(await continueReq.json()).toEqual({ email: 'new@example.com', session_mode: 'server' })
		const result = await h.actions.submit(
			state,
			form({ intent: 'code', flow: state.flow, code: h.outbox[0]!.code }),
		)
		expect(result).toEqual({ step: 'complete' })
		expect(JSON.stringify(result)).not.toContain('private-session-bearer')
		expect(h.cookies).toHaveLength(1)
		expect(h.cookies[0]).toMatchObject({
			name: '__Host-session',
			value: 'private-session-bearer',
			options: { httpOnly: true, secure: true, sameSite: 'lax', path: '/' },
		})
		expect(h.cookies[0]!.options).not.toHaveProperty('domain')
		const replay = await h.actions.submit(
			state,
			form({ intent: 'code', flow: state.flow, code: h.outbox[0]!.code }),
		)
		expect(replay.step).toBe('error')
		expect(h.cookies).toHaveLength(1)
	})
	test('password step and explicit email fallback are returned and rendered from Continue', async () => {
		const h = harness()
		const state = await h.actions.submit(
			initial,
			form({ intent: 'continue', email: 'password@example.com' }),
		)
		expect(state.step).toBe('password')
		expect(html(state)).toContain('autoComplete="current-password"')
		expect(html(state)).toContain('Use an email code')
		expect(
			(await h.actions.submit(state, form({ intent: 'email_otp', email: 'password@example.com' })))
				.step,
		).toBe('email_otp')
		expect(
			await h.actions.submit(
				state,
				form({ intent: 'password', email: 'password@example.com', password: 'example' }),
			),
		).toEqual({ step: 'complete' })
	})
	for (const provider of ['google', 'apple'])
		test(`${provider} starts server mode and its return uses shared secret redemption`, async () => {
			const h = harness()
			let destination = ''
			try {
				await h.actions.social(form({ provider }))
			} catch (e) {
				destination = (e as Error).message.replace('REDIRECT:', '')
			}
			const url = new URL(destination)
			expect(url.pathname).toBe(`/v1/client/oauth/${provider}/start`)
			expect(url.searchParams.get('redirect_url')).toBe(options.returnUrl)
			expect(url.searchParams.get('session_mode')).toBe('server')
			const result = await h.actions.submit(
				initial,
				form({ intent: 'ticket', ticket: `${provider}-return-ticket` }),
			)
			expect(result).toEqual({ step: 'complete' })
			expect(h.cookies).toHaveLength(1)
			expect(JSON.stringify(result)).not.toContain('private-session-bearer')
			expect(
				(
					await h.actions.submit(
						initial,
						form({ intent: 'ticket', ticket: `${provider}-return-ticket` }),
					)
				).step,
			).toBe('error')
		})
	test('bad code, invalid ticket and unknown intent fail closed, without cookies or upstream errors', async () => {
		const h = harness()
		for (const input of [
			{ intent: 'code', flow: 'missing', code: '000000' },
			{ intent: 'ticket', ticket: 'invalid' },
			{ intent: 'unknown', email: 'a@b.com' },
		]) {
			const result = await h.actions.submit(initial, form(input))
			expect(result).toEqual({ step: 'error', message: 'Unable to sign in. Please try again.' })
		}
		expect(h.cookies).toHaveLength(0)
		await expect(h.actions.social(form({ provider: 'unknown' }))).rejects.toThrow('unavailable')
	})
})
