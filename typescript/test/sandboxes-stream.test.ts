// @sylphx/sdk/sandboxes/stream and stream-mock against the golden file
// contracts/fixtures/sandboxes-stream.json (the Rust SDK reads the same
// file): every frame round-trips through the TypeScript types, tolerated
// frames parse to their canonical form, invalid frames are refused, chunking
// is byte-identical, and the mock machine reproduces every session,
// including that a stream token admits only its own sandbox.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
	admitStatus,
	ChunkJoiner,
	type Clip,
	type ClipChunk,
	type ClipItem,
	checkClip,
	chunkClip,
	clipFrames,
	type FrameKind,
	type Holder,
	MAX_CLIP_IMAGE_BYTES,
	MAX_CLIP_TEXT_BYTES,
	MAX_FRAME_BYTES,
	parseFrame,
	parseInput,
	StreamFrameError,
} from '../src/sandboxes/stream.js'
import {
	type Channel,
	desktopLease,
	MockAdmitError,
	type MockClaims,
	type MockLeaseSpec,
	MockStreamHub,
	type SessionId,
} from '../src/sandboxes/stream-mock.js'

type Json = null | boolean | number | string | Json[] | { [k: string]: Json }
interface Step {
	admit?: string
	token?: string
	state?: string
	peer?: string
	ch?: Channel
	send?: Record<string, Json>
	send_raw?: string
	chunk?: number
	hub?: string
	lease?: string
	holder?: Holder
	label?: string
	items?: ClipItem[]
	expect: Json
}
interface Fixture {
	messages: Record<string, Json[]>
	tolerated: { kind: string; in: Json; canonical: Json }[]
	invalid: { kind: string; in: Json; why: string }[]
	chunking: { clip: Clip; slice_bytes: number; chunks: ClipChunk[] }
	hub: {
		leases: MockLeaseSpec[]
		tokens: Record<string, MockClaims>
	}
	sessions: { name: string; steps: Step[] }[]
}

const fx: Fixture = JSON.parse(
	readFileSync(new URL('../../contracts/fixtures/sandboxes-stream.json', import.meta.url), 'utf8'),
)

/** The fixture's snake_case groups name the SDK's frame kinds. */
const KIND: Record<string, FrameKind> = {
	signal_client: 'signalClient',
	signal_server: 'signalServer',
	control_client: 'controlClient',
	control_server: 'controlServer',
	input: 'input',
	pointer: 'pointer',
	clipboard: 'clipboard',
}

function roundTrip(kind: string, frame: Json): unknown {
	const parsed = parseFrame(KIND[kind]!, JSON.stringify(frame))
	return JSON.parse(JSON.stringify(parsed))
}

/** `{"$contains": [...]}` matches a string holding every part. */
function matches(expected: Json | undefined, actual: unknown): boolean {
	if (expected && typeof expected === 'object' && !Array.isArray(expected)) {
		const parts = expected.$contains
		if (Array.isArray(parts)) {
			return typeof actual === 'string' && parts.every((p) => actual.includes(p as string))
		}
		if (!actual || typeof actual !== 'object' || Array.isArray(actual)) return false
		const a = actual as Record<string, unknown>
		const keys = Object.keys(expected)
		return (
			keys.length === Object.keys(a).length &&
			keys.every((k) => k in a && matches(expected[k], a[k]))
		)
	}
	if (Array.isArray(expected)) {
		return (
			Array.isArray(actual) &&
			actual.length === expected.length &&
			expected.every((e, i) => matches(e, actual[i]))
		)
	}
	return expected === actual
}

