import { createHash, randomUUID } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fromBytes, toBytes } from './codec.js'
import {
	type KvStore,
	type ObjectStore,
	type RedisLike,
	redisKv,
	type SylphxDataClient,
	sdkKv,
	sdkObjects,
} from './stores.js'

export interface CacheOptions {
	/**
	 * The KV store. Default: the namespace's Valkey, at `REDIS_URL` (or `CACHE_URL`), which the platform
	 * injects when a namespace is bound; else the URL from `connect` on `NEXT_CACHE_KV_NAME`. Needs the `redis` package.
	 */
	kv?: KvStore
	/** Where bodies larger than `blobThresholdBytes` go. Default: the Bucket named by `NEXT_CACHE_BUCKET` (Data API); without one every body stays in KV. */
	objects?: ObjectStore
	/** A ready `@sylphx/sdk` client for the Bucket (and `connect`). Default: a new `Sylphx()`. */
	client?: SdkClient
	/** Start of every key. Default `next:<SYLPHX_SERVICE_NAME or app>`. Give each app in a shared namespace its own. */
	prefix?: string
	/**
	 * Entries of a different build are never read. Default, first that exists: `SYLPHX_IMAGE_DIGEST`,
	 * `SYLPHX_DEPLOYMENT_ID`, `SYLPHX_GIT_COMMIT_SHA`, the build id in `<distDir>/BUILD_ID`. None is an error.
	 */
	buildId?: string
	/** Bodies whose stored size is above this many bytes go to the Bucket. Default 262144 (256 KiB). */
	blobThresholdBytes?: number
	/** The longest an entry is kept, in seconds. Default 30 days. */
	retentionSeconds?: number
	/** Next.js output directory, where `BUILD_ID` is read. Default `.next` under the working directory. */
	distDir?: string
	/** Called when a cache read or write fails and the cache falls back to a miss. Default `console.warn`. */
	onError?: (error: unknown, operation: string) => void
	/** Clock in milliseconds; tests replace it. */
	now?: () => number
}

/** The state of a tag: entries older than `stale` are stale; older than `expired` (once passed) are gone. */
export interface TagState {
	stale: number
	expired: number
}

interface Stored {
	v: 1
	key: string
	tags: string[]
	timestamp: number
	/** JSON text of the encoded payload, when kept in KV. */
	payload?: string
	/** Bucket object key of the payload, when large. */
	blob?: string
}

const sha = (s: string): string => createHash('sha256').update(s).digest('hex')
const DAY = 86_400
const text = new TextEncoder()
const untext = new TextDecoder()

export class CacheCore {
	readonly prefix: string
	readonly buildId: string
	readonly threshold: number
	readonly retention: number
	readonly now: () => number
	readonly onError: (error: unknown, operation: string) => void
	private stores: Promise<{ kv: KvStore; objects: ObjectStore | undefined }> | undefined

	constructor(private readonly options: CacheOptions = {}) {
		const env = process.env
		this.prefix = options.prefix ?? `next:${env.SYLPHX_SERVICE_NAME ?? 'app'}`
		this.buildId = options.buildId ?? resolveBuildId(options.distDir)
		this.threshold = options.blobThresholdBytes ?? 262_144
		this.retention = options.retentionSeconds ?? 30 * DAY
		this.now = options.now ?? Date.now
		this.onError = options.onError ?? ((e, op) => console.warn(`@sylphx/next: ${op} failed:`, e))
	}

	private resolve() {
		this.stores ??= (async () => {
			const o = this.options
			const env = process.env
			const bucket = env.NEXT_CACHE_BUCKET
			let client = o.client
			const sdk = async () => (client ??= await defaultClient())
			let kv = o.kv
			if (kv === undefined) {
				let url = env.REDIS_URL ?? env.CACHE_URL
				if (!url && env.NEXT_CACHE_KV_NAME) {
					const r = await (await sdk()).data.kvNamespaces.connect({ name: env.NEXT_CACHE_KV_NAME })
					url = r.connectionInfo?.uri
				}
				if (!url)
					throw new Error(
						'@sylphx/next: no KV endpoint. Bind a KV namespace (REDIS_URL), set NEXT_CACHE_KV_NAME, or pass `kv`',
					)
				kv = redisKv(await connectRedis(url))
			}
			const objects = o.objects ?? (bucket ? sdkObjects(await sdk(), bucket) : undefined)
			return { kv, objects }
		})()
		// A failed setup is retried by the next call, not remembered.
		const pending = this.stores
		pending.catch(() => {
			if (this.stores === pending) this.stores = undefined
		})
		return pending
	}

