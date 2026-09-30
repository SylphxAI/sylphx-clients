import { describe, expect, test } from 'bun:test'
import { createCacheHandler } from '../src/index.js'
import { clock, fakeKv, fakeObjects } from './fakes.js'

const page = (html: string) => ({
	kind: 'APP_PAGE',
	html,
	rscData: Buffer.from('rsc:' + html),
	headers: { 'x-next-cache-tags': '_N_T_/blog,post-1' },
	segmentData: new Map([['/a', Buffer.from('seg')]]),
	status: 200,
})

function setup(opts: { threshold?: number } = {}) {
	const c = clock()
	const kv = fakeKv(c)
	const objects = fakeObjects()
	const make = () => {
		const H = createCacheHandler({
			kv,
			objects,
			prefix: 'test',
			buildId: 'b1',
			now: c.now,
			blobThresholdBytes: opts.threshold,
			onError: (e) => {
				throw e
			},
		})
		return new H({})
	}
	return { c, kv, objects, make }
}

describe('isr cache handler', () => {
	test('set then get returns the value with Buffers and Maps intact', async () => {
		const { make, c } = setup()
		const h = make()
		await h.set('/blog', page('<p>hi</p>'), { tags: ['x'] })
		const got = await h.get('/blog', { kind: 'APP_PAGE' })
		expect(got?.lastModified).toBe(c.t)
		const v = got?.value as ReturnType<typeof page>
		expect(v.html).toBe('<p>hi</p>')
		expect(Buffer.isBuffer(v.rscData)).toBe(true)
		expect(v.rscData.toString()).toBe('rsc:<p>hi</p>')
		expect(v.segmentData.get('/a')?.toString()).toBe('seg')
	})

	test('an unknown key is a miss; a null set removes the entry', async () => {
		const { make } = setup()
		const h = make()
		expect(await h.get('/nope')).toBeNull()
		await h.set('/a', page('a'), {})
		await h.set('/a', null, {})
		expect(await h.get('/a')).toBeNull()
	})

	test('a second replica sees the entry and a tag revalidation made on the first', async () => {
		const { make, c } = setup()
		const a = make()
		const b = make()
		await a.set('/blog/1', page('v1'), { tags: [] })
		expect((await b.get('/blog/1'))?.lastModified).toBe(c.t)
		c.t += 5000
		await a.revalidateTag('post-1') // tag comes from the x-next-cache-tags header
		const stale = await b.get('/blog/1')
		expect(stale?.lastModified).toBe(-1) // served once while rebuilt
		c.t += 1000
		await b.set('/blog/1', page('v2'), {})
		const fresh = await a.get('/blog/1')
		expect(fresh?.lastModified).toBe(c.t)
		expect((fresh?.value as ReturnType<typeof page>).html).toBe('v2')
	})

	test('a revalidated fetch entry is a miss, and soft tags from get count', async () => {
		const { make, c } = setup()
		const a = make()
		const b = make()
		await a.set('f1', { kind: 'FETCH', data: { body: 'x' }, revalidate: 60 }, { tags: ['t1'] })
		c.t += 10
		await b.revalidateTag(['t1'])
		expect(await a.get('f1', { kind: 'FETCH' })).toBeNull()
		await a.set('/p', page('p'), { tags: [] })
		c.t += 10
		await b.revalidateTag('_N_T_/soft')
		expect((await a.get('/p', { softTags: ['_N_T_/soft'] }))?.lastModified).toBe(-1)
		expect((await a.get('/p'))?.lastModified).not.toBe(-1)
	})

	test('entries expire after the retention time', async () => {
		const c = clock()
		const H = createCacheHandler({
			kv: fakeKv(c),
			prefix: 't',
			buildId: 'b',
			now: c.now,
			retentionSeconds: 60,
		})
		const h = new H()
		await h.set('/p', page('p'), {})
		c.t += 59_000
		expect(await h.get('/p')).not.toBeNull()
		c.t += 2_000
		expect(await h.get('/p')).toBeNull()
	})

	test('a different build never reads the entry', async () => {
		const { kv, c } = setup()
		const mk = (buildId: string) =>
			new (createCacheHandler({ kv, prefix: 't', buildId, now: c.now }))()
		await mk('one').set('/p', page('p'), {})
		expect(await mk('two').get('/p')).toBeNull()
		expect(await mk('one').get('/p')).not.toBeNull()
	})

	test('a large body goes to the bucket, is read back, and the old body is removed on rewrite', async () => {
		const { make, objects, kv } = setup({ threshold: 100 })
		const h = make()
		const big = 'x'.repeat(5000)
		await h.set('/big', page(big), {})
		expect(objects.rows.size).toBe(1)
		const record = [...kv.rows.values()].find((r) =>
			new TextDecoder().decode(r.v).includes('"blob"'),
		)
		expect(new TextDecoder().decode(record?.v)).not.toContain(big)
		expect(((await make().get('/big'))?.value as ReturnType<typeof page>).html).toBe(big)
		await h.set('/big', page(big + 'y'), {})
		expect(objects.rows.size).toBe(1)
		await h.set('/small', { kind: 'FETCH', data: { body: 's' } }, {})
		expect(objects.rows.size).toBe(1)
	})

	test('a store failure is a miss on get and is reported, not thrown, on set', async () => {
		const c = clock()
		const kv = fakeKv(c)
		kv.getMany = async () => {
			throw new Error('down')
		}
		kv.set = async () => {
			throw new Error('down')
		}
		const errors: string[] = []
		const H = createCacheHandler({
			kv,
			buildId: 'b',
			now: c.now,
			onError: (_e, op) => void errors.push(op),
		})
		const h = new H()
		expect(await h.get('/p')).toBeNull()
		await h.set('/p', page('p'), {})
		expect(errors).toEqual(['get', 'set'])
	})

	test('revalidateTag surfaces a store failure', async () => {
		const c = clock()
		const kv = fakeKv(c)
		kv.set = async () => {
			throw new Error('down')
		}
		const h = new (createCacheHandler({ kv, buildId: 'b', now: c.now }))()
		await expect(h.revalidateTag('t')).rejects.toThrow('down')
	})
})
