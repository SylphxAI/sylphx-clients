import { afterEach, describe, expect, jest, mock, test } from 'bun:test'
import { gunzipSync } from 'node:zlib'
import {
	bindCheckoutAttribution,
	createSignalBrowser,
	detectSignalRegion,
	parseAttributionCookies,
	parseSignalAttribution,
	resolveEffectiveConsent,
	type SignalAttribution,
	type SignalConsent,
	type SignalConsentChoice,
	type SignalEventInput,
	type SignalIngestOptions,
	type SignalPageViewOptions,
} from '../src/signal.js'

const denied: SignalConsent = {
	analytics_storage: 'denied',
	ad_storage: 'denied',
	ad_user_data: 'denied',
	ad_personalization: 'denied',
}
const granted: SignalConsent = {
	...denied,
	analytics_storage: 'granted',
	ad_storage: 'granted',
	ad_user_data: 'granted',
	ad_personalization: 'granted',
}
const analyticsOnly: SignalConsent = { ...denied, analytics_storage: 'granted' }
const gaCookie = '_ga_TEST123=GS2.1.o4%24s1700000000%24g1; _ga_OTHER=GS1.1.1700000001.2'
const cookie =
	'_ga=GA1.2.12345.67890; _fbc=fb.1.1700000000000.OriginalClick; _fbp=fb.1.1700000000000.56789'
const ids = { checkoutSessionId: 'checkout_123' }
const receipt = {
	binding_id: 'binding_123',
	checkout_session_id: 'checkout_123',
	processor_session_id: null,
	expires_at: '2030-01-01T00:00:00Z',
}
const key = ['sylphx', 'pk', 'test'].join('_')

function transport() {
	const requests: Request[] = []
	const inits: RequestInit[] = []
	const fetcher = (async (url, init) => {
		requests.push(new Request(url, init))
		if (init) inits.push(init)
		return Response.json(receipt, { status: 201 })
	}) as typeof fetch
	return { requests, inits, options: { apiKey: key, fetch: fetcher } }
}

describe('Signal consent policy', () => {
	for (const region of [
		'EU',
		'EEA',
		'UK',
		'GB',
		'CH',
		'DE',
		'NO',
		'LI',
		'IS',
		'AX',
		'GF',
		'GP',
		'MQ',
		'MF',
		'RE',
		'YT',
	]) {
		test(`${region} requires opt-in and honors GPC after opt-in`, () => {
			expect(resolveEffectiveConsent({ region })).toEqual(denied)
			expect(resolveEffectiveConsent({ region, storedChoice: 'granted' })).toEqual(granted)
			expect(resolveEffectiveConsent({ region, storedChoice: 'granted', gpc: true })).toEqual(
				denied,
			)
		})
	}
	for (const region of ['US', 'CA', 'AU', 'JP', 'ROW']) {
		test(`${region} grants all four flags by default and honors denial`, () => {
			expect(resolveEffectiveConsent({ region })).toEqual(granted)
			expect(resolveEffectiveConsent({ region, storedChoice: 'denied' })).toEqual(denied)
			expect(
				resolveEffectiveConsent(
					{ region, storedChoice: 'granted' },
					new Headers({ 'Sec-GPC': '0, 1' }),
				),
			).toEqual(denied)
		})
	}
	for (const region of [undefined, 'XX', 'T1', 'ZZ', '??', 'US-CA']) {
		test(`unknown region fails closed: ${region}`, () =>
			expect(resolveEffectiveConsent({ region })).toEqual(denied))
	}
	test('trace reads only loc and refuses malformed, ambiguous or unavailable signals', async () => {
		const trace = (async (url, init) => {
			expect(url).toBe('/cdn-cgi/trace')
			expect(init).toMatchObject({ credentials: 'omit', redirect: 'error', cache: 'no-store' })
			return new Response('colo=LHR\r\nloc=GB\r\nother=US\r\n')
		}) as typeof fetch
		expect(await detectSignalRegion(trace)).toBe('GB')
		for (const body of ['loc=US\nloc=US\n', 'colo=US\n', 'loc=XX\n', 'notloc=US\n']) {
			expect(
				await detectSignalRegion((async () => new Response(body)) as unknown as typeof fetch),
			).toBeUndefined()
		}
		expect(
			await detectSignalRegion((async () => {
				throw new Error('offline')
			}) as unknown as typeof fetch),
		).toBeUndefined()
	})
})

test('decodes only the named cookies, preserves case, rejects duplicates and malformed values', () => {
	expect(parseAttributionCookies(cookie.replace('GA1.2', 'GA1%2E2'))).toEqual({
		gaClientId: '12345.67890',
		fbc: 'fb.1.1700000000000.OriginalClick',
		fbp: 'fb.1.1700000000000.56789',
	})
	expect(
		parseAttributionCookies(
			'_ga=GA1.2.3.4; _ga=GA1.2.5.6; _fbc=%E0%A4; _fbp=fb.1.123.%0A12; _ga_extras=GA1.2.5.6',
		),
	).toEqual({})
})

test('selects only the configured GA4 property and parses GS1 and GS2 sessions', () => {
	expect(parseAttributionCookies(gaCookie)).toEqual({})
	expect(parseAttributionCookies(gaCookie, 'G-TEST123')).toEqual({ gaSessionId: '1700000000' })
	expect(parseAttributionCookies(gaCookie, 'G-OTHER')).toEqual({ gaSessionId: '1700000001' })
	expect(parseAttributionCookies('_ga_TEST123=GS2.1.s1$s2', 'G-TEST123')).toEqual({})
	expect(parseAttributionCookies(`${gaCookie}; _ga_TEST123=GS1.1.1.2`, 'G-TEST123')).toEqual({})
})

test('captures bounded known click and UTM fields only', () => {
	const url = new URL(
		'https://example.com/?gclid=one&gclid=two&gbraid=%0A&fbclid=Some-Click&utm_campaign=Summer+Launch&utm_term=%FF&other=secret',
	)
	expect(parseSignalAttribution(url, 1700000000000)).toEqual({
		fbclid: 'Some-Click',
		capturedAt: 1700000000000,
		utm_campaign: 'Summer Launch',
	})
})

