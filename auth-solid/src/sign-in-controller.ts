import {
	authRoutePrefix,
	type EntryConfig,
	type Provider,
	providerDestination,
	providers,
	safeReturnTo,
} from '@sylphx/auth/client'
import { createEffect, createSignal, onCleanup, onMount } from 'solid-js'

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
	/** Public methods loaded by the host on the server, reused on hydration. */
	initialConfig?: EntryConfig
	/** Same-origin pathname, defaults to /auth. */
	routePrefix?: string
	/** Start a discoverable WebAuthn request once on the sign-in screen. */
	conditionalPasskey?: boolean
	/** Return only versions the user has accepted; undefined means no acceptance. */
	getAcceptance?: () => Readonly<Record<string, string>> | undefined
}

/** Create under a Solid owner. Requests start on mount and abort on cleanup. */
export function createSignInController(options: SignInControllerOptions) {
	const search = new URLSearchParams(options.search)
	const next = safeReturnTo(options.next)
	const route = authRoutePrefix(options.routePrefix)
	const loginPath = safeReturnTo(options.loginPath, '/login').split(/[?#]/, 1)[0]
	const challengeId = search.get('challenge_id')
	const verification = search.get('token')
	const hasVerification =
		!!challengeId && !!verification && search.get('type') === 'email_verification'
	const [config, setConfig] = createSignal<EntryConfig | undefined>(options.initialConfig)
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
	const [conditionalReady, setConditionalReady] = createSignal(false)
	let conditional: AbortController | undefined
	let attemptedConditional = false
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
			const res = await fetch(`${route}/config`, {
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
		if (!config()) void loadConfig()
		if (options.conditionalPasskey && canPasskey()) {
			void PublicKeyCredential.isConditionalMediationAvailable?.()
				.then((supported) => {
					if (!controller.signal.aborted) setConditionalReady(supported)
				})
				.catch(() => undefined)
		}
		const timer = window.setInterval(() => setNow(Date.now()), 1000)
		onCleanup(() => {
			window.clearInterval(timer)
			confController?.abort()
			stopConditional()
			controller.abort()
		})
	})

	function stopConditional() {
		conditional?.abort()
		conditional = undefined
	}

	function go(to: SignInStep) {
		if (to !== 'choose') stopConditional()
		setErrorCode('')
		setShowPassword(false)
		setStep(to)
	}
	async function post(
		path: string,
		body: Record<string, unknown>,
		signal = controller.signal,
	): Promise<RequestResult> {
		const res = await fetch(path, {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			referrerPolicy: 'origin',
			redirect: 'error',
			signal: AbortSignal.any([controller.signal, signal, AbortSignal.timeout(45_000)]),
			body: JSON.stringify({ ...body, acceptance: options.getAcceptance?.() }),
		})
		const data: RequestResult = await res.json()
		if (!res.ok) throw new SignInError(isErrorCode(data.error) ? data.error : 'unavailable')
		return data
	}
	async function request(action: string, fields: Record<string, unknown> = {}) {
		const data = await post(`${route}/email`, { action, next, ...fields })
		if (!controller.signal.aborted && typeof data.next === 'string')
			window.location.assign(safeReturnTo(data.next, next))
		return data
	}
	async function run(label: string, op: () => Promise<void>) {
		if (busy() || controller.signal.aborted) return
		attemptedConditional = true
		stopConditional()
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
			const { authorize } = await post(`${route}/start/${provider}`, { next })
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
	async function getPasskey(signal: AbortSignal, autofill: boolean) {
		const { challenge, challengeId, rpId, credentials } = await post(
			`${route}/email`,
			{ action: 'passkey_start', next },
			signal,
		)
		if (signal.aborted || !challenge || !challengeId || !rpId || !credentials) return
		// Conditional mediation is for discoverable credentials only. Do not
		// silently discard an upstream credential restriction.
		if (autofill && credentials.length) return
		const assertion = await navigator.credentials.get({
			signal,
			...(autofill && { mediation: 'conditional' as const }),
			publicKey: {
				challenge: decode(challenge),
				rpId,
				...(autofill ? {} : { timeout: 60_000 }),
				userVerification: 'required',
				allowCredentials: credentials.map((id) => ({ id: decode(id), type: 'public-key' })),
			},
		})
		if (
			signal.aborted ||
			!(assertion instanceof PublicKeyCredential) ||
			!(assertion.response instanceof AuthenticatorAssertionResponse)
		)
			return
		const response = assertion.response
		return {
			challenge_id: challengeId,
			credential_id: encode(assertion.rawId),
			client_data_json: encode(response.clientDataJSON),
			authenticator_data: encode(response.authenticatorData),
			signature: encode(response.signature),
			user_handle: response.userHandle ? encode(response.userHandle) : '',
		}
	}

	function passkey() {
		if (!canPasskey() || !config()?.passkey) return
		return run('passkey', async () => {
			const fields = await getPasskey(controller.signal, false).catch((error: unknown) => {
				if (error instanceof DOMException && error.name === 'NotAllowedError') return
				throw error
			})
			if (!fields) throw new SignInError('method_unavailable')
			await request('passkey_verify', fields)
		})
	}

	createEffect(() => {
		if (attemptedConditional || !conditionalReady() || step() !== 'choose' || !config()?.passkey)
			return
		// If the host requires an explicit gesture, wait until its reactive consent accessor permits it.
		if (options.getAcceptance && !options.getAcceptance()) return
		attemptedConditional = true
		const attempt = new AbortController()
		conditional = attempt
		void getPasskey(attempt.signal, true)
			.then((fields) => {
				if (fields && !attempt.signal.aborted && !controller.signal.aborted) {
					return run('passkey', async () => {
						await request('passkey_verify', fields)
					})
				}
				return undefined
			})
			.catch(() => undefined)
			.finally(() => {
				if (conditional === attempt) conditional = undefined
			})
	})

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
		emailRoute: `${route}/email`,
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