describe('sandboxes stream frames', () => {
	test('every message round-trips', () => {
		for (const [kind, frames] of Object.entries(fx.messages)) {
			for (const frame of frames) {
				expect(roundTrip(kind, frame)).toEqual(frame)
			}
		}
	})

	test('tolerated frames parse to their canonical form', () => {
		for (const c of fx.tolerated) expect(roundTrip(c.kind, c.in)).toEqual(c.canonical)
	})

	test('invalid frames are refused', () => {
		for (const c of fx.invalid) {
			let err: unknown
			try {
				roundTrip(c.kind, c.in)
			} catch (e) {
				err = e
			}
			expect(err, c.why).toBeInstanceOf(StreamFrameError)
			expect((err as StreamFrameError).code).toBe('invalid_message')
		}
		expect(() => parseInput('{not json')).toThrow(StreamFrameError)
	})

	test('chunking is byte-identical and joins back', () => {
		const { clip, slice_bytes, chunks } = fx.chunking
		const got = chunkClip(clip, slice_bytes)
		expect(got).toEqual(chunks)
		const joiner = new ChunkJoiner()
		let out: Clip | null = null
		for (const c of got) out = joiner.push(c)
		expect(out).toEqual(clip)
	})

	test('large clips travel as chunks and limits hold', () => {
		const text = '粵語 Cantonese 🀄 '.repeat(8000)
		const clip: Clip = { t: 'clip', seq: 3, items: [{ mime: 'text/plain', data: text }] }
		expect(checkClip(clip)).toBeNull()
		const frames = clipFrames(clip)
		expect(frames.length).toBeGreaterThan(10)
		const joiner = new ChunkJoiner()
		let got: Clip | null = null
		for (const f of frames) {
			const s = JSON.stringify(f)
			expect(new TextEncoder().encode(s).length).toBeLessThanOrEqual(MAX_FRAME_BYTES)
			const back = parseFrame('clipboard', s)
			if (back.t === 'clip_chunk') got = joiner.push(back)
		}
		expect(got?.items[0]?.data).toBe(text)
		expect(clipFrames({ t: 'clip', seq: 1, items: [] })).toEqual([{ t: 'clip', seq: 1, items: [] }])

		const bigText = 'a'.repeat(MAX_CLIP_TEXT_BYTES + 1)
		expect(checkClip({ t: 'clip', seq: 1, items: [{ mime: 'text/plain', data: bigText }] })).toBe(
			'too_large',
		)
		const png = Buffer.alloc(MAX_CLIP_IMAGE_BYTES + 1).toString('base64')
		expect(checkClip({ t: 'clip', seq: 1, items: [{ mime: 'image/png', data: png }] })).toBe(
			'too_large',
		)
		expect(
			checkClip({
				t: 'clip',
				seq: 1,
				items: [
					{ mime: 'text/plain', data: 'a' },
					{ mime: 'text/plain', data: 'b' },
				],
			}),
		).toBe('invalid_message')
	})

	test('admission refusals carry the route status', () => {
		expect(admitStatus('UNAUTHENTICATED')).toBe(401)
		expect(admitStatus('NOT_FOUND')).toBe(404)
		expect(admitStatus('STALE_GENERATION')).toBe(409)
		expect(admitStatus('CONTROL_HELD')).toBe(409)
	})
})

class Run {
	hub = new MockStreamHub()
	tokens = new Map<string, string>()
	peers = new Map<string, SessionId>()

	constructor() {
		for (const l of fx.hub.leases) this.hub.addLease(l)
		for (const [name, claims] of Object.entries(fx.hub.tokens)) {
			this.tokens.set(name, this.hub.mintToken(claims))
		}
	}

	token(name: string): string {
		const n = name.startsWith('$') ? name.slice(1) : name
		return this.tokens.get(n) ?? n
	}

	peerName(id: SessionId): string {
		for (const [k, v] of this.peers) if (v === id) return k
		throw new Error(`no peer ${id}`)
	}

	outputs(): unknown[] {
		return this.hub
			.drain()
			.map((o) => ({ peer: this.peerName(o.session), ch: o.channel, msg: o.frame }))
	}

	state(lease: string): Record<string, unknown> {
		const l = this.hub.lease(lease)!
		return {
			probe: l.probeWhite ? 'white' : 'black',
			held_keys: l.heldKeys,
			held_buttons: l.heldButtons,
			typed: l.typed,
			pointer: l.pointer,
			clipboard: l.clipboard,
			combos: l.combos,
			controller: l.controller === null ? null : this.peerName(l.controller),
		}
	}

