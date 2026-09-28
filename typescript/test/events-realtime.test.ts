// @sylphx/sdk/events/realtime: the realtime channel client.

import { describe, expect, test } from 'bun:test'
import {
	parseRealtimeFrame,
	type RealtimeMessage,
	subscribeRealtime,
	type WebSocketLike,
} from '../src/events/realtime.js'

function frame(sequence: number, type: string, data: unknown, subject?: string): string {
	return JSON.stringify({
		cursor: `stream-1:${sequence}`,
		deliveryId: `d-${sequence}`,
		event: { specversion: '1.0', id: `e-${sequence}`, type, data, ...(subject ? { subject } : {}) },
		appendedAt: '2026-09-27T00:00:00Z',
	})
}

/** A WebSocket the test drives: it records URLs, options, and sent frames. */
function fakeSockets() {
	const sockets: FakeSocket[] = []
	class FakeSocket implements WebSocketLike {
		onopen: ((event: unknown) => void) | null = null
		onmessage: ((event: { data: unknown }) => void) | null = null
		onclose: ((event: unknown) => void) | null = null
		onerror: ((event: unknown) => void) | null = null
		sent: string[] = []
		closed = false
		constructor(
			readonly url: string,
			readonly options?: unknown,
		) {
			sockets.push(this)
		}
		send(data: string) {
			this.sent.push(data)
		}
		close() {
			this.closed = true
		}
		push(text: string) {
			this.onmessage?.({ data: text })
		}
		drop() {
			this.onclose?.({})
		}
	}
	return { sockets, FakeSocket }
}

describe('subscribeRealtime', () => {
	test('parses pushed frames and errors', () => {
		const message = parseRealtimeFrame(frame(3, 'chat.message', { text: 'hi' }, 'ann'))
		expect(message).toEqual({
			cursor: 'stream-1:3',
			type: 'chat.message',
			data: { text: 'hi' },
			clientId: 'ann',
			publishTime: '2026-09-27T00:00:00Z',
		})
		expect(parseRealtimeFrame('{"op":"error","code":"cursor_expired"}')).toEqual({
			error: 'cursor_expired',
		})
		expect(parseRealtimeFrame('{"op":"ack"}')).toBeUndefined()
		expect(parseRealtimeFrame('not json')).toBeUndefined()
	})

	test('connects with the token in the URL, delivers messages, and resumes after a drop', async () => {
		const { sockets, FakeSocket } = fakeSockets()
		const got: RealtimeMessage[] = []
		const sub = subscribeRealtime(
			'wss://api.events.sylphx.com/v1/realtime/env_a.chat/subscribe',
			{ token: 'rtt_abc', webSocket: FakeSocket },
			(message) => got.push(message),
		)
		expect(sockets).toHaveLength(1)
		const first = new URL(sockets[0]!.url)
		expect(first.searchParams.get('token')).toBe('rtt_abc')
		expect(first.searchParams.get('after_cursor')).toBeNull()
		sockets[0]!.push(frame(1, 'chat.message', { text: 'one' }))
		sockets[0]!.push(frame(2, 'chat.message', { text: 'two' }))
		expect(got.map((m) => m.data)).toEqual([{ text: 'one' }, { text: 'two' }])
		expect(sub.cursor).toBe('stream-1:2')

		sub.updatePresence({ typing: true })
		expect(JSON.parse(sockets[0]!.sent[0]!)).toEqual({
			op: 'presence.update',
			data: { typing: true },
		})

		sockets[0]!.drop()
		await new Promise((resolve) => setTimeout(resolve, 600))
		expect(sockets).toHaveLength(2)
		expect(new URL(sockets[1]!.url).searchParams.get('after_cursor')).toBe('stream-1:2')

		sub.close()
		expect(sockets[1]!.closed).toBe(true)
		sockets[1]!.drop()
		await new Promise((resolve) => setTimeout(resolve, 600))
		expect(sockets).toHaveLength(2)
	})

	test('never puts a key in a URL', () => {
		const { FakeSocket, sockets } = fakeSockets()
		const g = globalThis as unknown as { WebSocket?: unknown }
		const saved = g.WebSocket
		g.WebSocket = FakeSocket
		try {
			expect(() =>
				subscribeRealtime(
					'wss://x.test/v1/realtime/c/subscribe',
					{ key: 'sylphx_sk_live_x' },
					() => {},
				),
			).toThrow()
		} finally {
			g.WebSocket = saved
		}
		subscribeRealtime(
			'wss://x.test/v1/realtime/c/subscribe',
			{ key: 'sylphx_sk_live_x', webSocket: FakeSocket, reconnect: false },
			() => {},
		)
		expect(sockets[0]!.url).not.toContain('sylphx_sk_live_x')
		expect(sockets[0]!.options).toEqual({ headers: { authorization: 'Bearer sylphx_sk_live_x' } })
	})

	test('reports errors and forgets an expired cursor', () => {
		const { sockets, FakeSocket } = fakeSockets()
		const errors: string[] = []
		const sub = subscribeRealtime(
			'wss://x.test/v1/realtime/c/subscribe',
			{
				token: 'rtt_x',
				afterCursor: 'stream-1:9',
				webSocket: FakeSocket,
				onError: (c) => errors.push(c),
				reconnect: false,
			},
			() => {},
		)
		expect(new URL(sockets[0]!.url).searchParams.get('after_cursor')).toBe('stream-1:9')
		sockets[0]!.push('{"op":"error","code":"cursor_expired"}')
		expect(errors).toEqual(['cursor_expired'])
		expect(sub.cursor).toBeUndefined()
	})
})
