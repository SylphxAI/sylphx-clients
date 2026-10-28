// Contract: docs/services/contracts/auth-client-continue.md.
import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import Ajv2020 from 'ajv/dist/2020.js'
import contract from '../../../contracts/fixtures/auth-client-continue.json' with { type: 'json' }
import {
	AuthClientError,
	type AuthClientOptions,
	type AuthContinue,
	createAuthClient,
} from '../src/auth/client.js'
import { type ContinueMockOptions, createContinueMock } from '../src/auth/continue-mock.js'

const ajv = new Ajv2020.default({ strict: true, allErrors: true })
ajv.addSchema(contract.schema)
function fits(name: string, value: unknown) {
	const validate = ajv.getSchema(`${contract.schema.$id}#/$defs/${name}`)
	expect(validate, name).toBeDefined()
	expect(validate!(value), ajv.errorsText(validate!.errors)).toBe(true)
}
const key = 'sylphx_pk_test_continue'
const agent = 'contract-browser'
function setup(options: Partial<ContinueMockOptions> = {}, userAgent: string | undefined = agent) {
	const mock = createContinueMock({ publishableKey: key, ...options })
	// Every SDK exchange is validated independently of the mock implementation.
	const fetch: NonNullable<AuthClientOptions['fetch']> = async (input, init) => {
		const req = new Request(input, init)
		const door = new URL(req.url).pathname.endsWith('/verify')
			? contract.doors.verify
			: contract.doors.continue
		fits(door.request, await req.clone().json())
		const response = await mock.fetch(req.url, {
			method: req.method,
			headers: req.headers,
			body: await req.text(),
		})
		const body = await response.clone().json()
		fits(response.ok ? door.response : 'Error', body)
		if (!response.ok)
			expect(contract.errors[body.code as keyof typeof contract.errors]).toBe(response.status)
		return response
	}
	const client = createAuthClient({ publishableKey: key, fetch, userAgent })
	return { mock, client }
}
function otp(step: AuthContinue) {
	expect(step.step).toBe('email_otp')
	if (step.step !== 'email_otp') throw new Error('expected email OTP')
	return step
}
async function rejected(promise: Promise<unknown>, code = 'unauthorized', status = 401) {
	try {
		await promise
		throw new Error('expected rejection')
	} catch (error) {
		expect(error).toBeInstanceOf(AuthClientError)
		expect((error as AuthClientError).code).toBe(code)
		expect((error as AuthClientError).status).toBe(status)
	}
}

