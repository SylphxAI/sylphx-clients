import type { KvStore, ObjectStore } from '../src/stores.js'

export interface Clock {
	t: number
	now: () => number
}
export const clock = (start = 1_000_000): Clock => {
	const c: Clock = { t: start, now: () => c.t }
	return c
}

/** An in-memory KV honouring TTLs against the test clock. */
export function fakeKv(c: Clock): KvStore & { rows: Map<string, { v: Uint8Array; exp: number }> } {
	const rows = new Map<string, { v: Uint8Array; exp: number }>()
	const live = (k: string) => {
		const r = rows.get(k)
		return r && (r.exp === 0 || r.exp > c.t) ? r.v : undefined
	}
	return {
		rows,
		get: async (k) => live(k),
		getMany: async (ks) => ks.map(live),
		set: async (k, v, ttl) => void rows.set(k, { v, exp: ttl ? c.t + ttl * 1000 : 0 }),
		delete: async (k) => void rows.delete(k),
	}
}

export function fakeObjects(): ObjectStore & { rows: Map<string, Uint8Array> } {
	const rows = new Map<string, Uint8Array>()
	return {
		rows,
		get: async (k) => rows.get(k),
		put: async (k, v) => void rows.set(k, v),
		delete: async (k) => void rows.delete(k),
	}
}
