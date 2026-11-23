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
