import { afterEach, expect, mock, spyOn, test } from 'bun:test'
import type { EntryConfig } from '@sylphx/auth/client'
import { createRoot } from 'solid-js'
import { createSignInController, unavailable } from './sign-in-controller.js'

const config: EntryConfig = {
	providers: ['google'],
	password: true,
	signup: true,
	otp: true,
	passkey: false,
}
const email = 'member@example.invalid'
const next = '/account'

function controller(search = '') {
	return createRoot((dispose) => ({ ...createSignInController({ next, search }), dispose }))
}

afterEach(() => mock.restore())

test('only displays known callback errors and uses no browser globals during SSR', () => {
	const noError = controller('?error=constructor')
	const cancelled = controller('?error=cancelled')
	expect(noError.error()).toBe('')
	expect(noError.errorCode()).toBe('')
	expect(cancelled.errorCode()).toBe('cancelled')
	expect(cancelled.error()).toBe('Sign-in was canceled. You can try again whenever you’re ready.')
	expect(noError.step()).toBe('choose')
	noError.dispose()
	cancelled.dispose()
})

test('does not render unknown provider config and can retry a failed config request', async () => {
	const fetch = spyOn(globalThis, 'fetch')
		.mockResolvedValueOnce(Response.json({ ...config, providers: ['unknown'] }))
		.mockResolvedValueOnce(Response.json(config))
	const auth = controller()
	await auth.loadConfig()
	expect(auth.configFailed()).toBe(true)
	expect(auth.config()).toBeUndefined()
	await auth.loadConfig()
	expect(auth.configFailed()).toBe(false)
	expect(auth.config()).toEqual(config)
	expect(fetch.mock.calls.map(([url]) => url)).toEqual(['/auth/config', '/auth/config'])
	auth.dispose()
})

test('starts OTP, enforces resend delay, and submits MFA against the original flow and code', async () => {
	const flow = 'fixture-flow'
	const fetch = spyOn(globalThis, 'fetch')
		.mockResolvedValueOnce(Response.json(config))
		.mockResolvedValueOnce(Response.json({ flow }))
		.mockResolvedValueOnce(Response.json({ step: 'mfa' }))
		.mockResolvedValueOnce(Response.json({ error: 'invalid_code' }, { status: 400 }))
	const auth = controller()
	await auth.loadConfig()
	auth.setEmail(` ${email} `)
	await auth.continueWithEmail()
	expect(auth.wait()).toBe(30)
	await auth.sendCode()
	await auth.verifyCode('123')
	expect(fetch).toHaveBeenCalledTimes(2)
	await auth.verifyCode('123456')
	expect(auth.step()).toBe('mfa')
	auth.setRecovery(true)
	await auth.submitMfa(' fixture-factor ')
	expect(auth.error()).toBe('That code didn’t work. Check it and try again.')
	expect(auth.errorCode()).toBe('invalid_code')
	expect(
		fetch.mock.calls.slice(1).map(([url, init]) => [url, JSON.parse(String(init?.body))]),
	).toEqual([
		['/auth/email', { action: 'otp_start', email, next }],
		['/auth/email', { action: 'otp_verify', code: '123456', flow, next }],
		[
			'/auth/email',
			{
				action: 'otp_verify',
				code: '123456',
				flow,
				factor: 'fixture-factor',
				method: 'recovery_code',
				next,
			},
		],
	])
	auth.dispose()
})

test('uses the provider-specific start route with next only and refuses other destinations', async () => {
	const fetch = spyOn(globalThis, 'fetch')
		.mockResolvedValueOnce(Response.json(config))
		.mockResolvedValueOnce(Response.json({ authorize: 'https://example.invalid/authorize' }))
	const auth = controller()
	await auth.loadConfig()
	await auth.social('github')
	expect(fetch).toHaveBeenCalledTimes(1)
	await auth.social('google')
	const [url, init] = fetch.mock.calls[1]!
	expect(url).toBe('/auth/start/google')
	expect(init?.method).toBe('POST')
	expect(init?.redirect).toBe('error')
	expect(JSON.parse(String(init?.body))).toEqual({ next })
	expect(auth.error()).toBe(unavailable)
	auth.dispose()
})

