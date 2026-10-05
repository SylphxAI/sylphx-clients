/**
 * The porcelain method option: one short verb and one handle path per method,
 * declared once in the proto (`MethodPolicy.porcelain`) and read by
 * `sylphx-gen` for the TypeScript and Rust SDKs, the CLI and the MCP server, so
 * a verb and its tool name come from the same annotation.
 *
 * This module is the reference derivation (the mock) for that contract: the
 * names an annotation yields on each surface, and the lint that refuses a bad
 * annotation. The generator must agree with it on every row of
 * `contracts/fixtures/porcelain.json`. Contract: docs/services/contracts/porcelain.md.
 */

/** Mirrors `sylphx.common.v1.Porcelain`. */
export interface PorcelainOption {
	/** A short snake_case verb: `start`, `branch`, `check_upgrade`. */
	readonly verb: string
	/** The SDK handle path, dotted snake_case, at most three segments: `workflows`, `storage.bucket`. */
	readonly handle: string
}

/** The one name of an annotated method on each surface. */
export interface PorcelainNames {
	readonly typescript: string
	readonly rust: string
	readonly cli: string
	readonly mcp: string
}

export type PorcelainFindingCode = 'invalid_verb' | 'invalid_handle' | 'doubled_prefix'

export interface PorcelainFinding {
	readonly code: PorcelainFindingCode
	/** The MCP tool name the annotation would produce, where it can be derived. */
	readonly name?: string
	readonly message: string
}

export const PORCELAIN_VERB_MAX = 32
export const PORCELAIN_HANDLE_MAX_SEGMENTS = 3

const SNAKE = /^[a-z][a-z0-9]*(_[a-z0-9]+)*$/

const camel = (snake: string): string =>
	snake.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase())
const kebab = (snake: string): string => snake.replace(/_/g, '-')

function handleSegments(handle: string): string[] {
	return handle.split('.')
}

/** The names an annotation yields on the four surfaces. */
export function porcelainNames(p: PorcelainOption): PorcelainNames {
	const segs = handleSegments(p.handle)
	return {
		typescript: `sx.${[...segs.map(camel), camel(p.verb)].join('.')}`,
		rust: `sx.${[...segs, p.verb].map((s) => `${s}()`).join('.')}`,
		cli: `sylphx ${[...segs.map(kebab), kebab(p.verb)].join(' ')}`,
		mcp: [...segs, p.verb].join('_'),
	}
}

/** True when two neighbouring words of a tool name are the same (`ai_ai_chat`). */
function hasDoubledWord(name: string): boolean {
	const words = name.split('_')
	return words.some((w, i) => i > 0 && w === words[i - 1])
}

/** Findings for one annotation; empty when it is valid. */
export function lintPorcelain(p: PorcelainOption): PorcelainFinding[] {
	const findings: PorcelainFinding[] = []
	const verbOk = SNAKE.test(p.verb) && p.verb.length <= PORCELAIN_VERB_MAX
	if (!verbOk) {
		findings.push({
			code: 'invalid_verb',
			message: `verb must be short snake_case (at most ${PORCELAIN_VERB_MAX} characters)`,
		})
	}
	const segs = handleSegments(p.handle)
	const handleOk = segs.length <= PORCELAIN_HANDLE_MAX_SEGMENTS && segs.every((s) => SNAKE.test(s))
	if (!handleOk) {
		findings.push({
			code: 'invalid_handle',
			message: `handle must be dotted snake_case with at most ${PORCELAIN_HANDLE_MAX_SEGMENTS} segments`,
		})
	}
	if (verbOk && handleOk) {
		const name = porcelainNames(p).mcp
		if (hasDoubledWord(name)) {
			findings.push({
				code: 'doubled_prefix',
				name,
				message: `${name} repeats a word of its handle; drop the prefix from the verb`,
			})
		}
	}
	return findings
}

export interface PorcelainDuplicate {
	readonly name: string
	readonly methods: string[]
}

/** Lint every annotation of a set and report the methods that share one name. */
export function checkPorcelainSet(
	methods: readonly { method: string; porcelain: PorcelainOption }[],
): { findings: (PorcelainFinding & { method: string })[]; duplicates: PorcelainDuplicate[] } {
	const findings: (PorcelainFinding & { method: string })[] = []
	const byName = new Map<string, string[]>()
	for (const { method, porcelain } of methods) {
		for (const f of lintPorcelain(porcelain)) findings.push({ ...f, method })
		const name = porcelainNames(porcelain).mcp
		byName.set(name, [...(byName.get(name) ?? []), method])
	}
	const duplicates = [...byName.entries()]
		.filter(([, ms]) => ms.length > 1)
		.map(([name, ms]) => ({ name, methods: ms }))
	return { findings, duplicates }
}
