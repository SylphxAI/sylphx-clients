// @sylphx/sdk/events/webhooks: Standard Webhooks verification of Events deliveries.

import { describe, expect, test } from 'bun:test'
import { signWebhook, verifyWebhook, WebhookVerifyError } from '../src/events/webhooks.js'

// The Standard Webhooks reference vector (shared by the reference libraries).
// Public test values, not credentials.
const SECRET = 'whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw' // gitleaks:allow
const ID = 'msg_p5jXN8AQM9LWM0D4loKWxJek' // gitleaks:allow
const TS = 1614265330
const BODY = '{"test": 2432232314}'
const SIGNATURE = 'v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE='

function headers(signature: string, timestamp = TS, id = ID) {
	return {
		'webhook-id': id,
		'webhook-timestamp': String(timestamp),
		'webhook-signature': signature,
	}
}

async function refusal(promise: Promise<unknown>): Promise<string> {
	try {
		await promise
	} catch (error) {
		expect(error).toBeInstanceOf(WebhookVerifyError)
		return (error as WebhookVerifyError).code
	}
	throw new Error('expected a refusal')
}

describe('verifyWebhook', () => {
	test('signs and verifies the reference vector', async () => {
		expect(await signWebhook(SECRET, ID, TS, BODY)).toBe(SIGNATURE)
		const event = await verifyWebhook<{ test: number }>(BODY, headers(SIGNATURE), SECRET, {
			now: TS + 10,
		})
		expect(event.test).toBe(2432232314)
	})

	test('reads Headers objects and bytes bodies', async () => {
		const h = new Headers(headers(SIGNATURE))
		const event = await verifyWebhook(new TextEncoder().encode(BODY), h, SECRET, { now: TS })
		expect(event).toEqual({ test: 2432232314 })
	})

	test('accepts any one of several signatures during a secret rotation', async () => {
		const other = `whsec_${btoa('another-endpoint-secret-0123456')}`
		const both = `${await signWebhook(other, ID, TS, BODY)} ${SIGNATURE}`
		await verifyWebhook(BODY, headers(both), SECRET, { now: TS })
		await verifyWebhook(BODY, headers(both), other, { now: TS })
		const third = `whsec_${btoa('a-third-secret-that-never-signed')}`
		expect(await refusal(verifyWebhook(BODY, headers(both), third, { now: TS }))).toBe('signature')
	})

	test('refuses a timestamp outside five minutes, a changed body or id, and missing headers', async () => {
		expect(await refusal(verifyWebhook(BODY, headers(SIGNATURE), SECRET, { now: TS + 301 }))).toBe(
			'timestamp',
		)
		await verifyWebhook(BODY, headers(SIGNATURE), SECRET, { now: TS - 300 })
		expect(
			await refusal(verifyWebhook('{"test": 1}', headers(SIGNATURE), SECRET, { now: TS })),
		).toBe('signature')
		expect(
			await refusal(verifyWebhook(BODY, headers(SIGNATURE, TS, 'msg_other'), SECRET, { now: TS })),
		).toBe('signature')
		expect(await refusal(verifyWebhook(BODY, { 'webhook-id': ID }, SECRET, { now: TS }))).toBe(
			'malformed',
		)
		expect(
			await refusal(
				verifyWebhook(BODY, { ...headers(SIGNATURE), 'webhook-timestamp': 'soon' }, SECRET, {
					now: TS,
				}),
			),
		).toBe('malformed')
	})
})