for (const region of ['US', 'CA', 'AU']) {
	test(
		region +
			' implicit checkout sends granted storage and keeps the existing binding usable, even on a sparse repeat',
		async () => {
			const { options, requests } = transport()
			await bindCheckoutAttribution(ids, {
				...options,
				privacy: { region },
				headers: new Headers({ cookie }),
			})
			await bindCheckoutAttribution(ids, { ...options, privacy: { region } })
			const [first, second] = await Promise.all(requests.map((request) => request.json()))
			expect(first).toEqual({
				checkout_session_id: 'checkout_123',
				consent: granted,
				ga_client_id: '12345.67890',
				fbc: 'fb.1.1700000000000.OriginalClick',
				fbp: 'fb.1.1700000000000.56789',
			})
			// Events updates consent but COALESCEs absent attribution columns. No null or blank placeholders.
			expect(second).toEqual({ checkout_session_id: 'checkout_123', consent: granted })
		},
	)
}

for (const [region, storedChoice, gpc] of [
	['EU', undefined, '0'],
	['UK', undefined, '0'],
	['CA', 'denied', '0'],
	['US', 'granted', '1'],
	['US', 'analytics_only', '1'],
] as const) {
	test(`denied checkout upserts only identity: ${region}, choice ${storedChoice}, GPC ${gpc}`, async () => {
		const { options, requests } = transport()
		await bindCheckoutAttribution(ids, {
			...options,
			privacy: { region, storedChoice },
			headers: new Headers({ 'Sec-GPC': gpc }),
			get attribution(): SignalAttribution {
				throw new Error('must not read campaign')
			},
			readCookies: () => {
				throw new Error('must not read cookies')
			},
			pageLocation: new URL('https://example.com/?gclid=private'),
		})
		expect(await requests[0]?.json()).toEqual({
			checkout_session_id: 'checkout_123',
			consent: denied,
		})
	})
}

test('posts the exact canonical binding payload and transport after opt-in', async () => {
	const { options, requests, inits } = transport()
	const result = await bindCheckoutAttribution(
		{ ...ids, processorSessionId: 'processor_1', subscriptionId: 'subscription_1' },
		{
			...options,
			privacy: { region: 'UK', storedChoice: 'granted' },
			gaMeasurementId: 'G-TEST123',
			headers: new Headers({ cookie: [cookie, gaCookie].join('; ') }),
			attribution: {
				gclid: 'GoogleClick',
				gbraid: 'Braid',
				wbraid: 'WebBraid',
				fbclid: 'New.Click',
				capturedAt: 1700000000001,
				utm_campaign: 'Summer Launch',
			},
			pageLocation: new URL('https://example.com/landing?auth_code=secret&gclid=private#private'),
		},
	)
	expect(result).toEqual(receipt)
	const request = requests[0]!
	expect(request.url).toBe('https://api.sylphx.com/v1/events/attribution_bindings')
	expect(inits[0]).toMatchObject({
		method: 'POST',
		credentials: 'omit',
		redirect: 'error',
		referrerPolicy: 'no-referrer',
	})
	expect(request.headers.get('X-Sylphx-Key')).toBe(key)
	expect(request.headers.get('Content-Type')).toBe('application/json')
	expect(request.headers.has('Cookie')).toBe(false)
	expect(await request.json()).toEqual({
		checkout_session_id: 'checkout_123',
		processor_session_id: 'processor_1',
		subscription_id: 'subscription_1',
		consent: granted,
		ga_client_id: '12345.67890',
		ga_session_id: '1700000000',
		gclid: 'GoogleClick',
		gbraid: 'Braid',
		wbraid: 'WebBraid',
		fbc: 'fb.1.1700000000001.New.Click',
		fbp: 'fb.1.1700000000000.56789',
		page_location: 'https://example.com/landing?utm_campaign=Summer+Launch',
	})
})

test('analytics-only checkout sends GA4 and a sanitized page URL, never ads', async () => {
	const { options, requests } = transport()
	const storedChoice: SignalConsentChoice = 'analytics_only'
	expect(resolveEffectiveConsent({ region: 'US', storedChoice })).toEqual(analyticsOnly)
	await bindCheckoutAttribution(ids, {
		...options,
		privacy: { region: 'EU', storedChoice },
		gaMeasurementId: 'G-TEST123',
		headers: new Headers({ cookie: [cookie, gaCookie].join('; ') }),
		attribution: {
			gclid: 'GoogleClick',
			gbraid: 'Braid',
			wbraid: 'WebBraid',
			fbclid: 'New.Click',
			capturedAt: 1700000000001,
			utm_campaign: 'Summer Launch',
			utm_term: ' ',
		},
		pageLocation: new URL(
			'https://example.com/landing?code=private&gclid=private&utm_source=ignored#private',
		),
	})
	await bindCheckoutAttribution(ids, {
		...options,
		privacy: { storedChoice },
		headers: new Headers({ cookie: '_ga=; _fbc=; _fbp=' }),
		attribution: { utm_campaign: '' },
	})
	const [first, second] = await Promise.all(requests.map((request) => request.json()))
	expect(first).toEqual({
		checkout_session_id: 'checkout_123',
		consent: analyticsOnly,
		ga_client_id: '12345.67890',
		ga_session_id: '1700000000',
		page_location: 'https://example.com/landing?utm_campaign=Summer+Launch',
	})
	expect(second).toEqual({ checkout_session_id: 'checkout_123', consent: analyticsOnly })
})

test('rejects requests without a valid checkout key and surfaces endpoint failures', async () => {
	const { options, requests } = transport()
	await expect(bindCheckoutAttribution({ checkoutSessionId: '' }, options)).rejects.toThrow(
		'checkout session ID',
	)
	await expect(
		bindCheckoutAttribution(ids, { ...options, baseUrl: 'http://example.com' }),
	).rejects.toThrow('HTTPS API origin')
	expect(requests).toHaveLength(0)
	await expect(
		bindCheckoutAttribution(
			{ processorSessionId: 'processor_1' },
			{
				...options,
				fetch: (async () => new Response(null, { status: 403 })) as unknown as typeof fetch,
			},
		),
	).rejects.toThrow('HTTP 403')
})

const globals = new Map<string, PropertyDescriptor | undefined>()
function override(name: string, value: unknown) {
	if (!globals.has(name)) globals.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
	Object.defineProperty(globalThis, name, { configurable: true, value })
}
afterEach(() => {
	for (const [name, desc] of globals) {
		if (desc) Object.defineProperty(globalThis, name, desc)
		else Reflect.deleteProperty(globalThis, name)
	}
	globals.clear()
})

