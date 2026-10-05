// Sylphx Build contract v2: the short forms (`sx.build.run`, `image`,
// `cache.env`, `caches.purge`, `logs.read`) send the same wire call as their
// full-tree twins (docs/services/contracts/porcelain.md).

import { describe, expect, test } from 'bun:test'
import { type FetchLike, Sylphx } from '../src/index.js'

interface Sent {
	method: string
	url: URL
	body: unknown
}

function fake(body: unknown) {
	const sent: Sent[] = []
	const impl: FetchLike = async (input, init) => {
		sent.push({
			method: init?.method ?? 'GET',
			url: new URL(String(input)),
			body: init?.body == null ? undefined : JSON.parse(String(init.body)),
		})
		return new Response(JSON.stringify(body), {
			status: 200,
			headers: { 'sylphx-request-id': 'req_1' },
		})
	}
	const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: impl, baseUrl: 'https://api.test' })
	return { sx, sent }
}

const PROJECT = 'orgs/org_a/projects/prj_a'
const TREE = { rootDigest: `${'a'.repeat(64)}/1024` }
const OPERATION = { name: 'operations/op_a', done: false }

describe('Sylphx Build short forms', () => {
	test('run posts one command over a tree, like builds.run', async () => {
		const short = fake(OPERATION)
		await short.sx.build.run({
			parent: PROJECT,
			tree: TREE,
			command: ['cargo', 'test'],
			size: 'xlarge',
		})
		const full = fake(OPERATION)
		await full.sx.build.builds.run({
			parent: PROJECT,
			tree: TREE,
			command: ['cargo', 'test'],
			size: 'xlarge',
		})
		expect(short.sent[0]?.method).toBe('POST')
		expect(short.sent[0]?.url.pathname).toBe(`/v1/${PROJECT}/builds:run`)
		expect(short.sent[0]?.body).toEqual({
			tree: { root_digest: TREE.rootDigest },
			command: ['cargo', 'test'],
			size: 'xlarge',
		})
		expect(full.sent).toEqual(short.sent)
	})

	test('image posts to builds:buildImage', async () => {
		const { sx, sent } = fake(OPERATION)
		await sx.build.image({ parent: PROJECT, tree: TREE, image: { service: 'web' } })
		expect(sent[0]?.url.pathname).toBe(`/v1/${PROJECT}/builds:buildImage`)
		expect(sent[0]?.body).toEqual({
			tree: { root_digest: TREE.rootDigest },
			image: { service: 'web' },
		})
	})

	test('cache.env, caches.purge and logs.read address their Resource', async () => {
		const env = fake({ env: { TURBO_API: 'https://build-cache.sylphx.net' } })
		const got = await env.sx.build.cache.env({
			name: `${PROJECT}/build_caches/default`,
			access: 'read',
		})
		expect(env.sent[0]?.url.pathname).toBe(`/v1/${PROJECT}/build_caches/default:mintEnv`)
		expect(got.env).toEqual({ TURBO_API: 'https://build-cache.sylphx.net' })

		const purge = fake(OPERATION)
		await purge.sx.build.caches.purge({ name: `${PROJECT}/build_caches/default` })
		expect(purge.sent[0]?.url.pathname).toBe(`/v1/${PROJECT}/build_caches/default:purge`)

		const logs = fake({ lines: [{ stream: 'stdout', text: 'ok' }], next_page_token: 't2' })
		const page = await logs.sx.build.logs.read({
			name: `${PROJECT}/builds/bld_a`,
			pageToken: 't1',
			wait: '30s',
		})
		expect(logs.sent[0]?.method).toBe('GET')
		expect(logs.sent[0]?.url.pathname).toBe(`/v1/${PROJECT}/builds/bld_a:readLogs`)
		expect(logs.sent[0]?.url.searchParams.get('page_token')).toBe('t1')
		expect(logs.sent[0]?.url.searchParams.get('wait')).toBe('30s')
		expect(page).toEqual({ lines: [{ stream: 'stdout', text: 'ok' }], nextPageToken: 't2' })
	})
})
