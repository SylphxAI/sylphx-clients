import { afterEach, expect, jest, test } from 'bun:test'
import { createSignalBrowser } from '../src/signal.js'

const globals = new Map<string, PropertyDescriptor | undefined>()
afterEach(() => {
	jest.useRealTimers()
	for (const [key, value] of globals) {
		if (value) Object.defineProperty(globalThis, key, value)
		else Reflect.deleteProperty(globalThis, key)
	}
	globals.clear()
})

class BrowserEvents extends EventTarget {
	addEventListener(type: string, fn: EventListener, options?: AddEventListenerOptions) {
		super.addEventListener(type, (event) => fn((event as CustomEvent<Event>).detail), options)
	}
	fire(type: string, isTrusted = true) {
		this.dispatchEvent(new CustomEvent(type, { detail: { isTrusted } }))
	}
}

function setup(engagement?: true, retry = false) {
	jest.useFakeTimers()
	const values = new Map<string, string>()
	const storage = {
		getItem: (k: string) => values.get(k) ?? null,
		setItem: values.set.bind(values),
		removeItem: values.delete.bind(values),
	}
	const location = new URL('https://example.com/play?q=1#part')
	const win = Object.assign(new BrowserEvents(), {
		location,
		history: {
			pushState: (_state: unknown, _unused: string, url: string) => {
				location.href = new URL(url, location).href
			},
		},
		localStorage: storage,
		sessionStorage: storage,
	})
	const doc = Object.assign(new BrowserEvents(), { visibilityState: 'visible' })
	const nav = { globalPrivacyControl: false }
	for (const [key, value] of Object.entries({ window: win, document: doc, navigator: nav })) {
		globals.set(key, Object.getOwnPropertyDescriptor(globalThis, key))
		Object.defineProperty(globalThis, key, { configurable: true, value })
	}
	const bodies: string[] = []
	const send = (async (_url, init) => {
		bodies.push(String(init?.body))
		return retry && bodies.length === 1
			? new Response(null, { status: 503, headers: { 'Retry-After': '2' } })
			: new Response(null, { status: 202 })
	}) as typeof fetch
	const sdk = createSignalBrowser({
		engagement,
		region: 'ROW',
		ingest: { apiKey: ['sylphx', 'pk', 'test'].join('_'), fetch: send },
	})
	const visible = (visible: boolean) => {
		doc.visibilityState = visible ? 'visible' : 'hidden'
		doc.fire('visibilitychange')
	}
	const event = (i: number) => JSON.parse(bodies[i] ?? '{}').events?.[0]
	return { sdk, win, nav, values, visible, bodies, event }
}
const advance = (ms: number) => jest.advanceTimersByTime(ms)

test('default configuration retains the regional page-view and custom event stream', async () => {
	const { sdk, win, visible, bodies, event } = setup()
	expect(await sdk.pageView()).toBe(true)
	expect(await sdk.track({ eventName: 'round.start' })).toBe(true)
	win.fire('keydown')
	advance(31_000)
	visible(false)
	win.fire('pagehide')
	expect(bodies).toHaveLength(2)
	expect(event(0)).toMatchObject({
		event_name: 'page_view',
		properties: { path: '/play?q=1' },
	})
	expect(event(1)).toMatchObject({ event_name: 'round.start', properties: {} })
	sdk.dispose()
})

test('counts only trusted, visible input after explicit consent and pauses on idle', () => {
	const { sdk, win, visible, bodies } = setup(true)
	win.fire('keydown')
	advance(30_000)
	sdk.setConsent('analytics_only')
	win.fire('keydown', false)
	advance(30_000)
	expect(bodies).toHaveLength(0)
	win.fire('keydown')
	advance(35_000)
	visible(false)
	win.fire('wheel')
	advance(60_000)
	visible(true)
	advance(15_000)
	expect(bodies).toHaveLength(2)
	const expected = { event_name: 'engagement', properties: { engaged_ms: 15_000, path: '/play' } }
	for (const body of bodies)
		expect(JSON.parse(body)).toMatchObject({ consent: 'granted', events: [expected] })
	sdk.dispose()
})

test('flushes once across hide and pagehide; an identical retry preserves the delta and IDs', async () => {
	const { sdk, win, visible, bodies, event } = setup(true, true)
	sdk.setConsent('analytics_only')
	win.fire('wheel')
	advance(2_000)
	visible(false)
	win.fire('pagehide')
	expect(bodies).toHaveLength(1)
	await Promise.resolve()
	advance(2_000)
	await Promise.resolve()
	expect(bodies[1]).toBe(bodies[0])
	win.fire('pageshow')
	visible(true)
	win.fire('pointerdown')
	advance(15_000)
	expect(bodies).toHaveLength(3)
	expect([event(0).properties.engaged_ms, event(2).properties.engaged_ms]).toEqual([2_000, 15_000])
	expect(event(2).event_id).not.toBe(event(0).event_id)
	for (const key of ['distinct_id', 'session_id']) expect(event(2)[key]).toBe(event(0)[key])
	sdk.dispose()
})

test.each([
	'denied',
	'gpc',
	'removed',
])('drops unflushed time and pending retries on %s', async (mode) => {
	const { sdk, win, nav, bodies, values, visible, event } = setup(true, true)
	sdk.setConsent('granted')
	win.fire('keydown')
	advance(16_000)
	await Promise.resolve()
	if (mode === 'denied') sdk.setConsent('denied')
	else if (mode === 'gpc') nav.globalPrivacyControl = true
	else values.delete('sylphx.signal.v1.consent')
	visible(false)
	advance(30_000)
	await Promise.resolve()
	expect(bodies).toHaveLength(1)
	nav.globalPrivacyControl = false
	sdk.setConsent('analytics_only')
	visible(true)
	win.fire('keydown')
	advance(1_500)
	win.fire('pagehide')
	expect(event(1).properties.engaged_ms).toBe(1_500)
	sdk.dispose()
})

test('drops sub-second exits and stops measuring at 60 logical events', () => {
	const { sdk, win, visible, bodies } = setup(true)
	sdk.setConsent('analytics_only')
	for (const ms of [999, ...Array(61).fill(1_000)]) {
		visible(true)
		win.fire('keydown')
		advance(ms)
		visible(false)
		if (ms === 999) expect(bodies).toHaveLength(0)
	}
	expect(bodies).toHaveLength(60)
	expect(jest.getTimerCount()).toBe(0)
	sdk.dispose()
})

test.each([
	'input',
	'tick',
	'hide',
])('flushes only sampled time on a route change detected by %s', (detect) => {
	const { sdk, win, visible, bodies, event } = setup(true)
	sdk.setConsent('analytics_only')
	win.history.pushState(null, '', '/a')
	win.fire('keydown')
	advance(10_000)
	win.fire('pointerdown')
	advance(2_000)
	win.history.pushState(null, '', '/b?q=1#part')
	if (detect === 'tick') advance(3_000)
	if (detect === 'hide') visible(false)
	visible(true)
	win.fire('keydown')
	expect(bodies).toHaveLength(1)
	expect(event(0).properties).toEqual({ path: '/a', engaged_ms: 10_000 })
	advance(1_500)
	win.fire('pagehide')
	expect(bodies).toHaveLength(2)
	expect(event(1).properties).toEqual({ path: '/b', engaged_ms: 1_500 })
	sdk.dispose()
})
