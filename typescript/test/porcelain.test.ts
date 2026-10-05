/**
 * Contract test — the porcelain method option.
 *
 * Runs `contracts/fixtures/porcelain.json` through the reference derivation
 * (`test/porcelain-reference.ts`). The generator must produce these same names for the
 * TypeScript method, the Rust method, the CLI command and the MCP tool.
 * Contract: docs/services/contracts/porcelain.md.
 */

import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import {
	checkPorcelainSet,
	lintPorcelain,
	type PorcelainFindingCode,
	type PorcelainOption,
	porcelainNames,
} from './porcelain-reference.js'

interface Fixtures {
	methods: { method: string; porcelain: PorcelainOption; names: Record<string, string> }[]
	lint: { name: string; porcelain: PorcelainOption; code: PorcelainFindingCode; mcp?: string }[]
	collisions: {
		name: string
		methods: { method: string; porcelain: PorcelainOption }[]
		duplicate: string
	}[]
}

const fixtures = JSON.parse(
	readFileSync(
		fileURLToPath(new URL('../../../contracts/fixtures/porcelain.json', import.meta.url)),
		'utf8',
	),
) as Fixtures

describe('porcelain names', () => {
	for (const row of fixtures.methods) {
		test(`one annotation names every surface: ${row.method}`, () => {
			expect(lintPorcelain(row.porcelain)).toEqual([])
			expect(porcelainNames(row.porcelain)).toEqual(row.names as never)
		})
	}

	test('the four surfaces share one name for every annotated method', () => {
		for (const row of fixtures.methods) {
			const n = porcelainNames(row.porcelain)
			const words = (s: string) =>
				s
					.replace(/^sx\./, '')
					.replace(/^sylphx /, '')
					.replace(/\(\)/g, '')
			const norm = (s: string) =>
				words(s)
					.toLowerCase()
					.replace(/[^a-z0-9]+/g, '')
			const reference = norm(n.mcp)
			expect(norm(n.typescript)).toBe(reference)
			expect(norm(n.rust)).toBe(reference)
			expect(norm(n.cli)).toBe(reference)
		}
	})
})

describe('porcelain lint', () => {
	for (const row of fixtures.lint) {
		test(`refuses ${row.name}`, () => {
			const findings = lintPorcelain(row.porcelain)
			expect(findings.map((f) => f.code)).toContain(row.code)
		})
	}

	test('a doubled prefix such as ai_ai_ is named in the finding', () => {
		const [finding] = lintPorcelain({ verb: 'ai_chat', handle: 'ai' })
		expect(finding?.code).toBe('doubled_prefix')
		expect(finding?.name).toBe('ai_ai_chat')
	})

	for (const row of fixtures.collisions) {
		test(`refuses ${row.name}`, () => {
			const result = checkPorcelainSet(row.methods)
			expect(result.duplicates).toEqual([
				{ name: row.duplicate, methods: row.methods.map((m) => m.method) },
			])
		})
	}

	test('a set with distinct names has no duplicates', () => {
		const result = checkPorcelainSet(fixtures.methods)
		expect(result.duplicates).toEqual([])
		expect(result.findings).toEqual([])
	})
})
