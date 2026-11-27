import { afterEach, expect, test } from 'bun:test'
import { createSignalBrowser, type SignalAttribution } from '../src/signal.js'

type Project = (input: SignalAttribution) => SignalAttribution

const touchKey = 'sylphx.signal.v1.touch'
const origin = 'https://example.com'
const google = { gclid: 'G', gbraid: 'B', wbraid: 'W' }
const utms = { utm_source: 'search', utm_campaign: 'A Launch' }
const campaign = { ...google, fbclid: 'F', ...utms }
const landing = `/landing?plan=pro&${new URLSearchParams(campaign)}`
const withoutFbclid = ({ fbclid: _, ...keep }: SignalAttribution) => keep
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window')
let signal: ReturnType<typeof createSignalBrowser<false>>

afterEach(() => {
	signal?.dispose()
	if (originalWindow) Object.defineProperty(globalThis, 'window', originalWindow)
	else Reflect.deleteProperty(globalThis, 'window')
})

function setup(projectAttribution?: Project, path = landing, saved?: object, region = 'US') {
	const values = new Map<string, string>()
	if (saved) values.set(touchKey, JSON.stringify(saved))
	const location = { href: origin + path }
	Object.defineProperty(globalThis, 'window', {
		configurable: true,
		value: Object.assign(new EventTarget(), {
			location,
			localStorage: {
				getItem: (key: string) => values.get(key) ?? null,
				setItem: values.set.bind(values),
				removeItem: values.delete.bind(values),
			},
		}),
	})
	const sent: Record<string, unknown>[] = []
	const fetcher = (async (_url, init) => {
		const { events } = JSON.parse(String(init?.body))
		for (const event of events) sent.push(event.properties)
		return new Response(null, { status: 202 })
	}) as typeof fetch
	signal = createSignalBrowser({
		region,
		projectAttribution,
		ingest: {
			apiKey: ['sylphx', 'pk', 'test'].join('_'),
			fetch: fetcher,
			distinctId: 'anon',
			sessionId: 'session',
		},
	})
	return { values, location, sent }
}

test('only unchanged input values and original first sightings survive', () => {
	const now = Date.now()
	const mutate: Project = (input) => {
		if (input.firstSeenAt) input.firstSeenAt.wbraid = now + 1
		return Object.assign(input, { gclid: 'fabricated', fbclid: 'substituted', extra: true })
	}
	setup(mutate, '/landing', {
		expiresAt: now + 60_000,
		touch: { fbclid: 'F', wbraid: 'W', firstSeenAt: { fbclid: now - 20, wbraid: now - 10 } },
	})
	expect(signal.capture()).toEqual({ wbraid: 'W', firstSeenAt: { wbraid: now - 10 } })
})

test('queryless restore prunes the saved record without changing its expiry or other fields', async () => {
	const now = Date.now()
	const expiresAt = now + 42_000
	const firstSeenAt = { gclid: now - 30, gbraid: now - 20, wbraid: now - 10 }
	const { values, sent } = setup(withoutFbclid, '/landing', {
		expiresAt,
		touch: { ...campaign, firstSeenAt: { ...firstSeenAt, fbclid: now } },
	})
	const kept = { ...google, ...utms }
	expect(JSON.parse(values.get(touchKey)!)).toEqual({ expiresAt, touch: { ...kept, firstSeenAt } })
	expect(await signal.track({ eventName: 'test' })).toBe(true)
	expect(await signal.pageView()).toBe(true)
	expect(sent).toEqual([kept, { ...kept, path: '/landing' }])
})

test('projects both pre-grant buffering and queryless promotion under the new policy', () => {
	let granted = false
	const policy: Project = ({ utm_campaign, fbclid, ...keep }) =>
		granted ? { ...keep, utm_campaign } : { ...keep, fbclid }
	const { location, values } = setup(policy, landing, undefined, 'GB')
	expect(values.has(touchKey)).toBe(false)
	location.href = `${origin}/landing`
	granted = true
	signal.setConsent('granted')
	const { firstSeenAt, ...fields } = signal.capture()
	expect(fields).toEqual({ ...google, utm_source: 'search' })
	expect(Object.keys(firstSeenAt ?? {})).toEqual(Object.keys(google))
})

test('runtime changes narrow track, batch and page-view URLs without dropping unrelated query parameters', async () => {
	let narrow = false
	const { sent } = setup((input) => (narrow ? withoutFbclid(input) : input))
	expect(signal.capture().fbclid).toBe('F')
	narrow = true
	expect(await signal.track({ eventName: 'test' })).toBe(true)
	expect(await signal.trackBatch([{ eventName: 'batch' }])).toBe(true)
	expect(await signal.pageView(`${landing}&utm_term=one&utm_term=two`)).toBe(true)
	signal.setConsent('analytics_only')
	expect(await signal.pageView(landing.replace('/landing', '/other'))).toBe(true)
	const kept = { ...google, ...utms }
	const path = `?plan=pro&${new URLSearchParams(kept)}`
	expect(sent).toEqual([
		kept,
		kept,
		{ ...kept, path: `/landing${path}` },
		{ ...utms, path: `/other?plan=pro&${new URLSearchParams(utms)}` },
	])
})

test.each([
	() => ({}),
	() => null as unknown as SignalAttribution,
	() => {
		throw new Error('policy unavailable')
	},
])('empty or failed projections remove saved touch and strip campaign query parameters', async (projectAttribution) => {
	const { values, sent } = setup(projectAttribution, '/landing', {
		expiresAt: Date.now() + 60_000,
		touch: { ...campaign, firstSeenAt: { fbclid: Date.now() } },
	})
	expect(values.has(touchKey)).toBe(false)
	expect(signal.capture()).toEqual({})
	expect(await signal.pageView(landing)).toBe(true)
	expect(sent).toEqual([{ path: '/landing?plan=pro' }])
})

test('without projection, campaign and page-view path are unchanged', async () => {
	const { sent } = setup()
	await signal.pageView()
	expect(sent).toEqual([{ ...campaign, path: landing }])
})
