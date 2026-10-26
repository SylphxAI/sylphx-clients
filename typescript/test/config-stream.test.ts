import { expect, test } from 'bun:test'
import { Sylphx } from '../src/index.js'

const parent = 'orgs/org_a/projects/prj_a/envs/env_a'

test('generated watch decodes chunked revisions, passes the reconnect cursor and cancels', async () => {
	let cancelled = false
	let signal: AbortSignal | null | undefined
	const encoder = new TextEncoder()
	const client = new Sylphx({
		apiKey: 'sylphx_sk_live_test',
		fetch: async (url, init) => {
			expect(new URL(String(url)).pathname).toBe(`/v1/${parent}/config_flags:watch`)
			expect(new URL(String(url)).searchParams.get('if_none_match')).toBe('held')
			expect(new Headers(init?.headers).get('accept')).toBe('text/event-stream')
			signal = init?.signal
			return new Response(
				new ReadableStream<Uint8Array>({
					start(controller) {
						for (const chunk of [': keepalive\n\n', 'id: newer\ndata: {"et', 'ag":"newer"}\n\n']) {
							controller.enqueue(encoder.encode(chunk))
						}
					},
					cancel() {
						cancelled = true
					},
				}),
				{ headers: { 'content-type': 'text/event-stream' } },
			)
		},
	})
	const stream = client.config.configFlags.watch({ parent, ifNoneMatch: 'held' })
	expect(await stream.next()).toEqual({ done: false, value: { etag: 'newer' } })
	await stream.return(undefined)
	expect(cancelled).toBe(true)
	expect(signal?.aborted).toBe(true)
})

// Exercise the public evaluator over the real generated HTTP/SSE transport.
import { Flags } from '../src/config/flags.js'

const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 0))

async function until(predicate: () => boolean): Promise<void> {
	for (let i = 0; i < 100; i++) {
		if (predicate()) return
		await tick()
	}
	throw new Error('revision consumer did not advance')
}

test('local Flags invalidates on revision, shares one connection and closes a pending read', async () => {
	let revision = 'one'
	let watches = 0
	let snapshots = 0
	let controller: ReadableStreamDefaultController<Uint8Array> | undefined
	let signal: AbortSignal | null | undefined
	const client = new Sylphx({
		apiKey: 'sylphx_sk_live_test',
		maxRetries: 0,
		fetch: async (url, init) => {
			if (String(url).includes(':watch')) {
				watches++
				expect(new URL(String(url)).searchParams.get('if_none_match')).toBe('one')
				signal = init?.signal
				return new Response(
					new ReadableStream<Uint8Array>({
						start(c) {
							controller = c
							signal?.addEventListener('abort', () => c.error(new Error('aborted')), { once: true })
						},
					}),
					{ headers: { 'content-type': 'text/event-stream' } },
				)
			}
			snapshots++
			return Response.json({
				etag: revision,
				poll_interval: '30s',
				config_flags: [
					{
						name: `${parent}/config_flags/checkout`,
						spec: {
							value_type: 'bool',
							default_value: revision === 'two',
						},
					},
				],
			})
		},
	})
	const flags = new Flags({ client, parent, mode: 'local', now: () => 0 })
	try {
		expect(await flags.isEnabled('checkout')).toBe(false)
		await until(() => controller !== undefined)
		expect(await flags.isEnabled('checkout')).toBe(false)
		expect(snapshots).toBe(1)
		revision = 'two'
		controller!.enqueue(new TextEncoder().encode('id: two\ndata: {"etag":"two"}\n\n'))
		// The clock never advances: only the revision can invalidate the snapshot.
		await tick()
		expect(await flags.isEnabled('checkout')).toBe(true)
		expect(snapshots).toBe(2)
		expect(watches).toBe(1)
	} finally {
		flags.close()
	}
	await tick()
	expect(signal?.aborted).toBe(true)
})

test('local Flags can opt out of streaming and keeps the last snapshot offline', async () => {
	let offline = false
	let now = 0
	const client = new Sylphx({
		maxRetries: 0,
		fetch: async (url) => {
			expect(String(url)).not.toContain(':watch')
			if (offline) throw new Error('offline')
			return Response.json({
				etag: 'one',
				poll_interval: '1s',
				config_flags: [
					{
						name: `${parent}/config_flags/checkout`,
						spec: { value_type: 'bool', default_value: true },
					},
				],
			})
		},
	})
	const flags = new Flags({ client, parent, mode: 'local', stream: false, now: () => now })
	expect(await flags.isEnabled('checkout')).toBe(true)
	offline = true
	now = 2000
	expect(await flags.isEnabled('checkout')).toBe(true)
	flags.close()
})
