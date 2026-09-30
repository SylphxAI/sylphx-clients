// @sylphx/sdk/auth/verify: offline JWS verification against a JWKS.

import { describe, expect, test } from 'bun:test'
import { createVerifier, type Jwks, VerifyError, verifyWithKeys } from '../src/auth/verify.js'

const NOW = 1_800_000_000
const ISS = 'https://api.sylphx.com'

function b64(v: Uint8Array | string): string {
	const bytes = typeof v === 'string' ? new TextEncoder().encode(v) : v
	let s = ''
	for (const b of bytes) s += String.fromCharCode(b)
	return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}

async function es256() {
	const pair = (await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, [
		'sign',
		'verify',
	])) as CryptoKeyPair
	const jwk = await crypto.subtle.exportKey('jwk', pair.publicKey)
	const sign = async (header: object, claims: object) => {
		const signed = `${b64(JSON.stringify(header))}.${b64(JSON.stringify(claims))}`
		const sig = await crypto.subtle.sign(
			{ name: 'ECDSA', hash: 'SHA-256' },
			pair.privateKey,
			new TextEncoder().encode(signed),
		)
		return `${signed}.${b64(new Uint8Array(sig))}`
	}
	return {
		key: { kid: 'k1', kty: 'EC', crv: 'P-256', x: jwk.x, y: jwk.y, alg: 'ES256', use: 'sig' },
		sign,
	}
}

const good = { iss: ISS, aud: ['app'], sub: 'u1', exp: NOW + 300, iat: NOW - 10 }
const expectApp = { issuer: ISS, audience: 'app' }

async function code(p: Promise<unknown>): Promise<string> {
	try {
		await p
		return 'ok'
	} catch (e) {
		return e instanceof VerifyError ? e.code : `other:${String(e)}`
	}
}

describe('auth/verify', () => {
	test('a valid ES256 token verifies', async () => {
		const { key, sign } = await es256()
		const claims = await verifyWithKeys(
			await sign({ alg: 'ES256', kid: 'k1' }, good),
			{ keys: [key] },
			expectApp,
			NOW,
		)
		expect(claims.sub).toBe('u1')
	})

	test('hostile tokens are refused', async () => {
		const { key, sign } = await es256()
		const keys: Jwks = { keys: [key] }
		const h = { alg: 'ES256', kid: 'k1' }
		const other = await es256()
		const valid = await sign(h, good)
		const [vh, , vs] = valid.split('.')
		const cases: [string, string, string][] = [
			['expired', await sign(h, { ...good, exp: NOW - 120 }), 'expired'],
			['no exp', await sign(h, { ...good, exp: undefined }), 'expired'],
			['not yet valid', await sign(h, { ...good, nbf: NOW + 600 }), 'expired'],
			['other issuer', await sign(h, { ...good, iss: 'https://evil.example' }), 'issuer'],
			['other audience', await sign(h, { ...good, aud: 'other' }), 'audience'],
			['alg none', `${b64('{"alg":"none"}')}.${b64(JSON.stringify(good))}.`, 'algorithm'],
			[
				'alg HS256',
				`${b64('{"alg":"HS256","kid":"k1"}')}.${b64(JSON.stringify(good))}.x`,
				'algorithm',
			],
			['unknown kid', await sign({ alg: 'ES256', kid: 'zz' }, good), 'unknown_key'],
			['two parts', 'a.b', 'malformed'],
			['garbage', '!!.!!.!!', 'malformed'],
			[
				'swapped payload',
				`${vh}.${b64(JSON.stringify({ ...good, sub: 'admin' }))}.${vs}`,
				'signature',
			],
			['another key, same kid', await other.sign(h, good), 'signature'],
		]
		for (const [name, token, want] of cases)
			expect(`${name}: ${await code(verifyWithKeys(token, keys, expectApp, NOW))}`).toBe(
				`${name}: ${want}`,
			)
		expect(
			await code(verifyWithKeys(await sign(h, { ...good, exp: NOW - 30 }), keys, expectApp, NOW)),
		).toBe('ok')
	})

	test('an omitted or empty audience refuses every token', async () => {
		const { key, sign } = await es256()
		const keys: Jwks = { keys: [key] }
		const token = await sign({ alg: 'ES256', kid: 'k1' }, good)
		for (const audience of [undefined, '', [], ['']]) {
			const expect_ = { issuer: ISS, audience } as unknown as { issuer: string; audience: string }
			expect(await code(verifyWithKeys(token, keys, expect_, NOW))).toBe('audience')
		}
	})

	test('an unknown kid refetches the key set once, then verifies', async () => {
		const a = await es256()
		const b = await es256()
		let served: Jwks = { keys: [a.key] }
		let fetches = 0
		const verify = createVerifier({
			...expectApp,
			fetch: (async () => {
				fetches++
				return new Response(JSON.stringify(served))
			}) as unknown as typeof fetch,
		})
		const now = Math.floor(Date.now() / 1000)
		const claims = { ...good, exp: now + 300, iat: now }
		await verify(await a.sign({ alg: 'ES256', kid: 'k1' }, claims))
		served = { keys: [{ ...b.key, kid: 'k2' }] }
		const rotated = await verify(await b.sign({ alg: 'ES256', kid: 'k2' }, claims))
		expect(rotated.sub).toBe('u1')
		expect(fetches).toBe(2)
		// A flood of unknown kids refetches no more than once per 30 s.
		for (let i = 0; i < 5; i++)
			expect(await code(verify(await b.sign({ alg: 'ES256', kid: `bad${i}` }, claims)))).toBe(
				'unknown_key',
			)
		expect(fetches).toBe(2)
	})
})
