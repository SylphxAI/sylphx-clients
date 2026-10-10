/**
 * Contract test — end-user API keys (Auth), slice A: the store, the contract
 * and the lifecycle.
 *
 * Runs the golden vectors of `contracts/fixtures/end-user-keys.json` and the
 * lifecycle through the reference service (`src/auth/end-user-keys-mock.ts`).
 * The real Auth service and the Rust format functions must pass the same rows.
 * Contract: docs/services/contracts/end-user-keys.md.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import {
	assembleToken,
	checksum6,
	END_USER_KEY_SCOPES,
	parseToken,
	secretSha256Hex,
	validateApiKeysPolicy,
} from '../src/auth/end-user-keys.js'
import {
	EndUserKeyError,
	MockEndUserKeys,
	MockResourceApi,
} from '../src/auth/end-user-keys-mock.js'

interface Fixtures {
	scopes: { verify: string; write: string }
	tokens: {
		prefix: string
		mode: 'live' | 'test'
		secret: string
		checksum: string
		token: string
		sha256: string
		start: string
		last4: string
	}[]
	badTokens: { name: string; token: string; why: string }[]
	policy: { name: string; policy: Record<string, unknown>; valid: boolean; errorField?: string }[]
}

const fixtures = JSON.parse(
	readFileSync(new URL('../../../contracts/fixtures/end-user-keys.json', import.meta.url), 'utf8'),
) as Fixtures

const PROD = 'orgs/o1/projects/p1/envs/production'
const PREVIEW = 'orgs/o1/projects/p1/envs/preview'
const OTHER_PROD = 'orgs/o2/projects/p9/envs/production'
const USER = 'end_users/usr_01m3jzg7fpek2ayen9cq4bn2ef'
const USER_2 = 'end_users/usr_01m3jzg7fpek2ayen9cq4bn2eg'

function setup(opts: { start?: number } = {}) {
	let now = opts.start ?? Date.parse('2026-10-04T12:00:00Z')
	const svc = new MockEndUserKeys({ now: () => now })
	for (const parent of [PROD, PREVIEW, OTHER_PROD]) {
		svc.addEnvironment(parent, parent.endsWith('production') ? 'production' : 'preview')
		svc.setPolicy(parent, { prefix: 'kk', scopes: ['read', 'stream', 'webhooks'] })
	}
	return {
		svc,
		advance: (ms: number) => {
			now += ms
		},
		clock: () => now,
	}
}

function codeOf(fn: () => unknown): string {
	try {
		fn()
	} catch (e) {
		if (e instanceof EndUserKeyError) return e.code
		throw e
	}
	return 'NO_ERROR'
}

describe('token grammar and golden vectors', () => {
	for (const v of fixtures.tokens) {
		test(`vector ${v.start}`, () => {
			expect(checksum6(`${v.prefix}_${v.mode}_${v.secret}`)).toBe(v.checksum)
			expect(assembleToken(v.prefix, v.mode, v.secret)).toBe(v.token)
			expect(secretSha256Hex(v.token)).toBe(v.sha256)
			const parsed = parseToken(v.token)
			expect(parsed).toEqual({
				prefix: v.prefix,
				mode: v.mode,
				secret: v.secret,
				start: v.start,
				last4: v.last4,
			})
		})
	}

	for (const b of fixtures.badTokens) {
		test(`refuses ${b.name}`, () => {
			expect(parseToken(b.token), b.why).toBeNull()
		})
	}

	test('the two scopes are the ones the Access registry will carry', () => {
		expect(END_USER_KEY_SCOPES.verify).toBe(fixtures.scopes.verify)
		expect(END_USER_KEY_SCOPES.write).toBe(fixtures.scopes.write)
	})
})

describe('AuthConfig.api_keys policy', () => {
	for (const c of fixtures.policy) {
		test(`${c.valid ? 'accepts' : 'refuses'} ${c.name}`, () => {
			const errors = validateApiKeysPolicy(c.policy)
			if (c.valid) expect(errors).toEqual([])
			else expect(errors[0]?.field).toBe(c.errorField)
		})
	}
})

describe('create', () => {
	test('is refused until the environment has a policy', () => {
		const svc = new MockEndUserKeys()
		svc.addEnvironment(PROD, 'production')
		expect(
			codeOf(() => svc.create(PROD, { subject: { end_user: USER }, display_name: 'ci' })),
		).toBe('FAILED_PRECONDITION')
	})

	test('returns the secret once, in the app prefix, live in production and test elsewhere', () => {
		const { svc } = setup()
		const live = svc.create(PROD, {
			subject: { end_user: USER },
			display_name: 'ci',
			scopes: ['read'],
		})
		const test_ = svc.create(PREVIEW, {
			subject: { end_user: USER },
			display_name: 'ci',
			scopes: ['read'],
		})
		expect(parseToken(live.secret)?.mode).toBe('live')
		expect(parseToken(test_.secret)?.mode).toBe('test')
		expect(live.secret.startsWith('kk_live_')).toBe(true)
		expect(live.key.uid.startsWith('euk_')).toBe(true)
		expect(live.key.name).toBe(`${PROD}/end_user_keys/${live.key.uid}`)
		expect(live.key.state).toBe('ACTIVE')
		expect(live.key.start).toBe(parseToken(live.secret)?.start as string)
	})

	test('a secret never appears in any response but create and roll', () => {
		const { svc } = setup()
		const { key, secret } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'ci' })
		const digest = secretSha256Hex(secret)
		const others = [
			svc.get(PROD, key.uid),
			svc.list(PROD, {}),
			svc.update(PROD, key.uid, { display_name: 'renamed' }),
			svc.revoke(PROD, key.uid, 'rotated by hand'),
			svc.listEvents(PROD, key.uid),
			svc.verify(PROD, { key: secret }),
		]
		for (const response of others) {
			const text = JSON.stringify(response)
			expect(text).not.toContain(secret)
			expect(text).not.toContain(digest)
		}
		expect(JSON.stringify(svc.verify(PROD, { key: secret }))).not.toContain('"key"')
	})

	test('subject is exactly one of end user, customer organization or external id', () => {
		const { svc } = setup()
		expect(
			codeOf(() =>
				svc.create(PROD, {
					subject: { end_user: USER, external_id: 'x' } as never,
					display_name: 'a',
				}),
			),
		).toBe('INVALID_ARGUMENT')
		expect(codeOf(() => svc.create(PROD, { subject: {} as never, display_name: 'a' }))).toBe(
			'INVALID_ARGUMENT',
		)
		expect(
			codeOf(() =>
				svc.create(PROD, { subject: { external_id: 'x'.repeat(129) }, display_name: 'a' }),
			),
		).toBe('INVALID_ARGUMENT')
		expect(
			codeOf(() =>
				svc.create(PROD, {
					subject: { customer_organization: 'customer_organizations/cor_01' },
					display_name: 'a',
				}),
			),
		).toBe('NO_ERROR')
		expect(
			codeOf(() => svc.create(PROD, { subject: { external_id: 'acct-42' }, display_name: 'a' })),
		).toBe('NO_ERROR')
	})

	test('scopes are a subset of the policy vocabulary; expiry and metadata are bounded', () => {
		const { svc, clock } = setup()
		svc.setPolicy(PROD, {
			prefix: 'kk',
			scopes: ['read'],
			max_lifetime: '86400s',
			max_keys_per_subject: 2,
		})
		const create = (r: object) => () =>
			svc.create(PROD, { subject: { end_user: USER }, display_name: 'a', ...r })
		expect(codeOf(create({ scopes: ['admin'] }))).toBe('INVALID_ARGUMENT')
		expect(codeOf(create({ expire_time: new Date(clock() + 2 * 86400_000).toISOString() }))).toBe(
			'INVALID_ARGUMENT',
		)
		expect(codeOf(create({ expire_time: new Date(clock() + 3600_000).toISOString() }))).toBe(
			'NO_ERROR',
		)
		expect(codeOf(create({ metadata: { blob: 'x'.repeat(5000) } }))).toBe('INVALID_ARGUMENT')
		expect(codeOf(create({}))).toBe('NO_ERROR')
		expect(codeOf(create({}))).toBe('RESOURCE_EXHAUSTED')
	})

	test('a revoked key still counts toward the subject ceiling only while it is live', () => {
		const { svc } = setup()
		svc.setPolicy(PROD, { prefix: 'kk', max_keys_per_subject: 1 })
		const a = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		expect(codeOf(() => svc.create(PROD, { subject: { end_user: USER }, display_name: 'b' }))).toBe(
			'RESOURCE_EXHAUSTED',
		)
		svc.revoke(PROD, a.key.uid, '')
		expect(codeOf(() => svc.create(PROD, { subject: { end_user: USER }, display_name: 'b' }))).toBe(
			'NO_ERROR',
		)
	})
})

describe('get, list, update', () => {
	test('list filters by subject and state and pages', () => {
		const { svc } = setup()
		const a = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		svc.create(PROD, { subject: { end_user: USER }, display_name: 'b' })
		svc.create(PROD, { subject: { end_user: USER_2 }, display_name: 'c' })
		svc.revoke(PROD, a.key.uid, '')
		expect(svc.list(PROD, { subject: USER }).keys).toHaveLength(2)
		expect(svc.list(PROD, { subject: USER, state: 'ACTIVE' }).keys).toHaveLength(1)
		expect(svc.list(PROD, { state: 'REVOKED' }).keys.map((k) => k.uid)).toEqual([a.key.uid])
		const first = svc.list(PROD, { page_size: 2 })
		expect(first.keys).toHaveLength(2)
		expect(first.next_page_token).not.toBe('')
		const second = svc.list(PROD, { page_size: 2, page_token: first.next_page_token })
		expect(second.keys).toHaveLength(1)
		expect(second.next_page_token).toBe('')
	})

	test('update changes name, scopes, limits, metadata and expiry, never the subject or the secret', () => {
		const { svc, clock } = setup()
		const { key, secret } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		const expire = new Date(clock() + 3600_000).toISOString()
		const updated = svc.update(PROD, key.uid, {
			display_name: 'b',
			scopes: ['stream'],
			limits: [{ display_name: 'per_minute', limit: 60, period: '60s', scope: 'key' }],
			metadata: { version_date: '2026-10-03' },
			expire_time: expire,
		})
		expect(updated.name).toBe(`${PROD}/end_user_keys/${key.uid}`)
		expect(updated.display_name).toBe('b')
		expect(updated.scopes).toEqual(['stream'])
		expect(updated.metadata).toEqual({ version_date: '2026-10-03' })
		expect(updated.expire_time).toBe(expire)
		expect(updated.subject).toEqual({ end_user: USER })
		expect(
			codeOf(() => svc.update(PROD, key.uid, { subject: { end_user: USER_2 } } as never)),
		).toBe('INVALID_ARGUMENT')
		expect(svc.verify(PROD, { key: secret }).valid).toBe(true)
		expect(codeOf(() => svc.update(PROD, key.uid, { scopes: ['admin'] }))).toBe('INVALID_ARGUMENT')
	})

	test('an unknown key is NOT_FOUND', () => {
		const { svc } = setup()
		expect(codeOf(() => svc.get(PROD, 'euk_01m3jzg7fpek2ayen9cq4bn2ef'))).toBe('NOT_FOUND')
	})
})

describe('revoke and roll', () => {
	test('revoke refuses the key on the very next verify', () => {
		const { svc } = setup()
		const { key, secret } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		expect(svc.verify(PROD, { key: secret }).code).toBe('VALID')
		const revoked = svc.revoke(PROD, key.uid, 'leaked')
		expect(revoked.state).toBe('REVOKED')
		expect(revoked.revoke_reason).toBe('leaked')
		expect(revoked.revoke_time).toBeTruthy()
		expect(svc.verify(PROD, { key: secret }).code).toBe('REVOKED')
		// Revoking again changes nothing.
		expect(svc.revoke(PROD, key.uid, 'again').revoke_time).toBe(revoked.revoke_time)
		expect(svc.get(PROD, key.uid).revoke_reason).toBe('leaked')
	})

	test('roll returns a new secret once and keeps the old valid for the overlap', () => {
		const { svc, advance } = setup()
		const { key, secret: oldSecret } = svc.create(PROD, {
			subject: { end_user: USER },
			display_name: 'a',
		})
		const rolled = svc.roll(PROD, key.uid, { previous_expire_duration: '3600s' })
		expect(rolled.secret).not.toBe(oldSecret)
		expect(rolled.key.uid).toBe(key.uid)
		expect(svc.verify(PROD, { key: rolled.secret }).code).toBe('VALID')
		expect(svc.verify(PROD, { key: oldSecret }).code).toBe('VALID')
		advance(3600_000 + 1)
		expect(svc.verify(PROD, { key: oldSecret }).code).toBe('EXPIRED')
		expect(svc.verify(PROD, { key: rolled.secret }).code).toBe('VALID')
	})

	test('the overlap defaults to 24 hours and 0s ends the old secret at once', () => {
		const { svc, advance } = setup()
		const a = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		svc.roll(PROD, a.key.uid, {})
		advance(23 * 3600_000)
		expect(svc.verify(PROD, { key: a.secret }).code).toBe('VALID')
		advance(2 * 3600_000)
		expect(svc.verify(PROD, { key: a.secret }).code).toBe('EXPIRED')
		const b = svc.create(PROD, { subject: { end_user: USER_2 }, display_name: 'b' })
		svc.roll(PROD, b.key.uid, { previous_expire_duration: '0s' })
		expect(svc.verify(PROD, { key: b.secret }).code).toBe('EXPIRED')
	})

	test('rolling a revoked key is refused', () => {
		const { svc } = setup()
		const { key } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		svc.revoke(PROD, key.uid, '')
		expect(codeOf(() => svc.roll(PROD, key.uid, {}))).toBe('FAILED_PRECONDITION')
	})
})

describe('import (hash-only)', () => {
	const row = (token: string, extra: object = {}) => ({
		digest: secretSha256Hex(token),
		start: token.slice(0, 12),
		subject: { end_user: USER },
		scopes: ['read'],
		display_name: 'legacy',
		metadata: { version_date: '2026-01-01' },
		...extra,
	})
	const legacy = 'kk_live_legacyTokenFromTheAppsOwnStore0001'

	test('an imported token verifies with no reissue, and the response holds no secret', () => {
		const { svc } = setup()
		const result = svc.import(PROD, [row(legacy)])
		expect(result.imported).toBe(1)
		expect(result.existing).toBe(0)
		expect(JSON.stringify(result)).not.toContain(legacy)
		const verified = svc.verify(PROD, { key: legacy })
		expect(verified.code).toBe('VALID')
		expect(verified.subject).toEqual({ end_user: USER })
		expect(verified.scopes).toEqual(['read'])
		expect(verified.metadata).toEqual({ version_date: '2026-01-01' })
	})

	test('is idempotent on the digest', () => {
		const { svc } = setup()
		svc.import(PROD, [row(legacy)])
		const again = svc.import(PROD, [row(legacy, { display_name: 'changed' })])
		expect(again.imported).toBe(0)
		expect(again.existing).toBe(1)
		expect(svc.list(PROD, {}).keys).toHaveLength(1)
		expect(svc.list(PROD, {}).keys[0]?.display_name).toBe('legacy')
	})

	test('takes at most 500 rows per call and only well-formed digests', () => {
		const { svc } = setup()
		const rows = Array.from({ length: 501 }, (_, i) =>
			row(`kk_live_bulk${String(i).padStart(30, '0')}`),
		)
		expect(codeOf(() => svc.import(PROD, rows))).toBe('INVALID_ARGUMENT')
		expect(svc.list(PROD, {}).keys).toHaveLength(0)
		expect(svc.import(PROD, rows.slice(0, 500)).imported).toBe(500)
		expect(codeOf(() => svc.import(PROD, [{ ...row(legacy), digest: 'not-hex' }]))).toBe(
			'INVALID_ARGUMENT',
		)
	})

	test('a revoked imported key stays revoked', () => {
		const { svc } = setup()
		const { keys } = svc.import(PROD, [row(legacy)])
		svc.revoke(PROD, keys[0]?.uid as string, '')
		expect(svc.verify(PROD, { key: legacy }).code).toBe('REVOKED')
	})
})

describe('verify', () => {
	test('answers valid with the subject, scopes and metadata, and checks required scopes', () => {
		const { svc } = setup()
		const { key, secret } = svc.create(PROD, {
			subject: { external_id: 'acct-42' },
			display_name: 'a',
			scopes: ['read'],
			metadata: { version_date: '2026-10-03' },
		})
		expect(svc.verify(PROD, { key: secret })).toEqual({
			valid: true,
			code: 'VALID',
			end_user_key: key.name,
			subject: { external_id: 'acct-42' },
			scopes: ['read'],
			metadata: { version_date: '2026-10-03' },
		})
		expect(svc.verify(PROD, { key: secret, required_scopes: ['read'] }).code).toBe('VALID')
		expect(svc.verify(PROD, { key: secret, required_scopes: ['stream'] }).code).toBe(
			'INSUFFICIENT_SCOPE',
		)
	})

	test('an expired key answers EXPIRED and an unknown or malformed token NOT_FOUND', () => {
		const { svc, advance, clock } = setup()
		const { secret } = svc.create(PROD, {
			subject: { end_user: USER },
			display_name: 'a',
			expire_time: new Date(clock() + 1000).toISOString(),
		})
		expect(svc.verify(PROD, { key: secret }).code).toBe('VALID')
		advance(1001)
		expect(svc.verify(PROD, { key: secret }).code).toBe('EXPIRED')
		expect(svc.verify(PROD, { key: 'nonsense' })).toMatchObject({ valid: false, code: 'NOT_FOUND' })
		expect(svc.verify(PROD, { key: assembleToken('kk', 'live', 'q'.repeat(32)) }).code).toBe(
			'NOT_FOUND',
		)
	})

	test("another environment's verify never finds the key", () => {
		const { svc } = setup()
		const { secret } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		expect(svc.verify(PROD, { key: secret }).code).toBe('VALID')
		expect(svc.verify(PREVIEW, { key: secret }).code).toBe('NOT_FOUND')
		expect(svc.verify(OTHER_PROD, { key: secret }).code).toBe('NOT_FOUND')
	})

	test('a token with another environment prefix is refused without a lookup', () => {
		const { svc } = setup()
		svc.setPolicy(OTHER_PROD, { prefix: 'zz' })
		const { secret } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		const before = svc.lookupCount
		expect(svc.verify(OTHER_PROD, { key: secret }).code).toBe('NOT_FOUND')
		expect(svc.lookupCount).toBe(before)
	})

	test('last_used_time is written at most once a minute', () => {
		const { svc, advance } = setup()
		const { key, secret } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		expect(svc.get(PROD, key.uid).last_used_time).toBeUndefined()
		svc.verify(PROD, { key: secret })
		const first = svc.get(PROD, key.uid).last_used_time
		expect(first).toBeTruthy()
		advance(30_000)
		svc.verify(PROD, { key: secret })
		expect(svc.get(PROD, key.uid).last_used_time).toBe(first)
		advance(31_000)
		svc.verify(PROD, { key: secret })
		expect(svc.get(PROD, key.uid).last_used_time).not.toBe(first)
	})
})

describe('an end-user key is never a Sylphx credential', () => {
	test('the Resource API answers UNAUTHENTICATED to a live end-user key', () => {
		const { svc } = setup()
		const { secret } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		const api = new MockResourceApi([
			'sylphx_sk_live_abcd0000000000000000_AbCdEfGhIjKlMnOpQrStUvWxYz0123454AYnNL',
		])
		expect(api.authenticate(secret)).toEqual({ ok: false, code: 'UNAUTHENTICATED' })
		expect(
			api.authenticate('sylphx_sk_live_abcd0000000000000000_AbCdEfGhIjKlMnOpQrStUvWxYz0123454AYnNL')
				.ok,
		).toBe(true)
	})

	test('the prefix can never be sylphx', () => {
		expect(validateApiKeysPolicy({ prefix: 'sylphx' })[0]?.field).toBe('prefix')
		const { svc } = setup()
		expect(codeOf(() => svc.setPolicy(PROD, { prefix: 'sylphx' }))).toBe('INVALID_ARGUMENT')
	})
})

describe('erasure and lifecycle events', () => {
	test('deleting an end user erases its keys; lifecycle rows keep only ids', () => {
		const { svc } = setup()
		const a = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		const b = svc.create(PROD, { subject: { end_user: USER }, display_name: 'b' })
		const keep = svc.create(PROD, { subject: { end_user: USER_2 }, display_name: 'c' })
		expect(svc.eraseEndUser(PROD, USER)).toBe(2)
		expect(svc.verify(PROD, { key: a.secret }).code).toBe('NOT_FOUND')
		expect(svc.verify(PROD, { key: b.secret }).code).toBe('NOT_FOUND')
		expect(codeOf(() => svc.get(PROD, a.key.uid))).toBe('NOT_FOUND')
		expect(svc.verify(PROD, { key: keep.secret }).code).toBe('VALID')
		const events = svc.listEvents(PROD, a.key.uid)
		expect(events.map((e) => e.action)).toEqual(['create', 'erase'])
		expect(JSON.stringify(events)).not.toContain(USER)
		expect(JSON.stringify(events)).not.toContain(a.secret)
	})

	test('every lifecycle action writes one event with the actor and changed field names only', () => {
		const { svc } = setup()
		const { key } = svc.create(
			PROD,
			{ subject: { end_user: USER }, display_name: 'a' },
			{ actor: 'prn_01', request_id: 'r1' },
		)
		svc.update(
			PROD,
			key.uid,
			{ display_name: 'b', scopes: ['read'] },
			{ actor: 'prn_01', request_id: 'r2' },
		)
		svc.roll(PROD, key.uid, {}, { actor: 'prn_01', request_id: 'r3' })
		svc.revoke(PROD, key.uid, 'x', { actor: 'prn_01', request_id: 'r4' })
		const events = svc.listEvents(PROD, key.uid)
		expect(events.map((e) => e.action)).toEqual(['create', 'update', 'roll', 'revoke'])
		expect(events.map((e) => e.request_id)).toEqual(['r1', 'r2', 'r3', 'r4'])
		expect(events[1]?.changed_fields).toEqual(['display_name', 'scopes'])
		expect(events.every((e) => e.actor === 'prn_01')).toBe(true)
	})

	test("one environment's events and keys are invisible to another", () => {
		const { svc } = setup()
		const { key } = svc.create(PROD, { subject: { end_user: USER }, display_name: 'a' })
		expect(codeOf(() => svc.get(PREVIEW, key.uid))).toBe('NOT_FOUND')
		expect(svc.list(PREVIEW, {}).keys).toEqual([])
		expect(codeOf(() => svc.listEvents(PREVIEW, key.uid))).toBe('NOT_FOUND')
	})
})