function browser() {
	const values = new Map<string, string>()
	const reads: string[] = []
	const writes: [string, string][] = []
	const removals: string[] = []
	let cookieReads = 0
	let cookieWrites = 0
	const location = { href: 'https://example.com/landing?fbclid=Same%2EClick&utm_source=source' }
	const win = Object.assign(new EventTarget(), {
		location,
		localStorage: {
			getItem: (name: string) => {
				reads.push(name)
				return values.get(name) ?? null
			},
			setItem: (name: string, value: string) => {
				writes.push([name, value])
				values.set(name, value)
			},
			removeItem: (name: string) => {
				removals.push(name)
				values.delete(name)
			},
		},
	})
	override('window', win)
	const nav = { globalPrivacyControl: false }
	override('navigator', nav)
	override('document', {
		get cookie() {
			cookieReads++
			return cookie
		},
		set cookie(_value: string) {
			cookieWrites++
		},
	})
	return {
		values,
		reads,
		writes,
		removals,
		nav,
		location,
		cookieReads: () => cookieReads,
		cookieWrites: () => cookieWrites,
	}
}

for (const region of ['EU', 'UK']) {
	test(`${region} Advanced loads only after the denied default and stays cookieless until opt-in`, async () => {
		const { values, reads, location, cookieReads } = browser()
		const calls: unknown[] = []
		const signal = createSignalBrowser({
			region,
			google: {
				mode: 'advanced',
				gtag: (...args) => calls.push(args),
				load: () => calls.push('load'),
			},
		})
		const { options, requests } = transport()
		expect(calls).toEqual([['default', denied], 'load'])
		await signal.bindCheckoutAttribution(ids, options)
		expect(await requests[0]?.json()).toEqual({
			checkout_session_id: 'checkout_123',
			consent: denied,
		})
		expect(reads.some((name) => name.endsWith('.touch'))).toBe(false)
		expect(values.size).toBe(0)
		expect(cookieReads()).toBe(0)
		signal.setConsent('granted')
		expect(calls).toEqual([['default', denied], 'load', ['update', granted]])
		const first: SignalAttribution = signal.capture()
		location.href = 'https://example.com/checkout'
		signal.dispose()
		const reopened = createSignalBrowser({ region })
		expect(reopened.getConsent()).toEqual(granted)
		expect(reopened.capture()).toEqual(first)
		expect(first).toMatchObject({ fbclid: 'Same.Click', utm_source: 'source' })
		reopened.setConsent('denied')
		expect(reopened.capture()).toEqual({})
		expect([...values.keys()]).toEqual(['sylphx.signal.v1.consent'])
		reopened.dispose()
	})
}

test('browser persists analytics-only, retains UTMs but not ad IDs, and honors navigator GPC', async () => {
	const { values, nav, location, cookieReads } = browser()
	const calls: unknown[] = []
	const signal = createSignalBrowser({
		region: 'US',
		google: { gtag: (...args) => calls.push(args), load: () => calls.push('load') },
	})
	expect(signal.capture()).toMatchObject({ fbclid: 'Same.Click', utm_source: 'source' })
	location.href = 'https://example.com/checkout?gclid=NewClick'
	signal.setConsent('analytics_only')
	expect(calls).toEqual([['default', granted], 'load', ['update', analyticsOnly]])
	expect(values.get('sylphx.signal.v1.consent')).toBe('analytics_only')
	expect(JSON.parse(values.get('sylphx.signal.v1.touch')!).touch).toEqual({ utm_source: 'source' })
	signal.dispose()

	const reopened = createSignalBrowser({ region: 'US' })
	expect(reopened.getConsent()).toEqual(analyticsOnly)
	expect(reopened.capture()).toEqual({ utm_source: 'source' })
	nav.globalPrivacyControl = true
	const { options, requests } = transport()
	await reopened.bindCheckoutAttribution(ids, options)
	expect(await requests[0]?.json()).toEqual({
		checkout_session_id: 'checkout_123',
		consent: denied,
	})
	expect(cookieReads()).toBe(0)
	reopened.dispose()
})

test('Basic does not load before grant, and GPC blocks even an explicit choice in Advanced', async () => {
	const { nav, cookieReads } = browser()
	const basic: unknown[] = []
	const signal = createSignalBrowser({
		region: 'EU',
		google: { gtag: (...args) => basic.push(args), load: () => basic.push('load') },
	})
	expect(basic).toEqual([['default', denied]])
	signal.setConsent('granted')
	expect(basic).toEqual([['default', denied], ['update', granted], 'load'])
	signal.dispose()
	nav.globalPrivacyControl = true
	const calls: unknown[] = []
	const gpc = createSignalBrowser({
		region: 'US',
		gpc: false,
		google: {
			mode: 'advanced',
			gtag: (...args) => calls.push(args),
			load: () => calls.push('load'),
		},
	})
	const { options, requests } = transport()
	await gpc.bindCheckoutAttribution(ids, options)
	expect(calls).toEqual([['default', denied]])
	expect(await requests[0]?.json()).toEqual({
		checkout_session_id: 'checkout_123',
		consent: denied,
	})
	expect(cookieReads()).toBe(0)
	gpc.dispose()
})

test('late geo detection cannot override an explicit opt-out', async () => {
	browser()
	let resolveTrace!: (response: Response) => void
	const pending = new Promise<Response>((resolve) => {
		resolveTrace = resolve
	})
	override('fetch', () => pending)
	const signal = createSignalBrowser()
	expect(signal.getConsent()).toEqual(denied)
	signal.setConsent('denied')
	resolveTrace(new Response('loc=US\n'))
	await signal.ready
	expect(signal.getConsent()).toEqual(denied)
	expect(signal.capture()).toEqual({})
	signal.dispose()
})

const consentStorageSlot = 'sylphx.signal.v1.consent'
const touchStorageSlot = 'sylphx.signal.v1.touch'
const utms = {
	utm_source: 'newsletter',
	utm_medium: 'email',
	utm_campaign: 'launch',
	utm_term: 'search',
	utm_content: 'banner',
	utm_id: 'campaign-1',
	utm_source_platform: 'web',
	utm_creative_format: 'image',
	utm_marketing_tactic: 'acquisition',
}
const clicks = { gclid: 'GoogleClick', gbraid: 'Braid', wbraid: 'WebBraid', fbclid: 'MetaClick' }

