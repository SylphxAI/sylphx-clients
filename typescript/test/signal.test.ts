import { afterEach, describe, expect, test } from 'bun:test'
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
	let cookieReads = 0
	const location = { href: 'https://example.com/landing?fbclid=Same%2EClick&utm_source=source' }
	const win = Object.assign(new EventTarget(), {
		location,
		localStorage: {
			getItem: (name: string) => {
				reads.push(name)
				return values.get(name) ?? null
			},
			setItem: (name: string, value: string) => {
				values.set(name, value)
			},
			removeItem: (name: string) => {
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
	})
	return { values, reads, nav, location, cookieReads: () => cookieReads }
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
		const first = signal.capture()
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
