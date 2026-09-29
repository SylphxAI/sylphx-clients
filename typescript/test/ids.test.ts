// The `ids` module against the golden vectors of contracts/fixtures/ids.json
// (the Rust SDK reads the same file): exact TypeID grammar, round trips, and
// every retired form refused.

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { ids } from '../src/index.js'

interface Fixture {
	valid: { prefix: ids.Prefix; uuid: string; typeid: string; dns: string }[]
	invalid: { input: string; why: string }[]
	wrong_prefix: { expect: ids.Prefix; input: string }[]
}

const fixture: Fixture = JSON.parse(
	readFileSync(new URL('../../../contracts/fixtures/ids.json', import.meta.url), 'utf8'),
)

describe('ids', () => {
	test('valid ids round-trip', () => {
		for (const v of fixture.valid) {
			const id = ids.parse(v.typeid)
			expect(id.prefix).toBe(v.prefix)
			expect(id.uuid()).toBe(v.uuid)
			expect(id.toString()).toBe(v.typeid)
			expect(id.dnsForm()).toBe(v.dns)
			expect(ids.TypeId.fromUuid(v.prefix, v.uuid).toString()).toBe(v.typeid)
			expect(ids.validate(v.prefix, v.typeid).equals(id)).toBe(true)
			expect(JSON.stringify({ id })).toBe(JSON.stringify({ id: v.typeid }))
		}
	})

	test('invalid forms are refused', () => {
		for (const v of fixture.invalid) {
			expect(() => ids.parse(v.input), `${JSON.stringify(v.input)}: ${v.why}`).toThrow(ids.IdError)
		}
	})

	test('a wrong prefix is named', () => {
		for (const v of fixture.wrong_prefix) {
			expect(ids.parse(v.input)).toBeDefined()
			try {
				ids.validate(v.expect, v.input)
				throw new Error(`${v.input} must not validate as ${v.expect}`)
			} catch (e) {
				expect(e).toBeInstanceOf(ids.IdError)
				expect((e as ids.IdError).code).toBe('wrong_prefix')
				expect((e as ids.IdError).message).toContain(`${v.expect}_`)
			}
		}
	})

	test('minted ids are v7 and ordered', async () => {
		const a = ids.mint('prj')
		await new Promise((r) => setTimeout(r, 2))
		const b = ids.mint('prj')
		expect(a.uuid()[14]).toBe('7')
		expect('89ab').toContain(a.uuid()[19] as string)
		expect(a.toString() < b.toString()).toBe(true)
		expect(ids.parse(a.toString()).equals(a)).toBe(true)
		expect(a.toString()).toMatch(/^prj_[0-7][0-9a-hjkmnp-tv-z]{25}$/)
	})

	test('the registry is well formed', () => {
		const seen = new Set<string>()
		for (const p of ids.PREFIXES) {
			expect(p.prefix).toMatch(/^[a-z]{2,5}$/)
			expect(seen.has(p.prefix)).toBe(false)
			seen.add(p.prefix)
		}
		expect(ids.PREFIXES.find((p) => p.prefix === 'prj')?.collection).toBe('projects')
	})
})
