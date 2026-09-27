// @sylphx/sdk/config/flags: the local evaluator answers the shared vectors
// exactly as the platform does (crates/sylphx-bc-tenancy/tests/flags_golden.rs
// reads the same files), the client's two modes, and the OpenFeature
// providers.

import { describe, expect, test } from 'bun:test'
import {
	bucket,
	evaluate,
	Flags,
	fnv1a32,
	fromOpenFeature,
	SylphxServerProvider,
	SylphxWebProvider,
} from '../src/config/flags.js'
import type { ConfigFlagSpec, ConfigSegmentSpec, TargetingRule } from '../src/config.js'
import type { Sylphx } from '../src/index.js'
import evaluateVectors from './fixtures/evaluate.golden.json' with { type: 'json' }
import legacyVectors from './fixtures/flags.golden.json' with { type: 'json' }

type Wire = Record<string, unknown>

/** The fixtures are wire JSON (snake_case); the SDK's types are camelCase. */
function camel(v: unknown): unknown {
	if (Array.isArray(v)) return v.map(camel)
	if (v === null || typeof v !== 'object') return v
	return Object.fromEntries(
		Object.entries(v as Wire).map(([k, x]) => [
			k.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase()),
			// Values and attributes are the customer's JSON, never renamed.
			k === 'value' || k === 'default_value' || k === 'attributes' ? x : camel(x),
		]),
	)
}

describe('evaluation vectors', () => {
	test('every contract case answers as the platform does', () => {
		expect(evaluateVectors.cases.length).toBeGreaterThanOrEqual(30)
		for (const c of evaluateVectors.cases) {
			const spec = camel(c.spec) as ConfigFlagSpec
			const segments = new Map(
				Object.entries(c.segments ?? {}).map(([k, s]) => [k, camel(s) as ConfigSegmentSpec]),
			)
			const ctx = c.context as { user_id?: string; anonymous_id?: string; attributes?: Wire }
			const e = evaluate(
				c.flag,
				spec,
				{
					...(ctx.user_id ? { userId: ctx.user_id } : {}),
					...(ctx.anonymous_id ? { anonymousId: ctx.anonymous_id } : {}),
					...(ctx.attributes ? { attributes: ctx.attributes } : {}),
				},
				segments,
			)
			const want = c.expect as { value: unknown; reason: string; rule_index?: number }
			expect({ id: c.id, value: e.value, reason: e.reason, rule: e.ruleIndex }).toEqual({
				id: c.id,
				value: want.value,
				reason: want.reason,
				rule: want.rule_index,
			})
		}
	})

	test('the legacy hash, buckets and rollouts keep their recorded answers', () => {
		for (const c of legacyVectors.cases) {
			const i = c.input as Wire
			const o = c.output as Wire
			if (c.op === 'fnv1a32') {
				expect({ id: c.id, hash: fnv1a32(i.input as string) }).toEqual({
					id: c.id,
					hash: o.hash as number,
				})
			}
			if (c.op === 'consistent_bucket') {
				const b = bucket(i.flagKey as string, i.identifier as string, i.buckets as number)
				expect({ id: c.id, bucket: b }).toEqual({ id: c.id, bucket: o.bucket as number })
			}
			if (c.op === 'is_in_rollout') {
				const p = i.percentage as number
				const rule: TargetingRule = { value: true, percentage: p }
				const e = evaluate(
					i.flagKey as string,
					{ valueType: 'bool', defaultValue: false, rules: [rule] },
					{
						userId: i.identifier as string,
					},
				)
				expect({ id: c.id, in: e.value === true }).toEqual({ id: c.id, in: o.inRollout as boolean })
			}
		}
	})
})

/** A fake client: counts calls, answers from `flags`. */
function fakeClient(flags: Record<string, ConfigFlagSpec>) {
	const calls = { evaluate: 0, snapshot: 0, notModified: 0 }
	let etag = '"v1"'
	const client = {
		config: {
			configFlags: {
				async evaluate(req: { parent: string; context?: { userId?: string; attributes?: Wire } }) {
					calls.evaluate++
					return {
						evaluations: Object.entries(flags).map(([id, spec]) => {
							const e = evaluate(id, spec, {
								...(req.context?.userId ? { userId: req.context.userId } : {}),
								...(req.context?.attributes ? { attributes: req.context.attributes } : {}),
							})
							return { configFlag: id, value: e.value, reason: e.reason }
						}),
					}
				},
				async snapshot(req: { parent: string; ifNoneMatch?: string }) {
					calls.snapshot++
					if (req.ifNoneMatch === etag) {
						calls.notModified++
						return { etag, notModified: true, pollInterval: '30s' }
					}
					return {
						etag,
						pollInterval: '30s',
						configFlags: Object.entries(flags).map(([id, spec]) => ({
							name: `${req.parent}/config_flags/${id}`,
							spec,
						})),
						configSegments: [],
					}
				},
			},
		},
	}
	return {
		client: client as unknown as Sylphx,
		calls,
		bump(next: Record<string, ConfigFlagSpec>) {
			Object.assign(flags, next)
			etag = '"v2"'
		},
	}
}

