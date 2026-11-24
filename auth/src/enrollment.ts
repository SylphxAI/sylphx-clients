import { type Data, field, type Identity, json, obj, problem, reply, token } from './http.js'

const base64url = /^[A-Za-z0-9_-]+$/

export async function enrollPasskey(
	identity: Identity,
	credential: string,
	instanceId: string,
	origin: string,
	input: Data,
) {
	if (input.action !== 'begin' && input.action !== 'verify') return problem('not_found', 404)
	const rpId = new URL(origin).hostname
	let body: Data = { rp_id: rpId, origin }
	if (input.action === 'verify') {
		body = {}
		for (const key of ['challenge_id', 'credential_id', 'client_data_json', 'attestation_object']) {
			const value = field(input, key, 16_384)
			if (!value || !base64url.test(value)) return problem('invalid_input', 400)
			body[key] = value
		}
		const transports = input.transports ?? []
		if (
			!Array.isArray(transports) ||
			transports.length > 16 ||
			!transports.every((value) => typeof value === 'string' && /^[a-z-]{1,32}$/.test(value))
		)
			return problem('invalid_input', 400)
		body.transports = transports
	}
	const session = await identity('/v1/client/session', { bearer: credential })
	const current = await json(session)
	if (
		!session.ok ||
		obj(current.session).instance_id !== instanceId ||
		obj(current.user).email_verified !== true
	)
		return problem('not_allowed', 403)
	const res = await identity(`/v1/client/me/passkeys/${input.action}`, { bearer: credential, body })
	if (!res.ok) {
		void res.body?.cancel().catch(() => undefined)
		return res.status === 429
			? problem('try_later', 429)
			: problem('method_unavailable', res.status >= 500 ? 503 : 403)
	}
	const data = await json(res)
	if (input.action === 'verify') {
		const passkey = obj(data.passkey)
		return token(passkey.credential_id) && passkey.rp_id === rpId && passkey.origin === origin
			? reply({ enrolled: true })
			: problem('unavailable', 502)
	}
	const { challenge_id, challenge, rp_id, user_id, user_name, public_key_algorithms } = data
	if (
		rp_id !== rpId ||
		!token(challenge_id) ||
		!token(challenge) ||
		!token(user_id) ||
		typeof user_name !== 'string' ||
		!user_name ||
		!Array.isArray(public_key_algorithms) ||
		!public_key_algorithms.length ||
		!public_key_algorithms.every(Number.isSafeInteger)
	)
		return problem('unavailable', 502)
	return reply({
		challengeId: challenge_id,
		challenge,
		rpId,
		userId: user_id,
		userName: user_name,
		algorithms: public_key_algorithms,
	})
}
