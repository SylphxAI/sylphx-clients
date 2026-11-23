import {
	type EntryConfig,
	type Provider,
	providerDestination,
	providers,
	safeReturnTo,
} from '@sylphx/auth/client'
import { createSignal, onCleanup, onMount } from 'solid-js'

import { catalogs } from './messages.js'

const errors = catalogs.en.errors
export const unavailable = errors.unavailable
export type SignInErrorCode = keyof typeof errors

function isErrorCode(value: unknown): value is SignInErrorCode {
	return typeof value === 'string' && Object.hasOwn(errors, value)
}

class SignInError extends Error {
	constructor(readonly code: SignInErrorCode) {
		super(code)
	}
}

export type SignInStep =
	| 'choose'
	| 'password'
	| 'signup'
	| 'check_email'
	| 'confirm'
	| 'otp'
	| 'mfa'
type RequestResult = {
	authorize?: string
	next?: string
	step?: 'check_email' | 'mfa'
	flow?: string
	challenge?: string
	challengeId?: string
	rpId?: string
	credentials?: string[]
	error?: string
}

function decode(value: string) {
	return Uint8Array.from(atob(value.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0))
}
function encode(value: ArrayBuffer) {
	return btoa(String.fromCharCode(...new Uint8Array(value)))
		.replace(/\+/g, '-')
		.replace(/\//g, '_')
		.replace(/=+$/, '')
}

export type SignInControllerOptions = {
	/** A path already allowlisted by the application. */
	next: string
	/** The login URL's query string, read once at construction. */
	search: string
	/** The same-origin login pathname, used to scrub a confirmation link. */
	loginPath?: string
}

/** Create under a Solid owner. Requests start on mount and abort on cleanup. */
export function createSignInController(options: SignInControllerOptions) {
	const search = new URLSearchParams(options.search)
	const next = safeReturnTo(options.next)
	const loginPath = safeReturnTo(options.loginPath, '/login').split(/[?#]/, 1)[0]
	const challengeId = search.get('challenge_id')
	const verification = search.get('token')
	const hasVerification =
		!!challengeId && !!verification && search.get('type') === 'email_verification'
	const [config, setConfig] = createSignal<EntryConfig>()
	const [configFailed, setConfigFailed] = createSignal(false)
	const [step, setStep] = createSignal<SignInStep>('choose')
	const [email, setEmail] = createSignal('')
	const initialError = search.get('error')
	const [errorCode, setErrorCode] = createSignal<SignInErrorCode | ''>(
		isErrorCode(initialError) ? initialError : '',
	)
	const error = () => {
		const value = errorCode()
		return value ? errors[value] : ''
	}
	const [busy, setBusy] = createSignal('')
	const [showPassword, setShowPassword] = createSignal(false)
	const [canPasskey, setCanPasskey] = createSignal(false)
	const [code, setCode] = createSignal('')
	const [flow, setFlow] = createSignal('')
	const [challenge, setChallenge] = createSignal('')
	const [confirmAfterMfa, setConfirmAfterMfa] = createSignal(false)
	const [recovery, setRecovery] = createSignal(false)
	const [resendAt, setResendAt] = createSignal(0)
	const [now, setNow] = createSignal(0)
	const wait = () => Math.max(0, Math.ceil((resendAt() - now()) / 1000))
	let confController: AbortController | undefined
	const controller = new AbortController()

	async function loadConfig() {
		if (controller.signal.aborted) return
		setConfigFailed(false)
		setConfig(undefined)
		confController?.abort()
		const attempt = new AbortController()
		confController = attempt
		try {
			const res = await fetch('/auth/config', {
				referrerPolicy: 'origin',
				redirect: 'error',
				signal: AbortSignal.any([attempt.signal, AbortSignal.timeout(12_000)]),
			})
			if (!res.ok) throw new Error()
			const data = (await res.json()) as EntryConfig
			if (
				!data ||
				!Array.isArray(data.providers) ||
				!data.providers.every((id) => typeof id === 'string' && Object.hasOwn(providers, id)) ||
				[data.password, data.signup, data.otp, data.passkey].some(
					(value) => typeof value !== 'boolean',
				)
			)
				throw new Error()
			if (!attempt.signal.aborted) setConfig(data)
		} catch {
			if (!attempt.signal.aborted && !controller.signal.aborted) setConfigFailed(true)
		}
	}
	onMount(() => {
		setCanPasskey(window.isSecureContext && typeof PublicKeyCredential !== 'undefined')
		if (hasVerification) {
			setStep('confirm')
			// Keep the mail proof only in memory, never in an outbound URL.
			window.history.replaceState(
				window.history.state,
				'',
				`${loginPath}?${new URLSearchParams({ next })}`,
			)
		}
		void loadConfig()
		const timer = window.setInterval(() => setNow(Date.now()), 1000)
		onCleanup(() => {
			window.clearInterval(timer)
			confController?.abort()
			controller.abort()
		})
	})

	function go(to: SignInStep) {
		setErrorCode('')
		setShowPassword(false)
		setStep(to)
	}
	async function post(path: string, body: Record<string, unknown>): Promise<RequestResult> {
		const res = await fetch(path, {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			referrerPolicy: 'origin',
			redirect: 'error',
			signal: AbortSignal.any([controller.signal, AbortSignal.timeout(45_000)]),
			body: JSON.stringify(body),
		})
		const data: RequestResult = await res.json()
		if (!res.ok) throw new SignInError(isErrorCode(data.error) ? data.error : 'unavailable')
		return data
	}
	async function request(action: string, fields: Record<string, unknown> = {}) {
		const data = await post('/auth/email', { action, next, ...fields })
		if (!controller.signal.aborted && typeof data.next === 'string')
			window.location.assign(safeReturnTo(data.next, next))
		return data
	}
	async function run(label: string, op: () => Promise<void>) {
		if (busy() || controller.signal.aborted) return
		setBusy(label)
		setErrorCode('')
		try {
			await op()
		} catch (cause) {
			if (!controller.signal.aborted)
				setErrorCode(cause instanceof SignInError ? cause.code : 'unavailable')
		} finally {
			setBusy('')
		}
	}
	function social(provider: Provider) {
		if (!config()?.providers.includes(provider)) return
		return run(providers[provider][0], async () => {
			const { authorize } = await post(`/auth/start/${provider}`, { next })
			const target = providerDestination(provider, authorize)
			if (!target) throw new Error()
			if (!controller.signal.aborted) window.location.assign(target)
		})
	}
	function sendCode() {
		if (!config()?.otp || (step() === 'otp' && wait() > 0)) return
		return run('email', async () => {
			const data = await request('otp_start', { email: email().trim() })
			if (!data.flow) throw new Error()
			setFlow(data.flow)
			setCode('')
			const sentAt = Date.now()
			setNow(sentAt)
			setResendAt(sentAt + 30_000)
			go('otp')
		})
	}
	function verifyCode(value = code()) {
		if (step() !== 'otp' || busy() || !/^\d{6}$/.test(value)) return
		setCode(value)
		return run('verify', async () => {
			const data = await request('otp_verify', { flow: flow(), code: value })
			if (data.step === 'mfa') {
				setChallenge('')
				setRecovery(false)
				go('mfa')
			}
		})
	}
	function passkey() {
		if (!canPasskey() || !config()?.passkey) return
		return run('passkey', async () => {
			const { challenge, challengeId, rpId, credentials } = await request('passkey_start')
			if (!challenge || !challengeId || !rpId || !credentials) throw new Error()
			const assertion = await navigator.credentials
				.get({
					signal: controller.signal,
					publicKey: {
						challenge: decode(challenge),
						rpId,
						timeout: 60_000,
						userVerification: 'required',
						allowCredentials: credentials.map((id) => ({ id: decode(id), type: 'public-key' })),
					},
				})
				.catch((error: unknown) => {
					if (error instanceof DOMException && error.name === 'NotAllowedError') return null
					throw error
				})
			if (
				!(assertion instanceof PublicKeyCredential) ||
				!(assertion.response instanceof AuthenticatorAssertionResponse)
			)
				throw new SignInError('method_unavailable')
			const response = assertion.response
			await request('passkey_verify', {
				challenge_id: challengeId,
				credential_id: encode(assertion.rawId),
				client_data_json: encode(response.clientDataJSON),
				authenticator_data: encode(response.authenticatorData),
				signature: encode(response.signature),
				user_handle: response.userHandle ? encode(response.userHandle) : '',
			})
		})
	}

	function continueWithEmail() {
		if (config()?.otp) return sendCode()
		if (config()?.password && !busy()) go('password')
		return undefined
	}
	function submitPassword(password: string) {
		const mode = step()
		if (mode !== 'password' && mode !== 'signup' && mode !== 'confirm') return
		if (mode === 'confirm' && !hasVerification) return
		return run('password', async () => {
			const data = await request(mode === 'confirm' ? 'verify_signup' : mode, {
				email: email().trim(),
				password,
				...(mode === 'confirm' ? { challengeId, verification } : {}),
			})
			if (data.step === 'check_email') go('check_email')
			if (data.step === 'mfa' && data.challenge) {
				setChallenge(data.challenge)
				setConfirmAfterMfa(mode === 'confirm')
				setRecovery(false)
				go('mfa')
			}
		})
	}
	function submitMfa(value: string) {
		if (step() !== 'mfa') return
		const factor = value.trim()
		const method = recovery() ? 'recovery_code' : 'totp'
		return run('verify', async () => {
			if (challenge())
				await request('mfa', {
					challenge: challenge(),
					code: factor,
					method,
					...(confirmAfterMfa() ? { challengeId, verification } : {}),
				})
			else await request('otp_verify', { flow: flow(), code: code(), factor, method })
		})
	}

	return {
		config,
		configFailed,
		loadConfig,
		step,
		go,
		email,
		setEmail,
		error,
		errorCode,
		busy,
		showPassword,
		setShowPassword,
		canPasskey,
		code,
		setCode,
		recovery,
		setRecovery,
		wait,
		continueWithEmail,
		social,
		sendCode,
		verifyCode,
		passkey,
		submitPassword,
		submitMfa,
	}
}