const PARENT = 'orgs/org_a/projects/prj_a/envs/env_a'
const proOnly: ConfigFlagSpec = {
	valueType: 'bool',
	defaultValue: false,
	rules: [{ conditions: [{ attribute: 'plan', operator: 'equals', value: 'pro' }], value: true }],
}

describe('Flags', () => {
	test('remote asks once per context and caches for the TTL', async () => {
		let now = 0
		const { client, calls } = fakeClient({ 'new-checkout': proOnly })
		const flags = new Flags({ client, parent: PARENT, now: () => now })
		expect(
			await flags.isEnabled('new-checkout', { userId: 'u', attributes: { plan: 'pro' } }),
		).toBe(true)
		expect(
			await flags.isEnabled('new-checkout', { userId: 'u', attributes: { plan: 'pro' } }),
		).toBe(true)
		expect(
			await flags.isEnabled('new-checkout', { userId: 'u', attributes: { plan: 'free' } }),
		).toBe(false)
		expect(calls.evaluate).toBe(2)
		now = 5 * 60 * 1000
		await flags.isEnabled('new-checkout', { userId: 'u', attributes: { plan: 'pro' } })
		expect(calls.evaluate).toBe(3)
		expect(await flags.getValue('missing', {}, 'fallback')).toBe('fallback')
	})

	test('local downloads the ruleset, evaluates in process, and polls with the etag', async () => {
		let now = 0
		const fake = fakeClient({ 'new-checkout': proOnly })
		const flags = new Flags({ client: fake.client, parent: PARENT, mode: 'local', now: () => now })
		expect(await flags.isEnabled('new-checkout', { attributes: { plan: 'pro' } })).toBe(true)
		expect(await flags.isEnabled('new-checkout', { attributes: { plan: 'free' } })).toBe(false)
		expect(fake.calls).toEqual({ evaluate: 0, snapshot: 1, notModified: 0 })
		now = 30_000
		await flags.evaluate('new-checkout')
		expect(fake.calls.notModified).toBe(1)
		fake.bump({ 'new-checkout': { ...proOnly, enabled: false } })
		now = 60_000
		expect(await flags.isEnabled('new-checkout', { attributes: { plan: 'pro' } })).toBe(false)
		expect(fake.calls.snapshot).toBe(3)
	})
})

describe('OpenFeature', () => {
	test('targetingKey is the user and the rest are attributes', () => {
		expect(fromOpenFeature({ targetingKey: 'u1', plan: 'pro' })).toEqual({
			userId: 'u1',
			attributes: { plan: 'pro' },
		})
	})

	test('the server provider resolves typed values and reports errors', async () => {
		const { client } = fakeClient({
			'new-checkout': proOnly,
			layout: { valueType: 'string', defaultValue: 'classic', rules: [] },
		})
		const provider = new SylphxServerProvider(new Flags({ client, parent: PARENT, mode: 'local' }))
		expect(
			await provider.resolveBooleanEvaluation('new-checkout', false, {
				targetingKey: 'u',
				plan: 'pro',
			}),
		).toEqual({ value: true, reason: 'TARGETING_MATCH', variant: 'rule-0' })
		expect(await provider.resolveStringEvaluation('layout', 'x', {})).toEqual({
			value: 'classic',
			reason: 'DEFAULT',
		})
		expect((await provider.resolveNumberEvaluation('layout', 7, {})).errorCode).toBe(
			'TYPE_MISMATCH',
		)
		expect((await provider.resolveBooleanEvaluation('missing', true, {})).errorCode).toBe(
			'FLAG_NOT_FOUND',
		)
	})

	test('the web provider answers synchronously after one evaluation per context', async () => {
		const fake = fakeClient({ 'new-checkout': proOnly })
		const provider = new SylphxWebProvider(new Flags({ client: fake.client, parent: PARENT }))
		await provider.initialize({ targetingKey: 'u', plan: 'pro' })
		expect(provider.resolveBooleanEvaluation('new-checkout', false).value).toBe(true)
		await provider.onContextChange({}, { targetingKey: 'u', plan: 'free' })
		expect(provider.resolveBooleanEvaluation('new-checkout', true).value).toBe(false)
		expect(fake.calls.evaluate).toBe(2)
	})
})
