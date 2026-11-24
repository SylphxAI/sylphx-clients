import { RANDOM } from './http.js'

export type AcceptanceVersions = Readonly<Record<string, string>>
export type ClickwrapPolicy = {
	/** Shared, high-entropy server-only key (at least 32 UTF-8 bytes). */
	signingKey: string
	/** Check the submitted versions against current policy. Only true permits sign-in. */
	validate(versions: AcceptanceVersions, request: Request): boolean | Promise<boolean>
}

export function readAcceptance(value: unknown): AcceptanceVersions | undefined {
	if (!value || typeof value !== 'object' || Array.isArray(value)) return
	const entries = Object.entries(value)
	if (!entries.length || entries.length > 8) return
	const valid = entries.every(
		([key, version]) =>
			/^[A-Za-z0-9][A-Za-z0-9_-]{0,31}$/.test(key) &&
			typeof version === 'string' &&
			/^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}$/.test(version),
	)
	return valid ? Object.freeze(Object.assign(Object.create(null), value)) : undefined
}

export function acceptanceSigner(secret: string) {
	const encoder = new TextEncoder()
	const bytes = encoder.encode(secret)
	if (bytes.byteLength < 32) throw new TypeError('Expected a high-entropy clickwrap signing key')
	const key = () =>
		crypto.subtle.importKey('raw', bytes, { name: 'HMAC', hash: 'SHA-256' }, false, [
			'sign',
			'verify',
		])
	return {
		async sign(value: string) {
			return encode(
				new Uint8Array(await crypto.subtle.sign('HMAC', await key(), encoder.encode(value))),
			)
		},
		async verify(value: string, mac: string) {
			return (
				RANDOM.test(mac) &&
				crypto.subtle.verify('HMAC', await key(), decode(mac), encoder.encode(value))
			)
		},
	}
}

function encode(bytes: Uint8Array) {
	return btoa(String.fromCharCode(...bytes))
		.replaceAll('+', '-')
		.replaceAll('/', '_')
		.replace(/=+$/, '')
}
function decode(value: string) {
	return Uint8Array.from(atob(value.replaceAll('-', '+').replaceAll('_', '/')), (c) =>
		c.charCodeAt(0),
	)
}
export function encodeAcceptance(versions: AcceptanceVersions) {
	return encode(new TextEncoder().encode(JSON.stringify(versions)))
}
export function decodeAcceptance(value: string): AcceptanceVersions | undefined {
	if (!/^[A-Za-z0-9_-]{1,1100}$/.test(value)) return
	return readAcceptance(JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(decode(value))))
}