	private entryKey = (key: string): string => `${this.prefix}:${this.buildId}:e:${sha(key)}`
	private tagKey = (tag: string): string => `${this.prefix}:t:${sha(tag)}`
	private blobKey = (): string => `${this.prefix}/${this.buildId}/${randomUUID()}`

	/** Tags each key had when this replica last saw it, so a lookup can ask for them up front. */
	private knownTags = new Map<string, string[]>()

	private remember(key: string, tags: string[]): void {
		this.knownTags.delete(key)
		this.knownTags.set(key, tags)
		if (this.knownTags.size > 10_000)
			this.knownTags.delete(this.knownTags.keys().next().value as string)
	}

	/**
	 * The entry and the newest tag times for its tags plus `extraTags`, or `undefined` when the entry is
	 * absent, unreadable or for another key. One round trip in the steady state: the entry and the tags
	 * this replica already knows for the key are read together (MGET); only an entry whose tags
	 * changed since needs a second read.
	 */
	async lookup(
		key: string,
		extraTags: string[] = [],
	): Promise<{ payload: unknown; tags: string[]; timestamp: number; state: TagState } | undefined> {
		const { kv, objects } = await this.resolve()
		const asked = [...new Set([...(this.knownTags.get(key) ?? []), ...extraTags])]
		const [raw, ...tagRows] = await kv.getMany([this.entryKey(key), ...asked.map(this.tagKey)])
		if (raw === undefined) return undefined
		const rec = JSON.parse(untext.decode(raw)) as Stored
		if (rec.v !== 1 || rec.key !== key) return undefined
		this.remember(key, rec.tags)
		const state = foldTags(tagRows)
		const missing = rec.tags.filter((t) => !asked.includes(t))
		if (missing.length > 0) {
			const more = foldTags(await kv.getMany(missing.map(this.tagKey)))
			state.stale = Math.max(state.stale, more.stale)
			state.expired = Math.max(state.expired, more.expired)
		}
		let payloadBytes: Uint8Array | undefined
		if (rec.blob !== undefined) payloadBytes = await objects?.get(rec.blob)
		else if (rec.payload !== undefined) payloadBytes = text.encode(rec.payload)
		if (payloadBytes === undefined) return undefined
		return { payload: fromBytes(payloadBytes), tags: rec.tags, timestamp: rec.timestamp, state }
	}

	async write(
		key: string,
		payload: unknown,
		meta: { tags: string[]; timestamp: number; ttlSeconds?: number },
	): Promise<void> {
		const { kv, objects } = await this.resolve()
		const bytes = toBytes(payload)
		const rec: Stored = { v: 1, key, tags: meta.tags, timestamp: meta.timestamp }
		const large = objects !== undefined && bytes.length > this.threshold
		if (large) {
			rec.blob = this.blobKey()
			await objects.put(rec.blob, bytes, 'application/json')
		} else rec.payload = untext.decode(bytes)
		this.remember(key, meta.tags)
		const ttl = Math.min(meta.ttlSeconds ?? this.retention, this.retention)
		const entryKey = this.entryKey(key)
		const previous = objects !== undefined ? await this.previousBlob(kv, entryKey) : undefined
		await kv.set(entryKey, text.encode(JSON.stringify(rec)), ttl)
		// The old body is unreachable now; removal is best effort.
		if (previous !== undefined && previous !== rec.blob)
			await objects?.delete(previous).catch(() => {})
	}

