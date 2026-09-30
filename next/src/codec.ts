// Next.js cache values hold Buffers, Maps and other non-JSON values (an
// APP_PAGE entry has a Buffer body and a Map of segment data). This codec turns
// any such value into JSON text and back, so the handlers can store it as bytes.

const TAG = '$sx'

export function encode(value: unknown): unknown {
	if (value === null || typeof value !== 'object') {
		if (value === undefined) return { [TAG]: 'u' }
		if (typeof value === 'number' && !Number.isFinite(value))
			return { [TAG]: 'n', v: String(value) }
		if (typeof value === 'bigint') return { [TAG]: 'i', v: value.toString() }
		return value
	}
	if (value instanceof Uint8Array) return { [TAG]: 'b', v: Buffer.from(value).toString('base64') }
	if (value instanceof ArrayBuffer) return { [TAG]: 'b', v: Buffer.from(value).toString('base64') }
	if (value instanceof Map)
		return { [TAG]: 'm', v: [...value].map(([k, v]) => [encode(k), encode(v)]) }
	if (value instanceof Set) return { [TAG]: 's', v: [...value].map(encode) }
	if (value instanceof Date) return { [TAG]: 'd', v: value.getTime() }
	if (Array.isArray(value)) return value.map(encode)
	const out: Record<string, unknown> = {}
	for (const [k, v] of Object.entries(value)) out[k] = encode(v)
	// A plain object that already uses the marker key is wrapped so it round-trips.
	return TAG in out ? { [TAG]: 'o', v: out } : out
}

export function decode(value: unknown): unknown {
	if (value === null || typeof value !== 'object') return value
	if (Array.isArray(value)) return value.map(decode)
	const o = value as Record<string, unknown>
	switch (o[TAG]) {
		case 'u':
			return undefined
		case 'n':
			return Number(o.v)
		case 'i':
			return BigInt(o.v as string)
		case 'b':
			return Buffer.from(o.v as string, 'base64')
		case 'm':
			return new Map((o.v as [unknown, unknown][]).map(([k, v]) => [decode(k), decode(v)]))
		case 's':
			return new Set((o.v as unknown[]).map(decode))
		case 'd':
			return new Date(o.v as number)
		case 'o': {
			const inner: Record<string, unknown> = {}
			for (const [k, v] of Object.entries(o.v as Record<string, unknown>)) inner[k] = decode(v)
			return inner
		}
	}
	const out: Record<string, unknown> = {}
	for (const [k, v] of Object.entries(o)) out[k] = decode(v)
	return out
}

const enc = new TextEncoder()
const dec = new TextDecoder()

export const toBytes = (value: unknown): Uint8Array => enc.encode(JSON.stringify(encode(value)))
export const fromBytes = (bytes: Uint8Array): unknown => decode(JSON.parse(dec.decode(bytes)))