function ingestTransport(result: number | ((attempt: number) => Response) = 202) {
	const calls: { url: string; init?: RequestInit }[] = []
	const ingest: SignalIngestOptions = {
		apiKey: key,
		fetch: (async (url, init) => {
			calls.push({ url: String(url), init })
			return typeof result === 'number'
				? new Response(null, { status: result })
				: result(calls.length)
		}) as typeof fetch,
	}
	return {
		ingest,
		calls,
		batches: () => calls.map(({ init }) => JSON.parse(String(init?.body))),
	}
}

for (const choice of ['granted', 'analytics_only'] as const) {
	test('SPA preserves campaign parameters in memory and filters them on ' + choice, async () => {
		const { location, writes, removals, values, cookieWrites, cookieReads } = browser()
		const { ingest, calls, batches } = ingestTransport()
		let uuids = 0
		override('crypto', { randomUUID: () => 'id-' + ++uuids })
		location.href = 'https://example.com/landing?' + new URLSearchParams(utms)
		const signal = createSignalBrowser({ region: 'EU', ingest })
		location.href = 'https://example.com/signup?' + new URLSearchParams(clicks)
		expect(signal.capture()).toEqual({})
		expect(await signal.track({ eventName: 'before_consent' })).toBe(false)
		expect(await signal.pageView()).toBe(false)
		location.href = 'https://example.com/checkout?utm_content=banner'
		expect(signal.capture()).toEqual({})
		expect(writes).toEqual([])
		expect(removals).toEqual([])
		expect(calls).toEqual([])
		expect(uuids).toBe(0)
		signal.setConsent(choice)
		const expected = choice === 'granted' ? { ...utms, ...clicks } : utms
		const capture = signal.capture()
		expect(capture).toEqual(
			choice === 'granted' ? { ...expected, capturedAt: expect.any(Number) } : expected,
		)
		expect(JSON.parse(values.get(touchStorageSlot)!).touch).toEqual(capture)
		expect(calls).toHaveLength(0) // Pending view was for /signup, not /checkout.
		await signal.track({ eventName: 'checkout_started' })
		await signal.track({ eventName: 'checkout_confirmed' })
		const events = batches().flatMap((batch) => batch.events)
		expect(events.map((event) => event.properties)).toEqual([expected, expected])
		expect(events.map((event) => [event.event_id, event.distinct_id, event.session_id])).toEqual([
			['id-1', 'id-2', 'id-3'],
			['id-4', 'id-2', 'id-3'],
		])
		expect(new Set(writes.map(([name]) => name))).toEqual(
			new Set([consentStorageSlot, touchStorageSlot]),
		)
		expect(cookieWrites()).toBe(0)
		expect(cookieReads()).toBe(0)
		signal.dispose()
	})
}

test('explicit denial clears pending campaign and pageview, and survives ROW reload', async () => {
	const { location, writes, values, removals } = browser()
	const { ingest, calls } = ingestTransport()
	values.set(touchStorageSlot, 'prior touch')
	const signal = createSignalBrowser({ region: 'UK', ingest })
	await signal.pageView()
	signal.setConsent('denied')
	expect(writes).toEqual([[consentStorageSlot, 'denied']])
	expect(removals).toEqual([touchStorageSlot])
	expect(await signal.track({ eventName: 'blocked' })).toBe(false)
	const reopened = createSignalBrowser({ region: 'ROW', ingest })
	expect(reopened.getConsent()).toEqual(denied)
	expect(await reopened.pageView()).toBe(false)
	reopened.dispose()
	location.href = 'https://example.com/checkout'
	signal.setConsent('granted')
	expect(signal.capture()).toEqual({})
	expect(values.has(touchStorageSlot)).toBe(false)
	expect(calls).toHaveLength(0)
	signal.dispose()
})

for (const via of ['navigator', 'server'] as const) {
	test('passive GPC blocks events and writes, and clears touch via ' + via, async () => {
		const { values, location, nav, writes, removals, cookieWrites } = browser()
		const { ingest, calls, batches } = ingestTransport()
		values.set(touchStorageSlot, 'prior touch')
		if (via === 'navigator') {
			const pending = createSignalBrowser({ region: 'EU', ingest })
			await pending.pageView()
			nav.globalPrivacyControl = true
			expect(await pending.track({ eventName: 'blocked' })).toBe(false)
			nav.globalPrivacyControl = false
			location.href = 'https://example.com/checkout'
			pending.setConsent('granted')
			expect(pending.capture()).toEqual({})
			expect(writes).toEqual([[consentStorageSlot, 'granted']])
			pending.dispose()
			writes.length = 0
			nav.globalPrivacyControl = true
		} else values.set(consentStorageSlot, 'granted')
		const signal = createSignalBrowser({ region: 'ROW', gpc: via === 'server', ingest })
		expect(signal.capture()).toEqual({})
		expect(await signal.pageView()).toBe(false)
		expect(await signal.track({ eventName: 'blocked' })).toBe(false)
		expect(calls).toHaveLength(0)
		expect(writes).toEqual([])
		expect(removals).toEqual([touchStorageSlot])
		expect(cookieWrites()).toBe(0)
		signal.dispose()
		nav.globalPrivacyControl = false
		const optedIn = createSignalBrowser({ region: 'ROW', ingest })
		await optedIn.track({ eventName: 'after_gpc' })
		expect(batches()[0].events[0].properties).toEqual(
			via === 'navigator'
				? {}
				: {
						utm_source: 'source',
						fbclid: 'Same.Click',
					},
		)
		optedIn.dispose()
	})
}

