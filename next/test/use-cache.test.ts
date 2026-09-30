import { describe, expect, test } from 'bun:test'
import { createUseCacheHandler, type UseCacheEntry } from '../src/index.js'
import { clock, fakeKv, fakeObjects } from './fakes.js'

const stream = (s: string) =>
	new ReadableStream<Uint8Array>({
		start(c) {
			c.enqueue(new TextEncoder().encode(s))
			c.close()
		},
	})
const entry = (s: string, timestamp: number, over: Partial<UseCacheEntry> = {}): UseCacheEntry => ({
	value: stream(s),
	tags: ['t1'],
	stale: 30,
	timestamp,
	expire: 300,
	revalidate: 60,
	...over,
})
const text = async (e: UseCacheEntry | undefined) =>
	e ? await new Response(e.value).text() : undefined

function setup() {
	const c = clock()
	const kv = fakeKv(c)
	const objects = fakeObjects()
	const make = () =>
		createUseCacheHandler({
			kv,
			objects,
			prefix: 'u',
			buildId: 'b',
			now: c.now,
			blobThresholdBytes: 50,
		})
	return { c, make, objects }
}

describe('use cache handler', () => {
	test('set awaits the pending entry, then get streams it back', async () => {
		const { make, c } = setup()
		const h = make()
		await h.set('k', Promise.resolve(entry('hello', c.t)))
		const got = await h.get('k', [])
		expect(await text(got)).toBe('hello')
		expect(got?.tags).toEqual(['t1'])
		expect(got?.revalidate).toBe(60)
	})

	test('an entry past its expire time is gone', async () => {
		const { make, c } = setup()
		const h = make()
		await h.set('k', Promise.resolve(entry('x', c.t)))
		c.t += 301_000
		expect(await h.get('k', [])).toBeUndefined()
	})

	test('updateTags on one replica makes the entry a miss on another', async () => {
		const { make, c } = setup()
		const a = make()
		const b = make()
		await a.set('k', Promise.resolve(entry('x', c.t)))
		c.t += 1000
		expect(await b.getExpiration(['t1'])).toBe(0)
		await b.updateTags(['t1'])
		expect(await a.get('k', [])).toBeUndefined()
		expect(await a.getExpiration(['t1'])).toBe(c.t)
	})

	test('updateTags with an expire window serves the entry stale (revalidate -1) until it passes', async () => {
		const { make, c } = setup()
		const a = make()
		const b = make()
		await a.set('k', Promise.resolve(entry('x', c.t)))
		c.t += 1000
		await b.updateTags(['t1'], { expire: 10 })
		expect((await a.get('k', []))?.revalidate).toBe(-1)
		c.t += 11_000
		expect(await a.get('k', [])).toBeUndefined()
	})

	test('a soft tag revalidated after the write makes it stale; a rewrite is fresh again', async () => {
		const { make, c } = setup()
		const h = make()
		await h.set('k', Promise.resolve(entry('x', c.t)))
		c.t += 1000
		await h.updateTags(['_N_T_/blog'], { expire: 60 })
		expect((await h.get('k', ['_N_T_/blog']))?.revalidate).toBe(-1)
		c.t += 1000
		await h.set('k', Promise.resolve(entry('y', c.t)))
		const got = await h.get('k', ['_N_T_/blog'])
		expect(got?.revalidate).toBe(60)
		expect(await text(got)).toBe('y')
	})

	test('a large entry goes through the bucket', async () => {
		const { make, c, objects } = setup()
		const h = make()
		await h.set('k', Promise.resolve(entry('z'.repeat(1000), c.t)))
		expect(objects.rows.size).toBe(1)
		expect((await text(await make().get('k', [])))?.length).toBe(1000)
	})
})
