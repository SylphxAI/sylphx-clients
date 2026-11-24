import type { AuthMethod, EntryConfig, Provider } from './client.js'
import { type Data, type Identity, json, obj, token } from './http.js'

export type AuthInstance = { slug: string; id: string; environment: string }
export type InstanceBinding =
	| { instance: AuthInstance; publishableKey?: string }
	| { instance?: undefined; publishableKey: string }

const SLUG = /^[a-z0-9][a-z0-9-]{0,62}$/

export function bindInstance({ instance, publishableKey }: InstanceBinding) {
	if (
		instance &&
		(!SLUG.test(instance.slug) || !token(instance.id) || !token(instance.environment))
	)
		throw new TypeError('Expected an Auth instance')
	if (
		publishableKey !== undefined &&
		(!token(publishableKey) || !publishableKey.startsWith('sylphx_pk_'))
	)
		throw new TypeError('Expected an Identity publishable key')
	if (!instance && !publishableKey)
		throw new TypeError('Expected an Auth instance or publishable key')
	// Identity accepts the Access publishable key as Authorization: Bearer.
	const scope = publishableKey
		? { bearer: publishableKey }
		: { query: { instance: instance!.slug } }
	return {
		scope,
		async discover(identity: Identity) {
			const res = await identity('/v1/client/config', scope)
			const data = await json(res)
			const { id, slug, environment } = obj(data.instance)
			if (
				!res.ok ||
				!token(id) ||
				typeof slug !== 'string' ||
				!SLUG.test(slug) ||
				!token(environment) ||
				(instance &&
					(id !== instance.id || slug !== instance.slug || environment !== instance.environment))
			)
				throw new Error('Unexpected authentication instance')
			return { data, instance: { id, slug, environment } }
		},
	}
}

export function entryConfig(
	methods: Data,
	providers: Provider[],
	allows: (method: AuthMethod) => boolean,
	registration: boolean,
): EntryConfig {
	const password = allows('password') && methods.password === true
	return {
		providers,
		password,
		signup: password && registration && methods.sign_up === true,
		otp: allows('otp') && methods.email_otp === true,
		passkey: allows('passkey') && methods.passkey === true,
	}
}

export const emailMethods = {
	password: 'password',
	verify_signup: 'password',
	signup: 'password',
	mfa: 'password',
	otp_start: 'otp',
	otp_verify: 'otp',
	passkey_start: 'passkey',
	passkey_verify: 'passkey',
	recovery_start: 'password',
} as const satisfies Record<string, Exclude<AuthMethod, 'oauth'>>
