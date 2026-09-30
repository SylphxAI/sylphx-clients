import { CacheCore, type CacheOptions } from './core.js'

/** Next.js 16 `cacheHandlers` entry (`'use cache'`). */
export interface UseCacheEntry {
	value: ReadableStream<Uint8Array>
	tags: string[]
	stale: number
	timestamp: number
	expire: number
	revalidate: number
}

interface Stored {
	value: Uint8Array
	stale: number
	expire: number
	revalidate: number
}

async function readAll(stream: ReadableStream<Uint8Array>): Promise<Uint8Array> {
	const reader = stream.getReader()
	const chunks: Uint8Array[] = []
	let size = 0
	for (;;) {
		const { done, value } = await reader.read()
		if (done) break
		chunks.push(value)
		size += value.length
	}
	const out = new Uint8Array(size)
	let at = 0
	for (const c of chunks) {
		out.set(c, at)
		at += c.length
	}
	return out
}

/**
 * Builds the handler object for Next.js 16 `cacheHandlers` (`'use cache'` and
 * `'use cache: remote'`), sharing entries and tag times through KV like the ISR handler.
 */
export function createUseCacheHandler(options: CacheOptions = {}) {
	let core: CacheCore | undefined
	const c = (): CacheCore => (core ??= new CacheCore(options))
	return {
		async get(cacheKey: string, softTags: string[] = []): Promise<UseCacheEntry | undefined> {
			c() // a missing build id is a setup error and throws here, not a cache miss
			try {
				const e = await c().lookup(cacheKey, softTags)
				if (e === undefined) return undefined
				const s = e.payload as Stored
				const now = c().now()
				if (now > e.timestamp + s.expire * 1000) return undefined
				const t = e.state
				// Past its expire time by a tag: gone. Otherwise a newer stale time means
				// serve it once and rebuild (revalidate -1).
				if (t.expired >= e.timestamp && t.expired <= now) return undefined
				const stale = t.stale >= e.timestamp
				const bytes = s.value
				return {
					value: new ReadableStream<Uint8Array>({
						start(controller) {
							controller.enqueue(bytes)
							controller.close()
						},
					}),
					tags: e.tags,
					stale: s.stale,
					timestamp: e.timestamp,
					expire: s.expire,
					revalidate: stale ? -1 : s.revalidate,
				}
			} catch (error) {
				c().onError(error, 'get')
				return undefined
			}
		},

		async set(cacheKey: string, pendingEntry: Promise<UseCacheEntry>): Promise<void> {
			c()
			try {
				const entry = await pendingEntry
				const value = await readAll(entry.value)
				const stored: Stored = {
					value,
					stale: entry.stale,
					expire: entry.expire,
					revalidate: entry.revalidate,
				}
				const ttlSeconds = Number.isFinite(entry.expire) ? Math.max(1, entry.expire) : undefined
				await c().write(cacheKey, stored, {
					tags: entry.tags,
					timestamp: entry.timestamp,
					ttlSeconds,
				})
			} catch (error) {
				c().onError(error, 'set')
			}
		},

		/** Tag times are read on every `get`, so there is nothing to sync ahead of a request. */
		async refreshTags(): Promise<void> {},

		async getExpiration(tags: string[]): Promise<number> {
			return (await c().tagState(tags)).stale
		},

		async updateTags(tags: string[], durations?: { expire?: number }): Promise<void> {
			await c().markTags(tags, durations?.expire)
		},
	}
}
