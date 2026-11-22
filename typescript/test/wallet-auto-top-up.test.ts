import { expect, expectTypeOf, test } from 'bun:test'
import {
	autoTopUpMine,
	SylphxError,
	type WalletAutoTopUpOptions,
	type WalletAutoTopUpRequest,
	type WalletAutoTopUpSetup,
	type WalletAutoTopUpTerms,
} from '../src/index.js'

const credentials = {
	parent: 'orgs/test-org/projects/test-project/envs/production',
	publishableKey: 'sylphx_pk_fixture',
	accessToken: 'fixture-auth-token',
}
const empty = { version: 0, enabled: false, has_payment_method: false }
const terms = {
	version: 1,
	enabled: true,
	has_payment_method: true,
	threshold_minor: 500,
	recharge_amount_minor: 1000,
	monthly_cap_minor: 3000,
}
const intent = {
	setup_intent_id: 'seti_fixture',
	client_secret: 'fixture-secret',
	stripe_account: 'acct_fixture',
}

function mockFunding(wire: object, reply: object): WalletAutoTopUpOptions {
	return {
		...credentials,
		fetch: async (url, init) => {
			expect(url.href).toBe(
				'https://api.sylphx.com/v1/money/orgs/test-org/projects/test-project/envs/production/wallet:autoTopUpMine',
			)
			expect(init.method).toBe('POST')
			const headers = new Headers(init.headers)
			expect(headers.get('x-sylphx-key')).toBe(credentials.publishableKey)
			expect(headers.get('authorization')).toBe('Bearer ' + credentials.accessToken)
			expect(headers.get('content-type')).toBe('application/json')
			expect(JSON.parse(String(init.body))).toEqual({ currency: 'USD', funding: wire })
			return Response.json(reply)
		},
	}
}

const status = { currency: 'USD', funding: { action: 'status' } } satisfies WalletAutoTopUpRequest

test('reads status and preserves absent terms on an unconfigured wallet', async () => {
	const result = await autoTopUpMine(mockFunding({ action: 'status' }, empty), status)
	expectTypeOf(result).toEqualTypeOf<WalletAutoTopUpTerms>()
	expect(result).toEqual({ version: 0, enabled: false, hasPaymentMethod: false })
})

test('sends the setup request ID and version zero and returns only setup details', async () => {
	const result = await autoTopUpMine(
		mockFunding({ action: 'setup', expected_version: 0, request_id: 'setup_1' }, intent),
		{
			currency: 'USD',
			funding: { action: 'setup', expectedVersion: 0, requestId: 'setup_1' },
		},
	)
	expectTypeOf(result).toEqualTypeOf<WalletAutoTopUpSetup>()
	expect(result).toEqual({
		setupIntentId: 'seti_fixture',
		clientSecret: 'fixture-secret',
		stripeAccount: 'acct_fixture',
	})
})

test('enables with a confirmed setup intent and integer amounts', async () => {
	const result = await autoTopUpMine(
		mockFunding(
			{
				action: 'enable',
				expected_version: 0,
				setup_intent_id: 'seti_fixture',
				threshold_minor: 500,
				recharge_amount_minor: 1000,
				monthly_cap_minor: 3000,
			},
			terms,
		),
		{
			currency: 'USD',
			funding: {
				action: 'enable',
				expectedVersion: 0,
				setupIntentId: 'seti_fixture',
				thresholdMinor: 500,
				rechargeAmountMinor: 1000,
				monthlyCapMinor: 3000,
			},
		},
	)
	expectTypeOf(result).toEqualTypeOf<WalletAutoTopUpTerms>()
	expect(result).toEqual({
		version: 1,
		enabled: true,
		hasPaymentMethod: true,
		thresholdMinor: 500,
		rechargeAmountMinor: 1000,
		monthlyCapMinor: 3000,
	})
})

for (const action of ['pause', 'cancel'] as const) {
	test(`sends ${action} with its expected version`, async () => {
		const hasPaymentMethod = action === 'pause'
		const result = await autoTopUpMine(
			mockFunding(
				{ action, expected_version: 1 },
				{ ...terms, version: 2, enabled: false, has_payment_method: hasPaymentMethod },
			),
			{ currency: 'USD', funding: { action, expectedVersion: 1 } },
		)
		expect(result).toMatchObject({ version: 2, enabled: false, hasPaymentMethod })
	})
}

test('surfaces a stale version as SylphxError without resending the mutation', async () => {
	let attempts = 0
	const promise = autoTopUpMine(
		{
			...credentials,
			baseUrl: 'https://mock.invalid/',
			fetch: async (url) => {
				expect(url.origin).toBe('https://mock.invalid')
				attempts++
				return Response.json(
					{ code: 'ETAG_MISMATCH', retryable: true, effect: 'none' },
					{
						status: 409,
						headers: { 'sylphx-request-id': 'fixture-request' },
					},
				)
			},
		},
		{ currency: 'USD', funding: { action: 'pause', expectedVersion: 1 } },
	)
	await expect(promise).rejects.toBeInstanceOf(SylphxError)
	await expect(promise).rejects.toMatchObject({
		code: 'ETAG_MISMATCH',
		status: 409,
		requestId: 'fixture-request',
	})
	expect(attempts).toBe(1)
})