	step(at: string, s: Step): void {
		if (s.admit !== undefined) {
			let got: string
			try {
				this.peers.set(s.admit, this.hub.admit(this.token(s.token!)))
				got = 'ok'
			} catch (e) {
				if (!(e instanceof MockAdmitError)) throw e
				got = e.code
			}
			expect(got, at).toBe(s.expect as string)
			expect(this.outputs(), at).toEqual([])
			return
		}
		if (s.state !== undefined) {
			const state = this.state(s.state)
			for (const [k, v] of Object.entries(s.expect as Record<string, Json>)) {
				expect(matches(v, state[k]), `${at}: ${k} is ${JSON.stringify(state[k])}`).toBe(true)
			}
			return
		}
		if (s.peer !== undefined) {
			let text: string
			if (s.send_raw !== undefined) text = s.send_raw
			else {
				const msg: Record<string, unknown> = {
					...(s.chunk === undefined ? s.send! : fx.chunking.chunks[s.chunk]!),
				}
				if (msg.t === 'upgrade') msg.token = this.token(msg.token as string)
				text = JSON.stringify(msg)
			}
			this.hub.send(this.peers.get(s.peer)!, s.ch!, text)
		} else {
			const lease = s.lease ?? ''
			switch (s.hub) {
				case 'acquire':
					this.hub.acquire(lease, s.holder!, s.label!)
					break
				case 'release':
					this.hub.release(lease)
					break
				case 'bump_generation':
					this.hub.bumpGeneration(lease)
					break
				case 'end_lease':
					this.hub.endLease(lease)
					break
				case 'revoke':
					this.hub.revoke(this.token(s.token!))
					break
				case 'machine_clipboard':
					this.hub.setMachineClipboard(lease, s.items!)
					break
				default:
					throw new Error(`${at}: unknown hub step ${s.hub}`)
			}
		}
		const got = this.outputs()
		expect(
			matches(s.expect, got),
			`${at}\n  got ${JSON.stringify(got)}\n  want ${JSON.stringify(s.expect)}`,
		).toBe(true)
	}
}

describe('sandboxes stream mock peer', () => {
	test('reproduces every session of the fixture', () => {
		for (const session of fx.sessions) {
			const run = new Run()
			session.steps.forEach((s, i) => {
				run.step(`${session.name} step ${i}: ${JSON.stringify(s)}`, s)
			})
		}
	})

	// Tenant isolation, stated: a stream token only ever admits its own
	// sandbox. The token names one lease in one tenant; a session reaches
	// that lease only, and another lease's controller token cannot drive it.
	test('a stream token admits only its own sandbox', () => {
		const hub = new MockStreamHub()
		hub.addLease(desktopLease('lease_a', 'org_1', 'proj_1', 1))
		hub.addLease(desktopLease('lease_b', 'org_2', 'proj_2', 1))
		const viewer = { generation: 1, role: 'viewer' as const, epoch: 0 }
		const tokenB = hub.mintToken({ lease: 'lease_b', org: 'org_2', project: 'proj_2', ...viewer })
		const forged = hub.mintToken({ lease: 'lease_a', org: 'org_2', project: 'proj_2', ...viewer })
		expect(() => hub.admit(forged)).toThrow(new MockAdmitError('NOT_FOUND'))

		const peer = hub.admit(tokenB)
		hub.send(
			peer,
			'signal',
			JSON.stringify({
				t: 'hello',
				v: 1,
				client: { dpr: 1, w: 800, h: 600, touch: false },
				monitors: 'all',
			}),
		)
		hub.send(peer, 'signal', JSON.stringify({ t: 'answer', sdp: 'v=0\r\n' }))
		hub.acquire('lease_a', 'human', 'Ana')
		hub.acquire('lease_b', 'human', 'Bob')
		hub.drain()
		const ctlA = hub.mintToken({
			lease: 'lease_a',
			org: 'org_1',
			project: 'proj_1',
			generation: 1,
			role: 'controller',
			epoch: 1,
		})
		hub.send(peer, 'control', JSON.stringify({ t: 'upgrade', token: ctlA }))
		expect(hub.drain()).toEqual([
			{ session: peer, channel: 'control', frame: { t: 'error', code: 'unauthenticated' } },
		])
		hub.send(peer, 'input', JSON.stringify({ t: 'tx', text: 'x' }))
		expect(hub.lease('lease_a')!.typed).toBe('')
		expect(hub.lease('lease_b')!.typed).toBe('')
		expect(hub.lease('lease_a')!.controller).toBeNull()
	})
})
