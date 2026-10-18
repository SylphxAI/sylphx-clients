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
