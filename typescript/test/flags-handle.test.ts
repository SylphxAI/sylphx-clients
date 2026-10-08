import { expect, test } from 'bun:test'
import { Sylphx } from '../src/index.js'

const parent = 'orgs/org_a/projects/prj_a/envs/env_a'

test('typed handle keeps last evaluated values offline and defaults without a cache', async () => {
	let offline = false
	let now = 0
	const sx = new Sylphx({
		maxRetries: 0,
		fetch: async () => {
			if (offline) throw new TypeError('API unreachable')
			return new Response(
				JSON.stringify({
					evaluations: [
						{ config_flag: 'enabled', value: false, reason: 'default' },
						{ config_flag: 'layout', value: 'new', reason: 'default' },
						{ config_flag: 'limit', value: 0, reason: 'default' },
						{ config_flag: 'settings', value: { color: 'blue' }, reason: 'default' },
					],
				}),
				{ status: 200 },
			)
		},
	})
	const flags = sx.flags({ parent, context: { userId: 'alice' }, now: () => now })
	expect(await flags.bool('enabled', true)).toBe(false)
	expect(await flags.string('layout', 'old')).toBe('new')
	expect(await flags.number('limit', 10)).toBe(0)
	expect(await flags.json('settings', {})).toEqual({ color: 'blue' })
	offline = true
	now = 300_000
	expect(await flags.bool('enabled', true)).toBe(false)
	expect(await flags.string('layout', 'old')).toBe('new')
	expect(await flags.number('limit', 10)).toBe(0)
	expect(await flags.json('settings', {})).toEqual({ color: 'blue' })
	expect(await flags.string('missing', 'default')).toBe('default')
	for (const options of [
		{ parent, context: { userId: 'bob' } },
		{ parent: parent.replace('env_a', 'env_b'), context: { userId: 'alice' } },
	]) {
		const fresh = sx.flags(options)
		expect(await fresh.bool('enabled', true)).toBe(true)
		expect(await fresh.string('layout', 'old')).toBe('old')
		expect(await fresh.number('limit', 10)).toBe(10)
		expect(await fresh.json('settings', { offline: true })).toEqual({ offline: true })
	}
	expect(await flags.bool('layout', true)).toBe(true)
})
