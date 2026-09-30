// The two stores the handlers need, as small interfaces, and their adapters:
//   - KV: the namespace's own Valkey over TLS (default; `redisKv`), or the Data
//     API's item routes (`sdkKv`, an alternative for a namespace with no
//     reachable Valkey endpoint).
//   - Bucket: the Data API's object routes (`sdkObjects`).
// The interfaces keep the handlers testable without a network.

/** A byte key-value store with optional expiry. */
export interface KvStore {
	get(key: string): Promise<Uint8Array | undefined>
	/** One answer per key, in order; `undefined` for an absent or expired key. */
	getMany(keys: string[]): Promise<(Uint8Array | undefined)[]>
	/** `ttlSeconds` of 0 or undefined never expires. */
	set(key: string, value: Uint8Array, ttlSeconds?: number): Promise<void>
	delete(key: string): Promise<void>
}

/** A byte object store. */
export interface ObjectStore {
	get(key: string): Promise<Uint8Array | undefined>
	put(key: string, value: Uint8Array, contentType?: string): Promise<void>
	delete(key: string): Promise<void>
}

/** The part of the `@sylphx/sdk` client the adapters call. */
export interface SylphxDataClient {
	data: {
		kv: {
			get(r: { namespaceId: string; key: string }): Promise<{ value?: { value?: string } }>
			getMany(r: {
				namespaceId: string
				keys: string[]
			}): Promise<{ values?: { value?: string }[] }>
			put(r: {
				namespaceId: string
				key: string
				value?: string
				ttlSeconds?: string
			}): Promise<unknown>
			delete(r: { namespaceId: string; key: string }): Promise<unknown>
		}
		objects: {
			get(r: { bucketId: string; key: string }): Promise<{ body?: string }>
			put(r: {
				bucketId: string
				key: string
				body?: string
				contentType?: string
			}): Promise<unknown>
			delete(r: { bucketId: string; key: string }): Promise<unknown>
		}
	}
}

const b64 = (bytes: Uint8Array): string => Buffer.from(bytes).toString('base64')
const unb64 = (text: string): Uint8Array => new Uint8Array(Buffer.from(text, 'base64'))

function isNotFound(e: unknown): boolean {
	const err = e as { code?: string; status?: number } | null
	return err?.code === 'RESOURCE_NOT_FOUND' || err?.status === 404
}

/** Reads one value; an absent key is `undefined`, any other failure throws. */
async function orMissing<T>(call: () => Promise<T>): Promise<T | undefined> {
	try {
		return await call()
	} catch (e) {
		if (isNotFound(e)) return undefined
		throw e
	}
}

export function sdkKv(client: SylphxDataClient, namespaceId: string): KvStore {
	const kv = client.data.kv
	return {
		async get(key) {
			const r = await orMissing(() => kv.get({ namespaceId, key }))
			return r?.value?.value === undefined ? undefined : unb64(r.value.value)
		},
		async getMany(keys) {
			const out: (Uint8Array | undefined)[] = []
			// The API takes at most 1000 keys per call.
			for (let i = 0; i < keys.length; i += 1000) {
				const chunk = keys.slice(i, i + 1000)
				const r = await kv.getMany({ namespaceId, keys: chunk })
				for (let j = 0; j < chunk.length; j++) {
					const v = r.values?.[j]?.value
					out.push(v === undefined ? undefined : unb64(v))
				}
			}
			return out
		},
		async set(key, value, ttlSeconds) {
			const ttl =
				ttlSeconds !== undefined && ttlSeconds > 0
					? { ttlSeconds: String(Math.ceil(ttlSeconds)) }
					: {}
			await kv.put({ namespaceId, key, value: b64(value), ...ttl })
		},
		async delete(key) {
			await orMissing(() => kv.delete({ namespaceId, key }))
		},
	}
}

export function sdkObjects(client: SylphxDataClient, bucketId: string): ObjectStore {
	const objects = client.data.objects
	return {
		async get(key) {
			const r = await orMissing(() => objects.get({ bucketId, key }))
			return r?.body === undefined ? undefined : unb64(r.body)
		},
		async put(key, value, contentType = 'application/octet-stream') {
			await objects.put({ bucketId, key, body: b64(value), contentType })
		},
		async delete(key) {
			await orMissing(() => objects.delete({ bucketId, key }))
		},
	}
}

/** The part of a node-redis client (`redis` v5 or later) the adapter calls. */
export interface RedisLike {
	sendCommand(args: string[]): Promise<unknown>
}

const utf8 = new TextDecoder()
const utf8e = new TextEncoder()

/**
 * A KV store on the namespace's Valkey. Values are the handlers' own JSON text,
 * stored as UTF-8 strings. `getMany` is one MGET, so a lookup is one round trip.
 * Commands go through `sendCommand`, which is the same in every node-redis 5.x/6.x.
 */
export function redisKv(client: RedisLike): KvStore {
	const text = (v: unknown): Uint8Array | undefined =>
		typeof v === 'string' ? utf8e.encode(v) : v instanceof Uint8Array ? v : undefined
	return {
		async get(key) {
			return text(await client.sendCommand(['GET', key]))
		},
		async getMany(keys) {
			if (keys.length === 0) return []
			const rows = (await client.sendCommand(['MGET', ...keys])) as unknown[]
			return rows.map(text)
		},
		async set(key, value, ttlSeconds) {
			const args = ['SET', key, utf8.decode(value)]
			if (ttlSeconds !== undefined && ttlSeconds > 0) args.push('EX', String(Math.ceil(ttlSeconds)))
			await client.sendCommand(args)
		},
		async delete(key) {
			await client.sendCommand(['DEL', key])
		},
	}
}