test('a server seed is immediately available and clickwrap travels on prefixed requests', async () => {
	const fetch = spyOn(globalThis, 'fetch')
		.mockResolvedValueOnce(Response.json({ authorize: 'https://example.invalid/authorize' }))
		.mockResolvedValueOnce(Response.json({ flow: 'fixture-flow' }))
	const value = { terms: 'v2', privacy: '2026-11-24' }
	const auth = createRoot((dispose) => ({
		...createSignInController({
			next,
			search: '',
			initialConfig: config,
			routePrefix: '/account/auth',
			getAcceptance: () => value,
		}),
		dispose,
	}))
	expect(auth.config()).toEqual(config)
	expect(fetch).not.toHaveBeenCalled()
	await auth.social('google')
	auth.setEmail(email)
	await auth.continueWithEmail()
	expect(fetch.mock.calls.map(([url, init]) => [url, JSON.parse(String(init?.body))])).toEqual([
		['/account/auth/start/google', { next, acceptance: value }],
		['/account/auth/email', { next, action: 'otp_start', email, acceptance: value }],
	])
	auth.dispose()
})

test('gates auth actions without agreement and admits the same action once accepted', async () => {
	let accepted = false
	const fetch = spyOn(globalThis, 'fetch').mockResolvedValueOnce(Response.json({ flow: 'fixture' }))
	const auth = createRoot((dispose) => ({
		...createSignInController({
			next,
			search: '',
			initialConfig: config,
			requireAgreement: true,
			get agreementAccepted() {
				return accepted
			},
		}),
		dispose,
	}))
	auth.setEmail(email)
	await auth.continueWithEmail()
	await auth.social('google')
	expect(fetch).not.toHaveBeenCalled()
	expect(auth.errorCode()).toBe('acceptance_required')
	accepted = true
	await auth.continueWithEmail()
	expect(auth.step()).toBe('otp')
	expect(fetch).toHaveBeenCalledTimes(1)
	auth.dispose()
})

test('password sign-in remains available before OTP and after OTP send returns 503', async () => {
	const fetch = spyOn(globalThis, 'fetch')
		.mockResolvedValueOnce(Response.json({ error: 'unavailable' }, { status: 503 }))
		.mockResolvedValueOnce(Response.json({ error: 'invalid_credentials' }, { status: 400 }))
	const auth = createRoot((dispose) => ({
		...createSignInController({ next, search: '', initialConfig: config }),
		dispose,
	}))
	auth.setEmail(email)
	await auth.continueWithEmail('password')
	expect(auth.step()).toBe('password')
	expect(fetch).not.toHaveBeenCalled()
	auth.go('choose')
	await auth.continueWithEmail()
	expect(auth.errorCode()).toBe('unavailable')
	expect(auth.step()).toBe('choose')
	await auth.continueWithEmail('password')
	await auth.submitPassword('fixture-only')
	expect(fetch.mock.calls.map(([, init]) => JSON.parse(String(init?.body)).action)).toEqual([
		'otp_start',
		'password',
	])
	expect(auth.errorCode()).toBe('invalid_credentials')
	auth.dispose()
})

test('recovery submit only dispatches from the enabled recovery step', async () => {
	const fetch = spyOn(globalThis, 'fetch').mockResolvedValueOnce(
		Response.json({ step: 'recovery_sent' }, { status: 202 }),
	)
	const auth = createRoot((dispose) => ({
		...createSignInController({ next, search: '', initialConfig: { ...config, recovery: true } }),
		dispose,
	}))
	await auth.sendRecovery()
	expect(fetch).not.toHaveBeenCalled()
	auth.go('recover')
	auth.setEmail(email)
	await auth.sendRecovery()
	expect(auth.step()).toBe('recovery_sent')
	expect(JSON.parse(String(fetch.mock.calls[0]?.[1]?.body))).toEqual({
		action: 'recovery_start',
		email,
		next,
	})
	auth.dispose()
})