for (const [region, choice, flags] of [
	['US', 'analytics_only', analyticsOnly],
	[undefined, 'analytics_only', analyticsOnly],
	['ROW', 'granted', granted],
] as const) {
	test(`explicit consent in ${region ?? 'an unknown region'} does not replay pre-grant data after ${choice}`, async () => {
		const { location, writes, values, cookieReads } = browser()
		const { ingest, calls, batches } = ingestTransport()
		const trace = mock(async () => new Response('loc=US\n'))
		override('fetch', trace)
		const google: unknown[] = []
		location.href = 'https://example.com/landing?utm_source=before&gclid=BeforeClick'
		const signal = createSignalBrowser({
			region,
			requireExplicitConsent: true,
			ingest,
			google: { gtag: (...args) => google.push(args), load: () => google.push('load') },
		})
		await signal.ready
		expect(signal.getConsent()).toEqual(denied)
		expect(google).toEqual([['default', denied]])
		expect(signal.capture()).toBeUndefined()
		expect(await signal.track({ eventName: 'before_consent' })).toBe(false)
		const { options, requests } = transport()
		await signal.bindCheckoutAttribution(ids, options)
		expect(await requests[0]?.json()).toEqual({
			checkout_session_id: 'checkout_123',
			consent: denied,
		})
		expect(cookieReads()).toBe(0)
		location.href = 'https://example.com/pricing'
		expect(await signal.pageView('/pricing', { ignored: true }, { eventId: 'before_grant' })).toBe(
			false,
		)
		expect(writes).toEqual([])
		expect(calls).toHaveLength(0)
		signal.setConsent(choice)
		expect(signal.getConsent()).toEqual(flags)
		expect(google).toEqual([['default', denied], ['update', flags], 'load'])
		expect(calls).toHaveLength(0)
		expect(values.has(touchStorageSlot)).toBe(false)
		expect(await signal.track({ eventName: 'after_consent' })).toBe(true)
		expect(await signal.pageView()).toBe(true)
		expect(batches().map(({ events }) => events[0].properties)).toEqual([{}, { path: '/pricing' }])
		signal.dispose()

		const reopened = createSignalBrowser({ requireExplicitConsent: true })
		await reopened.ready
		expect(reopened.getConsent()).toEqual(flags)
		expect(trace).not.toHaveBeenCalled()
		reopened.dispose()
	})
}

test('GPC denies collection and Google tag load even after explicitly granting consent', async () => {
	const { writes } = browser()
	const { ingest, calls } = ingestTransport()
	const google: unknown[] = []
	const signal = createSignalBrowser({
		region: 'US',
		requireExplicitConsent: true,
		gpc: true,
		ingest,
		google: {
			mode: 'advanced',
			gtag: (...args) => google.push(args),
			load: () => google.push('load'),
		},
	})
	signal.setConsent('granted')
	expect(signal.getConsent()).toEqual(denied)
	expect(signal.capture()).toBeUndefined()
	expect(await signal.track({ eventName: 'visit' })).toBe(false)
	expect(await signal.pageView()).toBe(false)
	expect(calls).toHaveLength(0)
	expect(writes).toEqual([[consentStorageSlot, 'granted']])
	expect(google).toEqual([['default', denied]])
	signal.dispose()
})

test('emits exactly one page_view per navigation, not per effect call', async () => {
	const { location, writes, values } = browser()
	const { ingest, calls, batches } = ingestTransport()
	location.href = 'https://example.com/landing?variant=a#initial'
	const signal = createSignalBrowser({ region: 'EU', ingest })
	let uuid = 0
	override('crypto', { randomUUID: () => `random-id-${++uuid}` })
	expect(
		await signal.pageView(
			'/landing?variant=a#rerender',
			{ ignored: true },
			{ eventId: 'not_replayed' },
		),
	).toBe(false)
	expect(calls).toHaveLength(0)
	expect(writes).toEqual([])
	signal.setConsent('granted')
	expect(calls).toHaveLength(1)
	expect(batches()[0].events[0].event_id).toBe('random-id-1')
	expect(await Promise.all([signal.pageView(), signal.pageView()])).toEqual([false, false])
	expect(calls).toHaveLength(1)
	location.href = 'https://example.com/pricing?variant=b'
	expect(await Promise.all([signal.pageView(), signal.pageView()])).toEqual([true, false])
	expect(calls).toHaveLength(2)
	location.href = 'https://example.com/landing?variant=a'
	expect(await signal.pageView()).toBe(true)
	expect(await signal.pageView()).toBe(false)
	expect(batches().map(({ events }) => [events[0].event_name, events[0].properties])).toEqual([
		['page_view', { path: '/landing?variant=a' }],
		['page_view', { path: '/pricing?variant=b' }],
		['page_view', { path: '/landing?variant=a' }],
	])
	expect([...values.keys()]).toEqual([consentStorageSlot])
	signal.dispose()
})

test('ROW region resolution promotes only the current pending path and retains attribution', async () => {
	const { location, writes } = browser()
	let resolveTrace!: (response: Response) => void
	override(
		'fetch',
		() =>
			new Promise<Response>((resolve) => {
				resolveTrace = resolve
			}),
	)
	const { ingest, calls, batches } = ingestTransport()
	const signal = createSignalBrowser({ ingest })
	await signal.pageView()
	location.href = 'https://example.com/pricing'
	await signal.pageView()
	expect(writes).toEqual([])
	expect(calls).toHaveLength(0)
	resolveTrace(new Response('loc=US\n'))
	await signal.ready
	expect(await signal.pageView()).toBe(false)
	expect(calls).toHaveLength(1)
	expect(batches()[0].events[0]).toMatchObject({
		event_name: 'page_view',
		properties: { path: '/pricing', utm_source: 'source', fbclid: 'Same.Click' },
	})
	expect(writes.map(([name]) => name)).toEqual([touchStorageSlot])
	signal.dispose()
})

test('track sends the exact Signal wire payload and authenticated keepalive request', async () => {
	const { writes, cookieReads, cookieWrites } = browser()
	const { ingest, calls, batches } = ingestTransport()
	const signal = createSignalBrowser({
		region: 'ROW',
		ingest: { ...ingest, distinctId: 'anon-1', sessionId: 'session-1' },
	})
	const event: SignalEventInput = {
		eventName: 'checkout.started',
		eventId: 'event:001',
		eventTime: '2026-01-02T03:04:05.123Z',
		properties: { utm_source: 'caller', tier: 'starter' },
		numericValue: 42,
		isError: false,
		latencyMs: 5,
	}
	expect(await signal.track(event)).toBe(true)
	expect(calls).toEqual([
		{
			url: 'https://api.sylphx.com/v1/analytics/events:batch',
			init: {
				method: 'POST',
				keepalive: true,
				credentials: 'omit',
				redirect: 'error',
				referrerPolicy: 'no-referrer',
				headers: { 'X-Sylphx-Key': key, 'Content-Type': 'application/json' },
				body: expect.any(String),
			},
		},
	])
	expect(batches()).toEqual([
		{
			consent: 'granted',
			events: [
				{
					event_id: 'event:001',
					event_name: 'checkout.started',
					distinct_id: 'anon-1',
					session_id: 'session-1',
					event_time: '2026-01-02T03:04:05.123Z',
					properties: { fbclid: 'Same.Click', utm_source: 'caller', tier: 'starter' },
					numeric_value: 42,
					is_error: false,
					latency_ms: 5,
				},
			],
		},
	])
	expect(writes.map(([name]) => name)).toEqual([touchStorageSlot])
	expect(cookieReads()).toBe(0)
	expect(cookieWrites()).toBe(0)
	signal.dispose()
})