describe('SDK continue contract against mock', () => {
	test('browser mode creates a session, maps the user and remembers its bearer; flow is single-use', async () => {
		const { client, mock } = setup()
		const step = otp(await client.continue({ email: 'New@Example.com', sessionMode: 'browser' }))
		expect(step.expiresIn).toBe(600)
		const mail = mock.outbox[0]!
		expect(mail.email).toBe('new@example.com')
		const result = await client.continueVerify({
			flow: step.flow,
			code: mail.code,
		})
		expect('session' in result).toBe(true)
		if ('session' in result) {
			expect(result.user.email).toBe('new@example.com')
			expect(result.user.emailVerified).toBe(true)
			expect(client.sessionToken).toBe(result.session.token)
			expect(result.session.expiresAt).toBeGreaterThan(0)
		}
		await rejected(client.continueVerify({ flow: step.flow, code: mail.code }))
	})

	test('server mode is the default and returns a ticket without remembering a token', async () => {
		const { client, mock } = setup()
		const step = otp(await client.continue({ email: 'new@example.com' }))
		const result = await client.continueVerify({
			flow: step.flow,
			code: mock.outbox[0]!.code,
		})
		expect('ticket' in result).toBe(true)
		if ('ticket' in result) expect(result.expiresIn).toBe(120)
		expect(client.sessionToken).toBeUndefined()
	})

	test('password selection sends no mail; explicit fallback preserves the password method', async () => {
		const { client, mock } = setup({ accounts: [{ email: 'known@example.com', password: true }] })
		expect(await client.continue({ email: 'known@example.com' })).toEqual({
			step: 'password',
		})
		expect(mock.outbox).toHaveLength(0)
		const step = otp(await client.continue({ email: 'known@example.com', method: 'email_otp' }))
		await client.continueVerify({ flow: step.flow, code: mock.outbox[0]!.code })
		expect(await client.continue({ email: 'known@example.com' })).toEqual({
			step: 'password',
		})
	})

	test('known passwordless and unknown emails have the same response shape', async () => {
		const { client } = setup({ accounts: [{ email: 'known@example.com' }] })
		const a = otp(await client.continue({ email: 'known@example.com' }))
		const b = otp(await client.continue({ email: 'new@example.com' }))
		expect({ ...a, flow: null }).toEqual({ ...b, flow: null })
		expect(a.flow).not.toBe(b.flow)
	})

	test('wrong browser or environment cannot verify or consume the legitimate flow', async () => {
		const { client, mock } = setup()
		const step = otp(await client.continue({ email: 'new@example.com' }))
		const code = mock.outbox[0]!.code
		const wrongBrowser = createAuthClient({
			publishableKey: key,
			fetch: mock.fetch,
			userAgent: 'other-browser',
		})
		await rejected(wrongBrowser.continueVerify({ flow: step.flow, code }))
		const other = createAuthClient({
			publishableKey: 'sylphx_pk_test_other',
			fetch: mock.fetch,
			userAgent: agent,
		})
		await rejected(other.continueVerify({ flow: step.flow, code }))
		expect('ticket' in (await client.continueVerify({ flow: step.flow, code }))).toBe(true)
	})

	test('a flow expires at ten minutes', async () => {
		let now = 1_800_000_000_000
		const { client, mock } = setup({ now: () => now })
		const step = otp(await client.continue({ email: 'new@example.com' }))
		now += 600_000
		await rejected(client.continueVerify({ flow: step.flow, code: mock.outbox[0]!.code }))
	})

	test('five failed attempts burn the flow', async () => {
		const { client, mock } = setup()
		const step = otp(await client.continue({ email: 'new@example.com' }))
		const code = mock.outbox[0]!.code
		const wrong = code.replace(/\d/g, (digit) => String((Number(digit) + 1) % 10))
		for (let i = 0; i < 5; i++)
			await rejected(client.continueVerify({ flow: step.flow, code: wrong }))
		await rejected(client.continueVerify({ flow: step.flow, code }))
	})

	test('sign-up disabled refuses unknown accounts only at verify', async () => {
		const { client, mock } = setup({ signUp: false })
		const step = otp(await client.continue({ email: 'new@example.com' }))
		await rejected(
			client.continueVerify({ flow: step.flow, code: mock.outbox[0]!.code }),
			'forbidden',
			403,
		)
	})

	test('disabled email OTP sends no code', async () => {
		const { client, mock } = setup({ emailOtp: false })
		await rejected(client.continue({ email: 'new@example.com' }), 'forbidden', 403)
		expect(mock.outbox).toHaveLength(0)
	})

	test('a missing User-Agent is unauthorized', async () => {
		const { client } = setup({}, '')
		await rejected(client.continue({ email: 'new@example.com' }))
	})

	test('MFA proofs use wire field names and mfa_required preserves the held session', async () => {
		let received: unknown
		const client = createAuthClient({
			publishableKey: key,
			fetch: async (_input, init) => {
				received = JSON.parse(String(init?.body))
				return Response.json({ code: 'mfa_required', error: 'MFA required' }, { status: 401 })
			},
		})
		client.setSessionToken('existing-session')
		await rejected(
			client.continueVerify({
				flow: 'flow',
				code: '123456',
				factorProofs: [{ factorType: 'totp', factorId: 'factor', response: '654321' }],
			}),
			'mfa_required',
		)
		expect(received).toEqual({
			flow: 'flow',
			code: '123456',
			factor_proofs: [{ factor_type: 'totp', factor_id: 'factor', response: '654321' }],
		})
		expect(client.sessionToken).toBe('existing-session')
	})

	test('schema rejects obsolete draft fields and missing browser session fields', () => {
		const valid = (name: string, value: unknown) =>
			ajv.getSchema(`${contract.schema.$id}#/$defs/${name}`)!(value)
		expect(valid('ContinueRequest', { identifier: 'a@example.com' })).toBe(false)
		expect(valid('ContinueRequest', { email: 'a@example.com', method: 'email_code' })).toBe(false)
		expect(valid('ContinueResponse', { step: 'email_otp' })).toBe(false)
		expect(valid('ContinueVerifyResponse', { session: {}, user: {} })).toBe(false)
	})

	test('committed generated client and mock equal their generator templates', () => {
		for (const [output, template] of [
			['client.ts', 'auth_client.ts.tmpl'],
			['continue-mock.ts', 'auth_continue_mock.ts.tmpl'],
		]) {
			const source = readFileSync(
				new URL(`../../../tools/sylphx-gen/templates/ts/${template}`, import.meta.url),
				'utf8',
			)
			expect(readFileSync(new URL(`../src/auth/${output}`, import.meta.url), 'utf8')).toBe(
				`// @generated by sylphx-gen. Do not edit.\n${source}`,
			)
		}
	})
})
