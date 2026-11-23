import {
	type Data,
	field,
	type Identity,
	json,
	obj,
	problem,
	RANDOM,
	reply,
	token,
} from './http.js'
import type { IssueSession } from './server.js'

const CODE = /^[0-9]{6}$/

export function refused(res: Response, error: string) {
	return res.status === 429
		? problem('try_later', 429)
		: res.status >= 500
			? problem('unavailable', 503)
			: problem(error, 400)
}
export async function emailEntry(
	identity: Identity,
	input: Data,
	next: string,
	ctx: {
		origin: string
		loginPath: string
		instance: Record<string, string>
		session: IssueSession
	},
) {
	const { origin, instance, session: issue } = ctx
	const rp = new URL(origin).hostname
	const action = input.action
	const email = field(input, 'email', 254)?.trim()
	const password = field(input, 'password', 1024)
	const code = field(input, 'code', 128)
	let path: string
	let body: Data = { session_mode: 'browser' }
	const challengeId = field(input, 'challengeId', 200)
	const secret = field(input, 'verification', 256)
	const verification =
		token(challengeId) && token(secret)
			? { idempotency_key: crypto.randomUUID(), challenge_id: challengeId, secret }
			: undefined
	switch (action) {
		case 'password':
		case 'verify_signup': {
			if (!email || !password) return problem('invalid_input', 400)
			if (action === 'verify_signup' && !verification) return problem('verification_failed', 400)
			path = '/v1/client/sign-in/password'
			Object.assign(body, { email, password })
			break
		}
		case 'signup': {
			if (
				!email ||
				!password ||
				new TextEncoder().encode(password).byteLength < 12 ||
				new TextEncoder().encode(password).byteLength > 1024
			)
				return problem('invalid_input', 400)
			const target = new URL(ctx.loginPath, origin)
			target.searchParams.set('next', next)
			const res = await identity('/v1/client/sign-up', {
				query: instance,
				body: { email, password, redirect_url: target.href },
			})
			const data = await json(res)
			return res.status === 202 && data.accepted === true
				? reply({ step: 'check_email' }, 202)
				: refused(res, 'signup_failed')
		}
		case 'otp_start':
			if (!email) return problem('invalid_input', 400)
			path = '/v1/client/continue'
			Object.assign(body, { email, method: 'email_otp' })
			break
		case 'otp_verify': {
			const flow = field(input, 'flow', 43)
			if (!flow || !RANDOM.test(flow) || !code || !CODE.test(code))
				return problem('invalid_code', 400)
			path = '/v1/client/continue/verify'
			body = { flow, code }
			const factor = field(input, 'factor', 128)
			if (factor && (input.method === 'totp' || input.method === 'recovery_code'))
				body.factor_proofs = [{ factor_type: input.method, factor_id: '', response: factor }]
			break
		}
		case 'mfa': {
			const challenge = field(input, 'challenge', 512)
			if (
				!token(challenge) ||
				!code ||
				(input.method !== 'totp' && input.method !== 'recovery_code')
			)
				return problem('invalid_code', 400)
			path = '/v1/client/mfa/totp/verify'
			body = { challenge, code, method: input.method }
			break
		}
		case 'passkey_start':
			path = '/v1/client/sign-in/passkey/begin'
			body = { rp_id: rp, origin }
			break
		case 'passkey_verify':
			path = '/v1/client/sign-in/passkey/verify'
			for (const key of [
				'challenge_id',
				'credential_id',
				'client_data_json',
				'authenticator_data',
				'signature',
			]) {
				const value = field(input, key, 3500)
				if (!token(value)) return problem('invalid_input', 400)
				body[key] = value
			}
			if (token(input.user_handle)) body.user_handle = input.user_handle
			break
		default:
			return problem('not_found', 404)
	}
	const res = await identity(path, { query: instance, body })
	const data = await json(res)
	if (res.status === 401 && data.code === 'mfa_required') {
		if ((action === 'password' || action === 'verify_signup') && token(data.mfa_challenge))
			return reply({ step: 'mfa', challenge: data.mfa_challenge })
		if (action === 'otp_verify' && !input.factor) return reply({ step: 'mfa' })
	}
	if (!res.ok)
		return refused(
			res,
			action === 'password' || action === 'verify_signup'
				? 'invalid_credentials'
				: action === 'mfa' || action === 'otp_verify'
					? 'invalid_code'
					: 'method_unavailable',
		)
	if (
		action === 'otp_start' &&
		data.step === 'email_otp' &&
		typeof data.flow === 'string' &&
		RANDOM.test(data.flow)
	)
		return reply({ flow: data.flow })
	if (
		action === 'passkey_start' &&
		data.rp_id === rp &&
		token(data.challenge) &&
		token(data.challenge_id) &&
		Array.isArray(data.allowed_credential_ids) &&
		data.allowed_credential_ids.every(token)
	)
		return reply({
			challenge: data.challenge,
			challengeId: data.challenge_id,
			rpId: rp,
			credentials: data.allowed_credential_ids,
		})
	if (action === 'otp_start' || action === 'passkey_start') return problem('unavailable', 502)
	const session = obj(data.session)
	const credential = session.token
	const user = obj(data.user)
	if (!token(credential) || !token(user.id)) return problem('unavailable', 502)
	if (verification && (action === 'verify_signup' || action === 'mfa')) {
		// Require both the password session and mail proof. Auth binds them to
		// the same principal and preserves the registrant's chosen password.
		const confirmed = await identity('/v1/email-verification/complete', {
			bearer: credential,
			body: verification,
		})
		const result = await json(confirmed)
		if (!confirmed.ok || obj(result.verification).principal_id !== user.id)
			return refused(confirmed, 'verification_failed')
	} else if (user.email_verified !== true) return problem('verify_email', 403)
	const cookies = await issue({ token: credential, expiresAt: session.expires_at })
	return cookies ? reply({ next }, 200, cookies) : problem('session_unavailable', 502)
}