test('identify validates opaque IDs and uses them only in future consented events', async () => {
	const { location, values, writes, cookieWrites, nav } = browser()
	location.href = 'https://example.com/landing'
	const { ingest, calls, batches } = ingestTransport()
	const signal = createSignalBrowser({ requireExplicitConsent: true, ingest })
	const id = 'user_.:-0123456789'.padEnd(128, 'x')
	expect(signal.identify(id)).toBe(true)
	for (const invalid of ['', 'a'.repeat(129), 'with space', 'line\n', 'user@example.com', 'é']) {
		expect(signal.identify(invalid)).toBe(false)
	}
	expect(await signal.track({ eventName: 'visit' })).toBe(false)
	expect(await signal.pageView()).toBe(false)
	expect(calls).toHaveLength(0)
	expect(writes).toEqual([])
	values.set(consentStorageSlot, 'granted')
	expect(await signal.track({ eventName: 'visit' })).toBe(true)
	expect(await signal.pageView()).toBe(true)
	expect(batches().map((batch) => batch.events[0].distinct_id)).toEqual([id, id])
	nav.globalPrivacyControl = true
	expect(await signal.track({ eventName: 'visit' })).toBe(false)
	expect(calls).toHaveLength(2)
	expect(writes).toEqual([])
	expect(cookieWrites()).toBe(0)
	signal.dispose()
	expect(signal.identify('new-user')).toBe(false)
})

test('reset rotates both IDs and allows the same page view without touching consent or attribution', async () => {
	const { values, writes, removals, cookieWrites } = browser()
	values.set(consentStorageSlot, 'analytics_only')
	const { ingest, batches } = ingestTransport()
	const signal = createSignalBrowser({
		requireExplicitConsent: true,
		ingest: { ...ingest, distinctId: 'configured-user', sessionId: 'configured-session' },
	})
	expect(signal.identify('signed-in-user')).toBe(true)
	expect(await signal.pageView()).toBe(true)
	expect(await signal.pageView()).toBe(false)
	const touch = values.get(touchStorageSlot)
	expect(touch).toBeDefined()
	signal.reset()
	expect(signal.getConsent()).toEqual(analyticsOnly)
	expect(await signal.pageView()).toBe(true)
	expect(await signal.track({ eventName: 'visit' })).toBe(true)
	const [before, after, next] = batches().map((batch) => batch.events[0])
	expect(before).toMatchObject({ distinct_id: 'signed-in-user', session_id: 'configured-session' })
	expect(after.distinct_id).toMatch(/^[a-f0-9-]{36}$/)
	expect(after.session_id).toMatch(/^[a-f0-9-]{36}$/)
	expect(after.distinct_id).not.toBe(before.distinct_id)
	expect(after.session_id).not.toBe(before.session_id)
	expect(after.properties).toEqual(before.properties)
	expect(next).toMatchObject({ distinct_id: after.distinct_id, session_id: after.session_id })
	expect(values.get(consentStorageSlot)).toBe('analytics_only')
	expect(values.get(touchStorageSlot)).toBe(touch)
	expect(writes).toEqual([[touchStorageSlot, expect.any(String)]])
	expect(removals).toEqual([])
	expect(cookieWrites()).toBe(0)
	// Withdrawal and a new grant must not restore the identity configured before reset.
	signal.setConsent('denied')
	signal.setConsent('analytics_only')
	expect(await signal.track({ eventName: 'visit' })).toBe(true)
	const renewed = batches()[3].events[0]
	expect(renewed.distinct_id).not.toBe('configured-user')
	expect(renewed.session_id).not.toBe('configured-session')
	signal.dispose()
})

test('pageView sends the caller event ID and rejects invalid IDs without marking the route viewed', async () => {
	const { location } = browser()
	location.href = 'https://example.com/pricing'
	const { ingest, calls, batches } = ingestTransport()
	const signal = createSignalBrowser({ region: 'US', requireExplicitConsent: false, ingest })
	for (const eventId of ['', 'not valid', 'x'.repeat(129)]) {
		expect(await signal.pageView('/pricing', undefined, { eventId })).toBe(false)
		expect(await signal.track({ eventName: 'page_view', eventId })).toBe(false)
	}
	expect(calls).toHaveLength(0)
	const options: SignalPageViewOptions = { eventId: 'pv_pricing_1' }
	expect(await signal.pageView('/pricing', { plan: 'pro' }, options)).toBe(true)
	expect(calls[0]?.url).toBe('https://api.sylphx.com/v1/analytics/events:batch')
	expect(batches()[0].events[0]).toMatchObject({
		event_id: 'pv_pricing_1',
		event_name: 'page_view',
		properties: { path: '/pricing', plan: 'pro' },
	})
	signal.dispose()
	const remounted = createSignalBrowser({ region: 'US', ingest })
	expect(await remounted.pageView('/pricing', { plan: 'pro' }, options)).toBe(true)
	expect(batches()[1].events[0].event_id).toBe('pv_pricing_1')
	remounted.dispose()
})

test('page_view strips ad IDs from analytics-only path while preserving the route and UTMs', async () => {
	const { location } = browser()
	const { ingest, batches } = ingestTransport()
	location.href = 'https://example.com/?gclid=GoogleClick&utm_source=ad#fragment'
	const signal = createSignalBrowser({ region: 'EU', ingest })
	await signal.pageView()
	signal.setConsent('analytics_only')
	expect(await signal.pageView()).toBe(false)
	expect(batches()[0].events[0].properties).toEqual({ path: '/?utm_source=ad', utm_source: 'ad' })
	signal.dispose()
})

