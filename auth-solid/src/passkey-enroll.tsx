import { authRoutePrefix } from '@sylphx/auth/client'
import { createMemo, createSignal, onCleanup, onMount, Show } from 'solid-js'
import { resolveAuthLocale } from './locale.js'
import { resolveSignInMessages, type SignInMessagesOverride } from './messages.js'
import { signInLanguage } from './sign-in-messages.js'
import { postAuthJSON, SignInError, type SignInErrorCode } from './sign-in-request.js'
import { decodeBase64Url, encodeBase64Url } from './sign-in-webauthn.js'

export type SylphxPasskeyEnrollProps = {
	/** Enable only for a signed-in view, when discovery advertises passkeyEnrollment. */
	enabled: boolean
	routePrefix?: string
	locale?: string
	messages?: SignInMessagesOverride
	class?: string
}

function creationOptions(
	input: unknown,
	hostname: string,
): {
	challengeId: string
	publicKey: PublicKeyCredentialCreationOptions
} {
	if (!input || typeof input !== 'object') throw new Error()
	const { challenge, challengeId, rpId, userId, userName, algorithms } = input as Record<
		string,
		unknown
	>
	const bytes = /^[A-Za-z0-9_-]+$/
	if (
		typeof challenge !== 'string' ||
		!bytes.test(challenge) ||
		typeof challengeId !== 'string' ||
		!challengeId ||
		rpId !== hostname ||
		typeof userId !== 'string' ||
		!bytes.test(userId) ||
		typeof userName !== 'string' ||
		!userName ||
		!Array.isArray(algorithms) ||
		!algorithms.length ||
		!algorithms.every(Number.isSafeInteger)
	)
		throw new Error()
	return {
		challengeId,
		publicKey: {
			challenge: decodeBase64Url(challenge),
			rp: { id: rpId, name: rpId },
			user: { id: decodeBase64Url(userId), name: userName, displayName: userName },
			pubKeyCredParams: algorithms.map((alg) => ({ type: 'public-key', alg })),
			authenticatorSelection: { residentKey: 'required', userVerification: 'required' },
			attestation: 'none',
			timeout: 60_000,
		},
	}
}

/** Add a discoverable passkey to the current account. Never takes identity or tokens from the UI. */
export function SylphxPasskeyEnroll(props: SylphxPasskeyEnrollProps) {
	const route = `${authRoutePrefix(props.routePrefix)}/passkey/enroll`
	const owner = new AbortController()
	const [supported, setSupported] = createSignal(false)
	const [busy, setBusy] = createSignal(false)
	const [added, setAdded] = createSignal(false)
	const [error, setError] = createSignal<SignInErrorCode>()
	const locale = () => resolveAuthLocale(props.locale ? [props.locale] : [])
	const text = createMemo(() => resolveSignInMessages(locale(), props.messages))
	onMount(() => setSupported(window.isSecureContext && typeof PublicKeyCredential !== 'undefined'))
	onCleanup(() => owner.abort())

	async function enroll() {
		if (busy() || !supported() || !props.enabled || owner.signal.aborted) return
		setBusy(true)
		setError(undefined)
		setAdded(false)
		try {
			const begin = await postAuthJSON(route, { action: 'begin' }, owner.signal)
			owner.signal.throwIfAborted()
			if (!props.enabled) return
			const { challengeId, publicKey } = creationOptions(begin, window.location.hostname)
			const credential = await navigator.credentials.create({ publicKey, signal: owner.signal })
			if (!props.enabled || owner.signal.aborted) return
			if (
				!(credential instanceof PublicKeyCredential) ||
				!(credential.response instanceof AuthenticatorAttestationResponse)
			)
				throw new Error()
			const response = credential.response
			const result = await postAuthJSON(
				route,
				{
					action: 'verify',
					challenge_id: challengeId,
					credential_id: encodeBase64Url(credential.rawId),
					client_data_json: encodeBase64Url(response.clientDataJSON),
					attestation_object: encodeBase64Url(response.attestationObject),
					transports: response.getTransports?.() ?? [],
				},
				owner.signal,
			)
			if (
				!result ||
				typeof result !== 'object' ||
				!('enrolled' in result) ||
				result.enrolled !== true
			)
				throw new Error()
			if (!owner.signal.aborted) setAdded(true)
		} catch (cause) {
			if (!owner.signal.aborted)
				setError(cause instanceof SignInError ? cause.code : 'method_unavailable')
		} finally {
			if (!owner.signal.aborted) setBusy(false)
		}
	}

	return (
		<Show when={supported() && props.enabled}>
			<div lang={signInLanguage(locale(), props.locale, props.messages)}>
				<button
					type="button"
					class={props.class ?? 'auth-sign-in-secondary'}
					disabled={busy()}
					onClick={() => void enroll()}
				>
					{busy() ? text().working : text().addPasskey}
				</button>
				<Show when={error()}>{(code) => <p role="alert">{text().errors[code()]}</p>}</Show>
				<Show when={added()}>
					<output>{text().passkeyAdded}</output>
				</Show>
			</div>
		</Show>
	)
}
