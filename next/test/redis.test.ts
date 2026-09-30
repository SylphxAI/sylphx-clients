import { afterEach, describe, expect, test } from 'bun:test'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { createCacheHandler, type RedisLike, redisKv, resolveBuildId } from '../src/index.js'
import { clock, fakeKv } from './fakes.js'

/** A tiny in-memory Valkey speaking the four commands the adapter sends. */
function fakeValkey() {
	const rows = new Map<string, string>()
	const sent: string[][] = []
	const client: RedisLike = {
		async sendCommand(args) {
			sent.push(args)
			const [cmd, ...a] = args
			if (cmd === 'GET') return rows.get(a[0]!) ?? null
			if (cmd === 'MGET') return a.map((k) => rows.get(k) ?? null)
			if (cmd === 'SET') {
				rows.set(a[0]!, a[1]!)
				return 'OK'
			}
			if (cmd === 'DEL') return rows.delete(a[0]!) ? 1 : 0
			throw new Error(`unexpected ${cmd}`)
		},
	}
	return { client, rows, sent }
}

describe('redisKv', () => {
	test('maps get, mget, set with EX, and del', async () => {
		const v = fakeValkey()
		const kv = redisKv(v.client)
		await kv.set('a', new TextEncoder().encode('{"x":1}'), 90.2)
		expect(v.sent[0]).toEqual(['SET', 'a', '{"x":1}', 'EX', '91'])
		expect(new TextDecoder().decode(await kv.get('a'))).toBe('{"x":1}')
		const many = await kv.getMany(['a', 'b'])
		expect(many[1]).toBeUndefined()
		expect(v.sent.at(-1)).toEqual(['MGET', 'a', 'b'])
		await kv.delete('a')
		expect(await kv.get('a')).toBeUndefined()
	})

	test('the handler works on it, across two instances, and a steady-state get is one round trip', async () => {
		const v = fakeValkey()
		const c = clock()
		const mk = () =>
			new (createCacheHandler({ kv: redisKv(v.client), prefix: 'v', buildId: 'b', now: c.now }))()
		const a = mk()
		const b = mk()
		await a.set('/p', { kind: 'APP_PAGE', html: 'h', headers: { 'x-next-cache-tags': 'tg' } }, {})
		// b has not seen the entry's tags yet: the entry read plus one tag read.
		v.sent.length = 0
		expect(await b.get('/p', { softTags: ['_N_T_/p'] })).not.toBeNull()
		expect(v.sent.filter((s) => s[0] === 'MGET').length).toBe(2)
		// Now b knows them: one MGET carries the entry and every tag.
		v.sent.length = 0
		expect(await b.get('/p', { softTags: ['_N_T_/p'] })).not.toBeNull()
		expect(v.sent.length).toBe(1)
		expect(v.sent[0]![0]).toBe('MGET')
		expect(v.sent[0]!.length).toBe(1 + 1 + 2) // command + entry + tg + soft tag
		c.t += 10
		await a.revalidateTag('tg')
		expect((await b.get('/p', { softTags: ['_N_T_/p'] }))?.lastModified).toBe(-1)
	})
})

describe('build id', () => {
	const saved = { ...process.env }
	const dirs: string[] = []
	afterEach(() => {
		process.env = { ...saved }
		for (const d of dirs.splice(0)) rmSync(d, { recursive: true })
	})
	const clear = () => {
		for (const k of ['SYLPHX_IMAGE_DIGEST', 'SYLPHX_DEPLOYMENT_ID', 'SYLPHX_GIT_COMMIT_SHA'])
			delete process.env[k]
	}
	const distWith = (id?: string) => {
		const d = mkdtempSync(join(tmpdir(), 'sxnext-'))
		dirs.push(d)
		if (id) writeFileSync(join(d, 'BUILD_ID'), `${id}\n`)
		return d
	}

	test('falls back in order: image digest, deployment id, commit sha, BUILD_ID', () => {
		clear()
		const dist = distWith('nextid')
		expect(resolveBuildId(dist)).toBe('nextid')
		process.env.SYLPHX_GIT_COMMIT_SHA = 'sha'
		expect(resolveBuildId(dist)).toBe('sha')
		process.env.SYLPHX_DEPLOYMENT_ID = 'dep'
		expect(resolveBuildId(dist)).toBe('dep')
		process.env.SYLPHX_IMAGE_DIGEST = 'sha256:x'
		expect(resolveBuildId(dist)).toBe('sha256:x')
	})

	test('with none, building the handler throws a clear error (no shared default)', () => {
		clear()
		mkdirSync(join(tmpdir(), 'nope'), { recursive: true })
		const H = createCacheHandler({ kv: fakeKv(clock()), distDir: distWith() })
		expect(() => new H()).toThrow(/no build id/)
	})
})