test('ingest fails closed for absent configuration, unsafe endpoints and invalid event identifiers', async () => {
	browser()
	const { ingest, calls } = ingestTransport()
	for (const invalid of [
		undefined,
		{ ...ingest, apiKey: 'unsafe-key' },
		...[
			'http://api.example.com',
			'https://user@api.example.com',
			'https://api.example.com/path',
			'https://api.example.com/?',
			'https://api.example.com/#',
			'not-a-url',
		].map((baseUrl) => ({ ...ingest, baseUrl })),
	]) {
		const disabled = createSignalBrowser({ region: 'ROW', ingest: invalid })
		expect(await disabled.track({ eventName: 'visit' })).toBe(false)
		expect(await disabled.pageView()).toBe(false)
		disabled.dispose()
	}
	const signal = createSignalBrowser({
		region: 'ROW',
		ingest: { ...ingest, baseUrl: 'https://api.example.com:8443' },
	})
	for (const event of [
		{ eventName: '' },
		{ eventName: '$pageview' },
		{ eventName: 'x'.repeat(129) },
		{ eventName: 'visit', eventId: '' },
		{ eventName: 'visit', eventId: 'not valid' },
	])
		expect(await signal.track(event)).toBe(false)
	expect(await signal.pageView('https://other.example.com')).toBe(false)
	expect(calls).toHaveLength(0)
	expect(await signal.track({ eventName: 'visit' })).toBe(true)
	expect(calls[0]?.url).toBe('https://api.example.com:8443/v1/analytics/events:batch')
	signal.dispose()
})

function unavailable(retryAfter?: string): Response {
	return new Response(null, {
		status: 503,
		headers: retryAfter === undefined ? undefined : { 'Retry-After': retryAfter },
	})
}

// Set IDs and time explicitly so the fixture can put the serialized UTF-8 size on the boundary.
const gzipEvent: SignalEventInput = {
	eventName: 'visit',
	eventId: 'gzip-test',
	eventTime: '2026-01-02T03:04:05.123Z',
}
const gzipWire = (note: string) =>
	JSON.stringify({
		consent: 'granted',
		events: [
			{
				event_id: 'gzip-test',
				event_name: 'visit',
				distinct_id: 'anon',
				session_id: 'session',
				event_time: '2026-01-02T03:04:05.123Z',
				properties: { note },
				is_error: false,
			},
		],
	})
const noteAtSize = (size: number) => {
	const bytes = size - new TextEncoder().encode(gzipWire('')).byteLength
	return 'é'.repeat(Math.floor(bytes / 2)) + 'a'.repeat(bytes % 2)
}
function gzipBrowser(result?: (attempt: number) => Response) {
	const state = browser()
	state.location.href = 'https://example.com/landing'
	const { ingest, calls } = ingestTransport(result)
	const signal = createSignalBrowser({
		region: 'ROW',
		ingest: { ...ingest, distinctId: 'anon', sessionId: 'session' },
	})
	return { ...state, signal, calls }
}

test('uses gzip at exactly 1024 UTF-8 bytes and reuses the same buffered request on 503', async () => {
	const { signal, calls } = gzipBrowser((attempt) =>
		attempt === 1 ? unavailable('0') : new Response(null, { status: 202 }),
	)
	const note = noteAtSize(1024)
	const json = gzipWire(note)
	expect(json.length).toBeLessThan(1024)
	expect(new TextEncoder().encode(json).byteLength).toBe(1024)
	expect(await signal.track({ ...gzipEvent, properties: { note } })).toBe(true)
	expect(calls).toHaveLength(2)
	const body = calls[0]?.init?.body
	if (!(body instanceof ArrayBuffer)) throw new Error('Expected a buffered gzip body')
	expect(body.byteLength).toBeLessThan(1024)
	expect(gunzipSync(new Uint8Array(body)).toString('utf8')).toBe(json)
	expect(calls[0]).toEqual({
		url: 'https://api.sylphx.com/v1/analytics/events:batch',
		init: {
			method: 'POST',
			keepalive: true,
			credentials: 'omit',
			redirect: 'error',
			referrerPolicy: 'no-referrer',
			headers: {
				'X-Sylphx-Key': key,
				'Content-Type': 'application/json',
				'Content-Encoding': 'gzip',
			},
			body,
		},
	})
	expect(calls[1]).toEqual(calls[0])
	expect(calls[1]?.init?.body).toBe(body)
	signal.dispose()
})

test.each([
	'small',
	'missing',
	'constructor error',
	'stream error',
	'equal size',
	'larger',
] as const)('sends the exact plain JSON body when gzip is %s', async (mode) => {
	const { signal, calls } = gzipBrowser()
	const size = mode === 'small' ? 1023 : 1024
	let attempts = 0
	override(
		'CompressionStream',
		mode === 'missing'
			? undefined
			: class extends TransformStream {
					constructor() {
						attempts++
						if (mode === 'constructor error') throw new Error('gzip unavailable')
						super({
							transform(_chunk, controller) {
								if (mode === 'stream error') controller.error(new Error('gzip failed'))
								else controller.enqueue(new Uint8Array(size + (mode === 'larger' ? 1 : 0)))
							},
						})
					}
				},
	)
	const note = noteAtSize(size)
	const json = gzipWire(note)
	expect(new TextEncoder().encode(json).byteLength).toBe(size)
	expect(await signal.track({ ...gzipEvent, properties: { note } })).toBe(true)
	expect(attempts).toBe(mode === 'small' || mode === 'missing' ? 0 : 1)
	expect(calls).toHaveLength(1)
	expect(new Headers(calls[0]?.init?.headers).has('Content-Encoding')).toBe(false)
	expect(calls[0]?.init?.body).toBe(json)
	signal.dispose()
})

test.each([
	'denied',
	'denied then granted',
	'analytics_only',
	'gpc',
	'disposed',
] as const)('cancels delivery when collection becomes %s during compression', async (action) => {
	const { signal, calls, nav } = gzipBrowser()
	const NativeCompressionStream = CompressionStream
	const entered = Promise.withResolvers<void>()
	const resume = Promise.withResolvers<void>()
	override(
		'CompressionStream',
		class {
			readonly writable: WritableStream<BufferSource>
			readonly readable: ReadableStream<Uint8Array>

			constructor() {
				const gate = new TransformStream<BufferSource, BufferSource>({
					async transform(chunk, controller) {
						entered.resolve()
						await resume.promise
						controller.enqueue(chunk)
					},
				})
				this.writable = gate.writable
				this.readable = gate.readable.pipeThrough(new NativeCompressionStream('gzip'))
			}
		},
	)
	const sent = signal.track({ ...gzipEvent, properties: { note: noteAtSize(1024) } })
	// If compression is accidentally bypassed, fail immediately rather than hang on the barrier.
	await Promise.race([entered.promise, sent])
	expect(calls).toHaveLength(0)
	if (action === 'disposed') signal.dispose()
	else if (action === 'gpc') nav.globalPrivacyControl = true
	else if (action === 'denied then granted') {
		signal.setConsent('denied')
		signal.setConsent('granted')
	} else signal.setConsent(action)
	resume.resolve()
	expect(await sent).toBe(false)
	expect(calls).toHaveLength(0)
	signal.dispose()
})

