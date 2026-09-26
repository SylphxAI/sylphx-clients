// Sylphx Observability error capture through the generated SDK: the raw
// runtime stack travels as `stack`, and a service or page with only the
// environment's publishable key (SYLPHX_PUBLISHABLE_KEY) captures with it.

import { afterEach, describe, expect, test } from 'bun:test'
import { type FetchLike, Sylphx } from '../src/index.js'

const saved: Record<string, string | undefined> = {}
function setEnv(name: string, value: string | undefined): void {
	if (!(name in saved)) saved[name] = process.env[name]
	if (value === undefined) delete process.env[name]
	else process.env[name] = value
}
afterEach(() => {
	for (const [k, v] of Object.entries(saved)) {
		if (v === undefined) delete process.env[k]
		else process.env[k] = v
	}
})

function capture() {
	const sent: { url: string; headers: Record<string, string>; body: unknown }[] = []
	const impl: FetchLike = async (input, init) => {
		sent.push({
			url: String(input),
			headers: (init?.headers ?? {}) as Record<string, string>,
			body: JSON.parse(String(init?.body)),
		})
		return new Response(JSON.stringify({ error_event: { name: 'e' }, sample_rate: 1 }), {
			status: 200,
		})
	}
	return { sent, fetch: impl }
}

const EVENT = {
	exceptionType: 'TypeError',
	message: 'boom',
	stack: 'TypeError: boom\n    at f (app.js:1:2)',
}

describe('observability capture', () => {
	test('the stack text is sent as `stack`', async () => {
		const f = capture()
		setEnv('SYLPHX_API_KEY', 'sylphx_sk_live_env')
		await new Sylphx({ fetch: f.fetch }).observability.errorGroups.capture({
			parent: 'orgs/-/projects/-/envs/-',
			errorEvent: EVENT,
		})
		expect(f.sent[0]!.url).toBe(
			'https://api.sylphx.com/v1/orgs/-/projects/-/envs/-/error_groups:capture',
		)
		expect(f.sent[0]!.body).toEqual({
			error_event: { exception_type: 'TypeError', message: 'boom', stack: EVENT.stack },
		})
	})

	test('without a secret key the environment publishable key is used', async () => {
		const f = capture()
		setEnv('SYLPHX_API_KEY', undefined)
		setEnv('SYLPHX_PUBLISHABLE_KEY', 'sylphx_pk_live_env')
		await new Sylphx({ fetch: f.fetch }).observability.errorGroups.capture({
			parent: 'orgs/-/projects/-/envs/-',
			errorEvent: EVENT,
		})
		expect(f.sent[0]!.headers.authorization).toBe('Bearer sylphx_pk_live_env')
	})

	test('a secret key wins over the publishable one', async () => {
		const f = capture()
		setEnv('SYLPHX_API_KEY', 'sylphx_sk_live_env')
		setEnv('SYLPHX_PUBLISHABLE_KEY', 'sylphx_pk_live_env')
		await new Sylphx({ fetch: f.fetch }).observability.errorGroups.capture({
			parent: 'orgs/-/projects/-/envs/-',
			errorEvent: EVENT,
		})
		expect(f.sent[0]!.headers.authorization).toBe('Bearer sylphx_sk_live_env')
	})
})
