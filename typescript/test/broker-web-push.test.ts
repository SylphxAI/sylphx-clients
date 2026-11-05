import { expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { Sylphx } from '../src/index.js'

interface PublicProof {
	response: { application_server_key: string; expire_time: string }
	expected_public_key_hex: string
	authorization_public_key: string
}

async function subscribeThroughCodec(proof: PublicProof) {
	const sdk = new Sylphx({
		apiKey: 'sylphx_sk_test_fixture',
		baseUrl: 'https://api.example',
		fetch: async (url, init) => {
			expect(url.pathname).toBe('/v1/orgs/org_a/connections/web-push:signVapidAssertion')
			expect(init.method).toBe('POST')
			expect(JSON.parse(String(init.body))).toEqual({ audience: 'https://push.example' })
			return new Response(JSON.stringify(proof.response), { status: 200 })
		},
	})
	const response = await sdk.broker.connections.signVapidAssertion({
		name: 'orgs/org_a/connections/web-push',
		audience: 'https://push.example',
	})
	const encoded = response.applicationServerKey
	expect(encoded).toBe(proof.response.application_server_key)
	expect(encoded?.endsWith('=')).toBe(true)
	let subscribed: Uint8Array | undefined
	const registration = {
		pushManager: {
			subscribe(options: { userVisibleOnly: boolean; applicationServerKey: unknown }) {
				// The browser accepts BufferSource or base64url, never a padded
				// standard-base64 string. Exercise that negative boundary first.
				if (!(options.applicationServerKey instanceof Uint8Array)) {
					throw new TypeError('subscription must receive decoded key bytes')
				}
				expect(options.userVisibleOnly).toBe(true)
				subscribed = options.applicationServerKey
				return Promise.resolve()
			},
		},
	}
	expect(() =>
		registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: encoded }),
	).toThrow(TypeError)
	expect(subscribed).toBeUndefined()
	const key = Uint8Array.from(atob(encoded!), (c) => c.charCodeAt(0))
	await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key })
	expect(subscribed).toBeInstanceOf(Uint8Array)
	expect(key).toHaveLength(65)
	expect(key[0]).toBe(4)
	expect(Buffer.from(key).toString('hex')).toBe(proof.expected_public_key_hex)
	expect(Buffer.from(key).toString('base64url')).toBe(proof.authorization_public_key)
}

test('Broker bytes survive the real SDK JSON codec and reach subscribe as BufferSource', async () => {
	// Existing independently derived Keys public vector; deliberately contains
	// standard-base64 + and / so an alphabet mixup cannot pass accidentally.
	const raw = Buffer.from(
		'041fbaefce8b13fedeb0680bed822e29116c39c7ba3f24267af593d33262bcfb56834a9d79432f71fd06428178b3eb11167f1c8fff0d557df024f92ff69721edb9',
		'hex',
	)
	const standard = raw.toString('base64')
	expect(standard).toMatch(/[+/]/)
	await subscribeThroughCodec({
		response: { application_server_key: standard, expire_time: '2026-11-05T00:00:00Z' },
		expected_public_key_hex: raw.toString('hex'),
		authorization_public_key: raw.toString('base64url'),
	})
})

const publicFixture = process.env.VAPID_PUBLIC_WIRE_FIXTURE
test.skipIf(!publicFixture)(
	'actual Broker HTTP signer key passes the SDK and browser byte boundary',
	async () => {
		const proof = JSON.parse(readFileSync(publicFixture!, 'utf8')) as PublicProof
		await subscribeThroughCodec(proof)
	},
)