describe('Signal browser ingest retries', () => {
	afterEach(() => {
		jest.useRealTimers()
	})

	test('replays the exact request after Retry-After: 0, including the generated event ID', async () => {
		browser()
		const { ingest, calls, batches } = ingestTransport((attempt) =>
			attempt === 1 ? unavailable('0') : new Response(null, { status: 202 }),
		)
		const signal = createSignalBrowser({ region: 'ROW', ingest })
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		expect(calls).toHaveLength(2)
		expect(calls[1]).toEqual(calls[0])
		expect(batches()[0].events[0].event_id).toEqual(expect.any(String))
		signal.dispose()
	})

	test.each([
		{ header: '2', delay: 2_000 },
		{ header: undefined, delay: 5_000 },
		{ header: '', delay: 5_000 },
		{ header: '-1', delay: 5_000 },
		{ header: '1.5', delay: 5_000 },
		{ header: 'Wed, 25 Nov 2026 12:00:00 GMT', delay: 5_000 },
		{ header: '99999999999999999999', delay: 10_000 },
	])('waits $delay ms for Retry-After: $header', async ({ header, delay }) => {
		browser()
		jest.useFakeTimers()
		const { ingest, calls } = ingestTransport((attempt) =>
			attempt === 1 ? unavailable(header) : new Response(null, { status: 202 }),
		)
		const signal = createSignalBrowser({ region: 'ROW', ingest })
		const sent = signal.track({ eventName: 'visit' })
		await Promise.resolve() // Let the first response schedule the retry timer.
		expect(jest.getTimerCount()).toBe(1)
		jest.advanceTimersByTime(delay - 1)
		await Promise.resolve()
		expect(calls).toHaveLength(1)
		expect(jest.getTimerCount()).toBe(1)
		jest.advanceTimersByTime(1)
		expect(jest.getTimerCount()).toBe(0)
		expect(await sent).toBe(true)
		expect(calls).toHaveLength(2)
		signal.dispose()
	})

	test.each([
		'denial',
		'disposal',
	] as const)('cancels pending retry timers on %s', async (action) => {
		browser()
		jest.useFakeTimers()
		const { ingest, calls } = ingestTransport(() => unavailable('5'))
		const signal = createSignalBrowser({ region: 'ROW', ingest })
		const sent = [signal.track({ eventName: 'visit' }), signal.pageView()]
		await Promise.resolve()
		expect(jest.getTimerCount()).toBe(2)
		if (action === 'denial') signal.setConsent('denied')
		else signal.dispose()
		// No clock advance: both calls must settle now and neither may send a retry.
		expect(jest.getTimerCount()).toBe(0)
		expect(await Promise.all(sent)).toEqual([false, false])
		expect(calls).toHaveLength(2)
		signal.dispose()
	})

	test.each([
		'denied',
		'analytics_only',
		'gpc',
	] as const)('rechecks %s before sending the retry', async (choice) => {
		const { nav, values } = browser()
		jest.useFakeTimers()
		const { ingest, calls, batches } = ingestTransport(() => unavailable('5'))
		const signal = createSignalBrowser({ region: 'ROW', ingest })
		const sent = signal.track({ eventName: 'visit' })
		await Promise.resolve()
		expect(jest.getTimerCount()).toBe(1)
		expect(batches()[0].events[0].properties).toMatchObject({ fbclid: 'Same.Click' })
		// No storage event: the retry itself must read the persisted choice again.
		if (choice === 'gpc') nav.globalPrivacyControl = true
		else values.set(consentStorageSlot, choice)
		jest.advanceTimersByTime(5_000)
		expect(await sent).toBe(false)
		expect(calls).toHaveLength(1)
		if (choice !== 'analytics_only') expect(values.has(touchStorageSlot)).toBe(false)
		signal.dispose()
	})

	test.each([400, 401, 403, 413, 415, 429])('does not retry HTTP %i', async (status) => {
		browser()
		const { ingest, calls } = ingestTransport(status)
		const signal = createSignalBrowser({ region: 'ROW', ingest })
		expect(await signal.track({ eventName: 'visit' })).toBe(false)
		expect(calls).toHaveLength(1)
		signal.dispose()
	})

	test('a network exception is false and cannot cause a duplicate view attempt', async () => {
		browser()
		const { ingest, calls } = ingestTransport(() => {
			throw new TypeError('Network request failed')
		})
		const signal = createSignalBrowser({ region: 'ROW', ingest })
		expect(await signal.pageView()).toBe(false)
		expect(await signal.pageView()).toBe(false)
		expect(calls).toHaveLength(1)
		signal.dispose()
	})

	test('a second 503 ends the retry, without permitting a duplicate page view', async () => {
		browser()
		const { ingest, calls } = ingestTransport(() => unavailable('0'))
		const signal = createSignalBrowser({ region: 'ROW', ingest })
		expect(await signal.pageView()).toBe(false)
		expect(await signal.pageView()).toBe(false)
		expect(calls).toHaveLength(2)
		signal.dispose()
	})
})

test('rechecks consent after serializing caller properties and never sends after a GPC change', async () => {
	const { nav, values, removals } = browser()
	const { ingest, calls } = ingestTransport()
	const signal = createSignalBrowser({ region: 'ROW', ingest })
	expect(
		await signal.track({
			eventName: 'visit',
			properties: {
				get stage() {
					nav.globalPrivacyControl = true
					return 'blocked'
				},
			},
		}),
	).toBe(false)
	expect(calls).toHaveLength(0)
	expect(removals).toEqual([touchStorageSlot])
	expect(values.has(touchStorageSlot)).toBe(false)
	signal.dispose()
})
