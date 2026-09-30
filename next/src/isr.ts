import { CacheCore, type CacheOptions } from './core.js'

/** What Next.js passes to `get`, in versions 14 to 16. */
export interface GetContext {
	kind?: string
	kindHint?: string
	tags?: string[]
	softTags?: string[]
}

/** What Next.js passes to `set`. */
export interface SetContext {
	tags?: string[]
	revalidate?: number | false
}

export interface CacheData {
	[field: string]: unknown
	kind?: string
	headers?: Record<string, string | string[] | undefined>
}

/** The entry Next.js reads back: the stored value and when it was written. */
export interface IsrEntry {
	lastModified: number
	value: unknown
}

/** Pages carry their tags in the `x-next-cache-tags` header (comma separated). */
function headerTags(data: CacheData | null): string[] {
	const h = data?.headers?.['x-next-cache-tags']
	const text = Array.isArray(h) ? h.join(',') : h
	return text ? text.split(',').filter(Boolean) : []
}

/**
 * Builds the class for Next.js `cacheHandler` (ISR, route handlers, fetch cache,
 * images). Entries and tag times live in KV (large bodies in a Bucket), so every
 * replica reads the same cache. `revalidateTag` writes a timestamp; `get` compares
 * it with the entry's write time, so no replica needs to be told.
 */
export function createCacheHandler(options: CacheOptions = {}) {
	let shared: CacheCore | undefined
	// Created when Next.js first builds the handler, so a missing build id stops the server at startup.
	const make = () => (shared ??= new CacheCore(options))
	return class SylphxCacheHandler {
		// Next.js constructs the handler with its own context object; nothing in it is needed.
		constructor(_nextContext?: unknown) {
			make()
		}

		async get(key: string, ctx: GetContext = {}): Promise<IsrEntry | null> {
			try {
				const entry = await make().lookup(key, [...(ctx.tags ?? []), ...(ctx.softTags ?? [])])
				if (entry === undefined) return null
				const data = entry.payload as CacheData
				const { stale } = entry.state
				if (stale >= entry.timestamp) {
					// A data-cache (fetch) entry must be refetched; a page may be served once
					// while it is rebuilt: lastModified -1 is Next.js's "stale" marker.
					const isFetch = data.kind === 'FETCH' || ctx.kind === 'FETCH' || ctx.kindHint === 'fetch'
					return isFetch ? null : { lastModified: -1, value: data }
				}
				return { lastModified: entry.timestamp, value: data }
			} catch (error) {
				make().onError(error, 'get')
				return null
			}
		}

		async set(key: string, data: CacheData | null, ctx: SetContext = {}): Promise<void> {
			try {
				if (data === null || data === undefined) return await make().remove(key)
				const tags = [...new Set([...(ctx.tags ?? []), ...headerTags(data)])]
				await make().write(key, data, { tags, timestamp: make().now() })
			} catch (error) {
				make().onError(error, 'set')
			}
		}

		async revalidateTag(tags: string | string[], durations?: { expire?: number }): Promise<void> {
			await make().markTags(Array.isArray(tags) ? tags : [tags], durations?.expire)
		}

		resetRequestCache(): void {}
	}
}