	private async previousBlob(kv: KvStore, entryKey: string): Promise<string | undefined> {
		try {
			const raw = await kv.get(entryKey)
			return raw === undefined ? undefined : (JSON.parse(untext.decode(raw)) as Stored).blob
		} catch {
			return undefined
		}
	}

	async remove(key: string): Promise<void> {
		const { kv } = await this.resolve()
		await kv.delete(this.entryKey(key))
	}

	/** The newest `stale` and `expired` times across `tags`; 0 when none was ever revalidated. */
	async tagState(tags: string[]): Promise<TagState> {
		if (tags.length === 0) return { stale: 0, expired: 0 }
		const { kv } = await this.resolve()
		return foldTags(await kv.getMany([...new Set(tags)].map(this.tagKey)))
	}

	/**
	 * Marks tags revalidated now: entries written before now are stale at once.
	 * With `expireSeconds` they may still be served until that long from now.
	 */
	async markTags(tags: string[], expireSeconds?: number): Promise<void> {
		if (tags.length === 0) return
		const { kv } = await this.resolve()
		const at = this.now()
		const state: TagState = { stale: at, expired: at + (expireSeconds ?? 0) * 1000 }
		const bytes = text.encode(JSON.stringify(state))
		// A tag row must outlive every entry it can affect.
		const ttl = this.retention + DAY
		await Promise.all([...new Set(tags)].map((t) => kv.set(this.tagKey(t), bytes, ttl)))
	}
}

function foldTags(rows: (Uint8Array | undefined)[]): TagState {
	const state: TagState = { stale: 0, expired: 0 }
	for (const raw of rows) {
		if (raw === undefined) continue
		const t = JSON.parse(untext.decode(raw)) as TagState
		state.stale = Math.max(state.stale, t.stale)
		state.expired = Math.max(state.expired, t.expired)
	}
	return state
}

/**
 * The identity of this build. A shared id would serve old HTML that points at
 * chunks a newer deploy removed, so there is no default.
 */
export function resolveBuildId(distDir = join(process.cwd(), '.next')): string {
	const env = process.env
	const fromEnv = env.SYLPHX_IMAGE_DIGEST || env.SYLPHX_DEPLOYMENT_ID || env.SYLPHX_GIT_COMMIT_SHA
	if (fromEnv) return fromEnv
	try {
		const id = readFileSync(join(distDir, 'BUILD_ID'), 'utf8').trim()
		if (id) return id
	} catch {}
	throw new Error(
		'@sylphx/next: no build id. Expected SYLPHX_IMAGE_DIGEST, SYLPHX_DEPLOYMENT_ID, SYLPHX_GIT_COMMIT_SHA or .next/BUILD_ID; pass `buildId`',
	)
}

async function importPackage<T>(name: string, hint: string): Promise<T> {
	try {
		return (await import(/* webpackIgnore: true */ name)) as T
	} catch {
		throw new Error(`@sylphx/next: ${hint}`)
	}
}

async function connectRedis(url: string): Promise<RedisLike> {
	const { createClient } = await importPackage<{
		createClient: (o: { url: string }) => RedisLike & {
			connect(): Promise<unknown>
			on(e: string, f: (x: unknown) => void): unknown
		}
	}>('redis', 'install the `redis` package (npm install redis), or pass `kv`')
	const client = createClient({ url })
	// A connection error must not crash the server; commands fail and the cache falls back to a miss.
	client.on('error', () => {})
	await client.connect()
	return client
}

export interface SdkClient extends SylphxDataClient {
	data: SylphxDataClient['data'] & {
		kvNamespaces: { connect(r: { name: string }): Promise<{ connectionInfo?: { uri?: string } }> }
	}
}

async function defaultClient(): Promise<SdkClient> {
	const mod = await importPackage<{ Sylphx: new (o: { apiKey?: string }) => SdkClient }>(
		'@sylphx/sdk',
		'install @sylphx/sdk (npm install @sylphx/sdk), or pass `objects`',
	)
	const env = process.env
	return new mod.Sylphx({ apiKey: env.SYLPHX_API_KEY ?? env.SYLPHX_SECRET_KEY })
}
