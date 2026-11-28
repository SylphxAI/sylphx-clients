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
	type SignalSessionOptions,
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
		firstSeenAt: { fbclid: 1700000000000 },
		utm_campaign: 'Summer Launch',
	})
})

test('click IDs need an observed, unexpired first-seen time before binding', async () => {
	const day = 24 * 60 * 60 * 1000
	const now = Date.parse('2026-11-27T00:00:00Z')
	const clock = jest.spyOn(Date, 'now').mockReturnValue(now)
	try {
		const { options, requests } = transport()
		const bind = (attribution: SignalAttribution, clickIdMaxAgeMs?: number) =>
			bindCheckoutAttribution(ids, {
				...options,
				privacy: { region: 'US' },
				headers: new Headers(),
				attribution,
				clickIdMaxAgeMs,
			})
		// A caller without timestamps keeps its binding, but no click ID or fbc is invented.
		await bind({ gclid: 'NoTime', fbclid: 'NoTime', capturedAt: now } as SignalAttribution)
		const firstSeenAt = { gclid: now - 91 * day, gbraid: now + day, wbraid: now - day }
		const clickIds = { gclid: 'G', gbraid: 'B', wbraid: 'WebBraid', fbclid: 'F' }
		await bind({ ...clickIds, firstSeenAt: { ...firstSeenAt, fbclid: now - 91 * day } })
		await bind({ wbraid: 'WebBraid', firstSeenAt }, 60 * 60 * 1000)
		const bodies = await Promise.all(requests.map((request) => request.json()))
		const base = { checkout_session_id: 'checkout_123', consent: granted }
		expect(bodies).toEqual([
			base,
			{ ...base, wbraid: 'WebBraid', wbraid_first_seen_at: now - day },
			base,
		])
	} finally {
		clock.mockRestore()
	}
})

const metaNow = Date.parse('2026-11-27T00:00:00Z')
const metaMaxAge = 60 * 60 * 1000
const metaFresh = metaNow - 1000
const metaClick = 'Current.Click'
const sameCookie = `fb.2.${metaNow - metaMaxAge - 1}.${metaClick}`
const otherCookie = 'fb.1.1700000000000.OriginalClick'

async function bindMeta(attribution: SignalAttribution | undefined, fbc: string) {
	const clock = jest.spyOn(Date, 'now').mockReturnValue(metaNow)
	try {
		const { options, requests } = transport()
		await bindCheckoutAttribution(ids, {
			...options,
			privacy: { region: 'US' },
			headers: new Headers({ cookie: [cookie.replace(otherCookie, fbc), gaCookie].join('; ') }),
			gaMeasurementId: 'G-TEST123',
			attribution,
			clickIdMaxAgeMs: metaMaxAge,
			pageLocation: new URL('https://example.com/landing'),
		})
		return await requests[0]!.json()
	} finally {
		clock.mockRestore()
	}
}

const metaCases = [
	['untimed', { fbclid: metaClick }],
	['legacy', { fbclid: metaClick, capturedAt: metaFresh }],
	['expired', { fbclid: metaClick, firstSeenAt: { fbclid: metaNow - metaMaxAge - 1 } }],
	['future-skewed', { fbclid: metaClick, firstSeenAt: { fbclid: metaNow + 10 * 60 * 1000 } }],
] as const

test.each([
	...metaCases,
	['invalid', { fbclid: 'Not A Click', firstSeenAt: { fbclid: metaFresh } }],
	['non-string', { fbclid: null as never, firstSeenAt: { fbclid: metaFresh } }],
] as const)('does not substitute another Meta click when the supplied click is %s', async (_mode, input) => {
	const body = await bindMeta(
		{
			...input,
			gclid: 'GoogleClick',
			gbraid: 'Braid',
			wbraid: 'WebBraid',
			utm_campaign: 'Summer Launch',
			firstSeenAt: {
				gclid: metaFresh,
				gbraid: metaFresh,
				wbraid: metaFresh,
				...('firstSeenAt' in input ? input.firstSeenAt : {}),
			},
		},
		otherCookie,
	)
	expect(body).toEqual({
		checkout_session_id: ids.checkoutSessionId,
		consent: granted,
		ga_client_id: '12345.67890',
		ga_session_id: '1700000000',
		gclid: 'GoogleClick',
		gclid_first_seen_at: metaFresh,
		gbraid: 'Braid',
		gbraid_first_seen_at: metaFresh,
		wbraid: 'WebBraid',
		wbraid_first_seen_at: metaFresh,
		fbp: 'fb.1.1700000000000.56789',
		page_location: 'https://example.com/landing?utm_campaign=Summer+Launch',
	})
})

test.each([
	...metaCases,
	['fresh', { fbclid: metaClick, firstSeenAt: { fbclid: metaFresh } }],
] as const)('keeps the exact same-click Meta cookie for %s attribution', async (_mode, attribution) => {
	expect((await bindMeta(attribution, sameCookie)).fbc).toBe(sameCookie)
})

test('derives a fresh Meta click and never substitutes an unrelated cookie if it is oversized', async () => {
	const firstSeenAt = { fbclid: metaFresh }
	expect((await bindMeta({ fbclid: metaClick, firstSeenAt }, otherCookie)).fbc).toBe(
		`fb.1.${metaFresh}.${metaClick}`,
	)
	expect(await bindMeta({ fbclid: 'a'.repeat(256), firstSeenAt }, otherCookie)).not.toHaveProperty(
		'fbc',
	)
})

test.each([
	undefined,
	{ utm_source: 'newsletter' },
	{ fbclid: '' },
])('keeps cookie-only Meta attribution when no fbclid was supplied (%j)', async (attribution) => {
	expect((await bindMeta(attribution, otherCookie)).fbc).toBe(otherCookie)
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
	const seen = Date.now() - 60_000
	const result = await bindCheckoutAttribution(
		{ ...ids, processorSessionId: 'processor_1', subscriptionId: 'subscription_1' },
		{
			...options,
			privacy: { region: 'UK', storedChoice: 'granted' },
			gaMeasurementId: 'G-TEST123',
			headers: new Headers({ cookie: [cookie, gaCookie].join('; '), 'User-Agent': 'Buyer/1.0' }),
			attribution: {
				gclid: 'GoogleClick',
				gbraid: 'Braid',
				wbraid: 'WebBraid',
				fbclid: 'New.Click',
				firstSeenAt: { gclid: seen - 2, gbraid: seen - 1, wbraid: seen, fbclid: seen + 1 },
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
		gclid_first_seen_at: seen - 2,
		gbraid: 'Braid',
		gbraid_first_seen_at: seen - 1,
		wbraid: 'WebBraid',
		wbraid_first_seen_at: seen,
		fbc: `fb.1.${seen + 1}.New.Click`,
		fbp: 'fb.1.1700000000000.56789',
		page_location: 'https://example.com/landing?utm_campaign=Summer+Launch',
		user_agent: 'Buyer/1.0',
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
		headers: new Headers({ cookie: [cookie, gaCookie].join('; '), 'User-Agent': 'Buyer/1.0' }),
		attribution: {
			gclid: 'GoogleClick',
			gbraid: 'Braid',
			wbraid: 'WebBraid',
			fbclid: 'New.Click',
			firstSeenAt: { gclid: Date.now(), fbclid: Date.now() },
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
	const sessions = new Map<string, string>()
	const reads: string[] = []
	const writes: [string, string][] = []
	const removals: string[] = []
	let cookieReads = 0
	let cookieWrites = 0
	const location = { href: 'https://example.com/landing?fbclid=Same%2EClick&utm_source=source' }
	const win = Object.assign(new EventTarget(), {
		location,
		sessionStorage: {
			getItem: (name: string) => sessions.get(name) ?? null,
			setItem: (name: string, value: string) => sessions.set(name, value),
			removeItem: (name: string) => sessions.delete(name),
		},
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
		sessions,
		win,
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
const anonymousStorageSlot = 'sylphx.signal.v1.anonymous'
const sessionStorageSlot = 'sylphx.signal.v1.session'
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
		const seen = Date.now()
		const clock = jest.spyOn(Date, 'now').mockReturnValue(seen)
		expect(signal.capture()).toEqual({})
		clock.mockRestore()
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
			choice === 'granted'
				? { ...expected, firstSeenAt: { gclid: seen, gbraid: seen, wbraid: seen, fbclid: seen } }
				: expected,
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
			new Set([consentStorageSlot, touchStorageSlot, anonymousStorageSlot]),
		)
		expect(cookieWrites()).toBe(0)
		expect(cookieReads()).toBe(0)
		signal.dispose()
	})
}

test('stored click IDs without a first sighting or past max age are dropped on read', () => {
	const { values, location } = browser()
	const now = Date.parse('2026-11-27T00:00:00Z')
	const clock = jest.spyOn(Date, 'now').mockReturnValue(now)
	try {
		values.set(consentStorageSlot, 'granted')
		const store = (touch: unknown) =>
			values.set(touchStorageSlot, JSON.stringify({ expiresAt: now + 1000, touch }))
		const stored = () => JSON.parse(values.get(touchStorageSlot)!).touch
		location.href = 'https://example.com/'
		store({ gclid: 'Legacy', fbclid: 'Legacy', capturedAt: now - 1000, utm_source: 's' })
		const signal = createSignalBrowser({ region: 'US', clickIdMaxAgeMs: 7 * 86_400_000 })
		expect(signal.capture()).toEqual({ utm_source: 's' })
		expect(stored()).toEqual({ utm_source: 's' })
		store({
			gclid: 'Old',
			wbraid: 'Ancient',
			firstSeenAt: { gclid: now - 8 * 86_400_000, wbraid: 1 },
		})
		expect(signal.capture()).toEqual({})
		const firstSeenAt = { gclid: now - 86_400_000 }
		store({ gclid: 'Fresh', utm_source: 's', firstSeenAt })
		// Re-landing on the same click ID keeps its first sighting.
		location.href = 'https://example.com/?gclid=Fresh&utm_source=t'
		expect(signal.capture()).toEqual({ gclid: 'Fresh', utm_source: 't', firstSeenAt })
		clock.mockReturnValue(now + 7 * 86_400_000)
		location.href = 'https://example.com/'
		expect(signal.capture()).toEqual({ utm_source: 't' })
		signal.dispose()
	} finally {
		clock.mockRestore()
	}
})

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
	expect([...values.keys()]).toEqual([consentStorageSlot, anonymousStorageSlot])
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

test('trackBatch sends ordered events once with shared identity and per-event attribution', async () => {
	const { writes, cookieWrites } = browser()
	const { ingest, calls, batches } = ingestTransport()
	const signal = createSignalBrowser({ requireExplicitConsent: true, ingest })
	signal.setConsent('analytics_only')
	expect(signal.identify('signed-in-user')).toBe(true)

	const time = '2026-01-02T03:04:05.123Z'
	const events = [
		{ eventName: 'round.start', eventId: 'round:1:start', eventTime: time },
		{ eventName: 'round.end', eventId: 'round:1:end', properties: { utm_source: 'caller' } },
		{ eventName: 'score', numericValue: 42, isError: true, latencyMs: 1.5 },
	] as const satisfies readonly SignalEventInput[]
	expect(await signal.trackBatch(events)).toBe(true)
	expect(calls).toHaveLength(1)
	expect(calls[0]?.url).toBe('https://api.sylphx.com/v1/analytics/events:batch')
	expect(calls[0]?.init?.method).toBe('POST')
	const [first, second] = batches()[0].events
	const session = first.session_id
	const now = second.event_time
	expect(session).toEqual(expect.any(String))
	expect(new Date(now).toISOString()).toBe(now)
	expect(batches()).toEqual([
		{
			consent: 'granted',
			events: [
				{
					event_id: 'round:1:start',
					event_name: 'round.start',
					distinct_id: 'signed-in-user',
					session_id: session,
					event_time: time,
					properties: { utm_source: 'source' },
					is_error: false,
				},
				{
					event_id: 'round:1:end',
					event_name: 'round.end',
					distinct_id: 'signed-in-user',
					session_id: session,
					event_time: now,
					properties: { utm_source: 'caller' },
					is_error: false,
				},
				{
					event_id: expect.stringMatching(/^[\da-f-]{36}$/),
					event_name: 'score',
					distinct_id: 'signed-in-user',
					session_id: session,
					event_time: now,
					properties: { utm_source: 'source' },
					numeric_value: 42,
					is_error: true,
					latency_ms: 1.5,
				},
			],
		},
	])
	expect(await signal.track({ eventName: 'round.next' })).toBe(true)
	expect(batches()[1].events[0]).toMatchObject({
		distinct_id: 'signed-in-user',
		session_id: session,
	})
	expect(writes.map(([name]) => name)).toEqual([consentStorageSlot, touchStorageSlot])
	expect(cookieWrites()).toBe(0)
	signal.dispose()
})

test('trackBatch rejects invalid bounds or any invalid event without allocating IDs or sending', async () => {
	browser()
	const { ingest, calls } = ingestTransport()
	const signal = createSignalBrowser({ region: 'ROW', ingest })
	const event = { eventName: 'visit' }
	const randomUUID = mock(() => 'unused-id')
	override('crypto', { randomUUID })
	for (const input of [
		{ 0: event, length: 1 },
		[],
		Array.from({ length: 501 }, () => event),
		Object.assign(new Array<SignalEventInput>(2), { 0: event }),
		[event, { eventName: 'not valid' }],
		[event, { eventName: 'x'.repeat(129) }],
		[event, { ...event, eventId: '' }],
		[event, { ...event, eventId: 'not valid' }],
		[event, { ...event, numericValue: Number.POSITIVE_INFINITY }],
		[event, { ...event, latencyMs: -1 }],
	]) {
		expect(await signal.trackBatch(input as readonly SignalEventInput[])).toBe(false)
	}
	expect(randomUUID).not.toHaveBeenCalled()
	expect(calls).toHaveLength(0)
	signal.dispose()
	expect(await signal.trackBatch([event])).toBe(false)
	expect(calls).toHaveLength(0)
})

test.each([
	'missing consent',
	'denied',
	'Sec-GPC',
	'navigator GPC',
] as const)('trackBatch does not send, mint or store IDs with %s', async (mode) => {
	const { nav, writes, cookieWrites } = browser()
	nav.globalPrivacyControl = mode === 'navigator GPC'
	const { ingest, calls } = ingestTransport()
	const randomUUID = mock(() => 'unused-id')
	override('crypto', { randomUUID })
	const signal = createSignalBrowser({
		requireExplicitConsent: true,
		gpc: mode === 'Sec-GPC',
		ingest,
	})
	if (mode === 'denied') signal.setConsent('denied')
	else if (mode !== 'missing consent') signal.setConsent('granted')
	expect(await signal.trackBatch([{ eventName: 'round.start' }, { eventName: 'round.end' }])).toBe(
		false,
	)
	expect(calls).toHaveLength(0)
	expect(randomUUID).not.toHaveBeenCalled()
	expect(writes.map(([name]) => name)).toEqual(
		mode === 'missing consent' ? [] : [consentStorageSlot],
	)
	expect(cookieWrites()).toBe(0)
	signal.dispose()
})

test.each([
	'granted',
	'analytics_only',
] as const)('reuses consented identity across SDK instances with %s', async (choice) => {
	const { values, sessions, location, cookieWrites } = browser()
	location.href = 'https://example.com/landing'
	const { ingest, batches } = ingestTransport()
	const first = createSignalBrowser({ requireExplicitConsent: true, ingest })
	first.setConsent(choice)
	expect(await first.pageView()).toBe(true)
	first.dispose()
	const [before] = batches()[0].events
	const stored = JSON.parse(values.get(anonymousStorageSlot) ?? 'null')
	expect(stored.id).toBe(before.distinct_id)
	expect(typeof stored.timestamp).toBe('number')
	// Reusing a record must leave future co-retained fields alone.
	const extended = JSON.stringify({ ...stored, erasureLink: 'future-field' })
	values.set(anonymousStorageSlot, extended)
	const second = createSignalBrowser({ requireExplicitConsent: true, ingest })
	expect(await second.pageView('/second')).toBe(true)
	expect(batches()[1].events[0]).toMatchObject({
		distinct_id: before.distinct_id,
		session_id: before.session_id,
	})
	expect(values.get(anonymousStorageSlot)).toBe(extended)
	// A separate tab has the same localStorage but a fresh sessionStorage.
	sessions.clear()
	second.dispose()
	const third = createSignalBrowser({ requireExplicitConsent: true, ingest })
	expect(await third.pageView()).toBe(true)
	expect(batches()[2].events[0].distinct_id).toBe(before.distinct_id)
	expect(batches()[2].events[0].session_id).not.toBe(before.session_id)
	expect(cookieWrites()).toBe(0)
	third.dispose()
})

test('regional defaults can send but never read or write persistent identity without explicit consent', async () => {
	const { location, values, sessions, reads, writes } = browser()
	location.href = 'https://example.com/'
	const { ingest, batches } = ingestTransport()
	values.set(anonymousStorageSlot, JSON.stringify({ id: 'not-consented', timestamp: Date.now() }))
	const signal = createSignalBrowser({ region: 'US', ingest })
	expect(await signal.pageView()).toBe(true)
	expect(batches()[0].events[0].distinct_id).not.toBe('not-consented')
	expect(reads).not.toContain(anonymousStorageSlot)
	expect(writes).toEqual([])
	expect(sessions.size).toBe(0)
	signal.dispose()
})

test('rotates the anonymous ID at 13 calendar months, even if active', async () => {
	const { values, location } = browser()
	location.href = 'https://example.com/'
	const { ingest, batches } = ingestTransport()
	const signal = createSignalBrowser({ requireExplicitConsent: true, ingest })
	const clock = jest.spyOn(Date, 'now')
	try {
		const created = Date.parse('2025-01-31T12:00:00Z')
		const expires = Date.parse('2026-02-28T12:00:00Z')
		clock.mockReturnValue(created)
		signal.setConsent('analytics_only')
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		const first = batches()[0].events[0].distinct_id
		clock.mockReturnValue(expires - 1)
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		expect(batches()[1].events[0].distinct_id).toBe(first)
		clock.mockReturnValue(expires)
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		const rotated = batches()[2].events[0].distinct_id
		expect(rotated).not.toBe(first)
		expect(JSON.parse(values.get(anonymousStorageSlot) ?? 'null')).toEqual({
			id: rotated,
			timestamp: expires,
		})
	} finally {
		clock.mockRestore()
		signal.dispose()
	}
})

test('keeps a session for SPA navigation and reloads until inactivity exceeds 30 minutes', async () => {
	const { location } = browser()
	location.href = 'https://example.com/'
	const { ingest, batches } = ingestTransport()
	const options = { requireExplicitConsent: true, ingest }
	let signal = createSignalBrowser(options)
	const clock = jest.spyOn(Date, 'now')
	try {
		const start = Date.parse('2026-11-25T12:00:00Z')
		const minute = 60_000
		clock.mockReturnValue(start)
		signal.setConsent('analytics_only')
		expect(await signal.pageView()).toBe(true)
		const spaTime = start + 30 * minute - 1_000
		clock.mockReturnValue(spaTime)
		expect(await signal.pageView('/spa')).toBe(true)
		signal.dispose()
		signal = createSignalBrowser(options)
		const reloadTime = spaTime + 30 * minute
		clock.mockReturnValue(reloadTime)
		expect(await signal.pageView('/reload')).toBe(true)
		clock.mockReturnValue(reloadTime + 30 * minute + 1_000)
		expect(await signal.pageView('/new')).toBe(true)
		const [first, spa, reload, next] = batches().map(({ events }) => events[0])
		expect(spa.session_id).toBe(first.session_id)
		expect(reload.session_id).toBe(first.session_id)
		expect(next.session_id).not.toBe(first.session_id)
		expect(next.distinct_id).toBe(first.distinct_id)
	} finally {
		clock.mockRestore()
		signal.dispose()
	}
})

async function checkTabSession(
	session: SignalSessionOptions,
	start: number,
	checks: readonly (readonly [now: number, rotate: boolean])[],
) {
	const { location, values } = browser()
	location.href = 'https://example.com/'
	const { ingest, batches } = ingestTransport()
	const signal = createSignalBrowser({ requireExplicitConsent: true, ingest, session })
	const clock = jest.spyOn(Date, 'now')
	try {
		clock.mockReturnValue(start)
		signal.setConsent('analytics_only')
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		const storedAnonymous = values.get(anonymousStorageSlot)
		expect(storedAnonymous).toEqual(expect.any(String))
		for (const [now, rotate] of checks) {
			clock.mockReturnValue(now)
			expect(await signal.track({ eventName: 'visit' })).toBe(true)
			const history = batches()
			const [previous, current] = history.slice(-2).map(({ events }) => events[0])
			if (rotate) expect(current.session_id).not.toBe(previous.session_id)
			else expect(current.session_id).toBe(previous.session_id)
			expect(current.distinct_id).toBe(history[0].events[0].distinct_id)
			expect(values.get(anonymousStorageSlot)).toBe(storedAnonymous)
		}
	} finally {
		clock.mockRestore()
		signal.dispose()
	}
}

test.each([
	[5, 5],
	[0, 1],
	[-5, 1],
	[500, 240],
	[NaN, 30],
	[Infinity, 30],
])('timeoutMinutes %s uses a %s-minute sliding timeout', async (timeoutMinutes, expected) => {
	const start = Date.parse('2026-11-28T12:00:00Z')
	const gap = expected * 60_000
	await checkTabSession({ timeoutMinutes }, start, [
		[start + gap - 1_000, false],
		[start + 2 * gap, true],
	])
})

test.each([
	[
		'UTC midnight in New York',
		'America/New_York',
		'2026-11-28T23:55:00Z',
		'2026-11-29T00:05:00Z',
		false,
	],
	['New York midnight', 'America/New_York', '2026-11-29T04:55:00Z', '2026-11-29T05:05:00Z', true],
	['Tokyo midnight', ' Asia/Tokyo ', '2026-11-28T14:55:00Z', '2026-11-28T15:05:00Z', true],
	[
		'spring-forward in New York',
		'America/New_York',
		'2026-03-08T06:55:00Z',
		'2026-03-08T07:05:00Z',
		false,
	],
	[
		'fall-back in New York',
		'America/New_York',
		'2026-11-01T05:55:00Z',
		'2026-11-01T06:05:00Z',
		false,
	],
] as const)('applies the reporting day at %s', async (_name, resetAtMidnightTimeZone, from, to, rotate) => {
	await checkTabSession({ resetAtMidnightTimeZone }, Date.parse(from), [[Date.parse(to), rotate]])
})

test.each([
	'Mars/Olympus_Mons',
	'',
])('ignores invalid or empty reporting time zone %j', async (resetAtMidnightTimeZone) => {
	const start = Date.parse('2026-11-28T23:58:00Z')
	await checkTabSession({ timeoutMinutes: 5, resetAtMidnightTimeZone }, start, [
		[start + 299_000, false],
		[start + 600_000, true],
	])
})

test('explicit ingest session ID wins across timeout and reporting midnight without persisting', async () => {
	const { location, sessions } = browser()
	location.href = 'https://example.com/'
	const { ingest, batches } = ingestTransport()
	const signal = createSignalBrowser({
		requireExplicitConsent: true,
		ingest: { ...ingest, sessionId: 'caller-session' },
		session: { timeoutMinutes: 1, resetAtMidnightTimeZone: 'UTC' },
	})
	const clock = jest.spyOn(Date, 'now')
	try {
		clock.mockReturnValue(Date.parse('2026-11-28T23:59:00Z'))
		signal.setConsent('analytics_only')
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		clock.mockReturnValue(Date.parse('2026-11-29T00:02:00Z'))
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		expect(batches().map(({ events }) => events[0].session_id)).toEqual([
			'caller-session',
			'caller-session',
		])
		expect(sessions.size).toBe(0)
	} finally {
		clock.mockRestore()
		signal.dispose()
	}
})

test.each([
	'reset',
	'withdrawal',
	'removed opt-in',
	'GPC',
] as const)('%s clears both stored IDs immediately and the next allowed send starts fresh', async (action) => {
	const { location, values, sessions, nav } = browser()
	location.href = 'https://example.com/'
	const { ingest, batches } = ingestTransport()
	const signal = createSignalBrowser({ requireExplicitConsent: true, ingest })
	signal.setConsent('analytics_only')
	expect(await signal.pageView()).toBe(true)
	expect(values.has(anonymousStorageSlot)).toBe(true)
	expect(sessions.has(sessionStorageSlot)).toBe(true)
	if (action === 'reset') signal.reset()
	else if (action === 'withdrawal') signal.setConsent('denied')
	else {
		if (action === 'GPC') nav.globalPrivacyControl = true
		else values.delete(consentStorageSlot)
		expect(signal.getConsent()).toEqual(denied)
	}
	expect(values.has(anonymousStorageSlot)).toBe(false)
	expect(sessions.has(sessionStorageSlot)).toBe(false)
	if (action !== 'reset') {
		expect(await signal.pageView()).toBe(false)
		nav.globalPrivacyControl = false
		signal.setConsent('analytics_only')
	}
	expect(await signal.pageView()).toBe(true)
	const [first, next] = batches().map(({ events }) => events[0])
	expect(next.distinct_id).not.toBe(first.distinct_id)
	expect(next.session_id).not.toBe(first.session_id)
	signal.dispose()
})

test.each([
	'getter',
	'quota',
] as const)('still sends in-memory IDs when storage throws (%s)', async (mode) => {
	const { win, location } = browser()
	location.href = 'https://example.com/'
	const fail = () => {
		throw new Error('storage restricted')
	}
	for (const name of ['localStorage', 'sessionStorage'] as const) {
		if (mode === 'getter') Object.defineProperty(win, name, { get: fail })
		else Object.defineProperty(win[name], 'setItem', { value: fail })
	}
	const { ingest, batches } = ingestTransport()
	const signal = createSignalBrowser({ requireExplicitConsent: true, ingest })
	signal.setConsent('analytics_only')
	expect(await signal.pageView()).toBe(true)
	expect(await signal.pageView('/another')).toBe(true)
	const [first, next] = batches().map(({ events }) => events[0])
	expect(next).toMatchObject({ distinct_id: first.distinct_id, session_id: first.session_id })
	signal.reset()
	expect(await signal.pageView()).toBe(true)
	const reset = batches()[2].events[0]
	expect(reset.distinct_id).not.toBe(first.distinct_id)
	expect(reset.session_id).not.toBe(first.session_id)
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
	const { values, writes, removals, cookieWrites, sessions } = browser()
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
	expect(writes).toEqual([
		[touchStorageSlot, expect.any(String)],
		[anonymousStorageSlot, expect.any(String)],
	])
	expect(removals).toEqual([])
	expect(cookieWrites()).toBe(0)
	// Withdrawal and a new grant must not restore the identity configured before reset.
	signal.setConsent('denied')
	signal.setConsent('analytics_only')
	expect(await signal.track({ eventName: 'visit' })).toBe(true)
	const renewed = batches()[3].events[0]
	expect(sessions.has(sessionStorageSlot)).toBe(true)
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
		expect(await disabled.trackBatch([{ eventName: 'visit' }])).toBe(false)
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
function gzipBrowser(
	result?: (attempt: number) => Response,
	options: Pick<SignalIngestOptions, 'compression'> = {},
) {
	const state = browser()
	state.location.href = 'https://example.com/landing'
	const { ingest, calls } = ingestTransport(result)
	const signal = createSignalBrowser({
		region: 'ROW',
		ingest: { ...ingest, distinctId: 'anon', sessionId: 'session', ...options },
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

test('explicit gzip compresses batches across the UTF-8 threshold and retries identical bytes once', async () => {
	const { signal, calls } = gzipBrowser(() => unavailable('0'), { compression: 'gzip' })
	const note = 'é'.repeat(180)
	const events = Array.from({ length: 2 }, (_, i) => ({
		...gzipEvent,
		eventId: `batch:${i}`,
		properties: { note },
	}))
	expect(await signal.trackBatch(events)).toBe(false)
	expect(calls).toHaveLength(2)
	expect(new Headers(calls[0]?.init?.headers).get('Content-Encoding')).toBe('gzip')
	const body = calls[0]?.init?.body
	if (!(body instanceof ArrayBuffer)) throw new Error('Expected a buffered gzip body')
	const json = gunzipSync(new Uint8Array(body)).toString('utf8')
	expect(json.length).toBeLessThan(1024)
	expect(new TextEncoder().encode(json).byteLength).toBeGreaterThanOrEqual(1024)
	expect(JSON.parse(json)).toEqual({
		consent: 'granted',
		events: events.map((event) => ({
			event_id: event.eventId,
			event_name: event.eventName,
			distinct_id: 'anon',
			session_id: 'session',
			event_time: event.eventTime,
			properties: { note },
			is_error: false,
		})),
	})
	expect(calls[1]).toEqual(calls[0])
	expect(calls[1]?.init?.body).toBe(body)
	signal.dispose()
})

test.each([
	false,
	true,
])('compression none works with a JSON-only proxy (retry: %s)', async (retry) => {
	expect(typeof CompressionStream).toBe('function')
	browser().location.href = 'https://example.com/landing'
	const calls: RequestInit[] = []
	const jsonOnly = (async (_url, init) => {
		if (!init) return new Response(null, { status: 400 })
		calls.push(init)
		if (new Headers(init.headers).has('Content-Encoding'))
			return new Response(null, { status: 415 })
		try {
			if (typeof init.body !== 'string' || !Array.isArray(JSON.parse(init.body).events))
				return new Response(null, { status: 400 })
		} catch {
			return new Response(null, { status: 400 })
		}
		return retry && calls.length % 2 === 1 ? unavailable('0') : new Response(null, { status: 202 })
	}) as typeof fetch
	const signal = createSignalBrowser({
		region: 'ROW',
		ingest: {
			apiKey: key,
			fetch: jsonOnly,
			distinctId: 'anon',
			sessionId: 'session',
			compression: 'none',
		},
	})
	const note = noteAtSize(1025)
	const single = { ...gzipEvent, properties: { note } }
	const batch = Array.from({ length: 500 }, (_, i) => ({ ...gzipEvent, eventId: `batch:${i}` }))
	const singleJson = gzipWire(note)
	const batchJson = JSON.stringify({
		consent: 'granted',
		events: batch.map(({ eventId }) => ({
			event_id: eventId,
			event_name: 'visit',
			distinct_id: 'anon',
			session_id: 'session',
			event_time: gzipEvent.eventTime,
			properties: {},
			is_error: false,
		})),
	})
	expect(await signal.track(single)).toBe(true)
	expect(await signal.trackBatch(batch)).toBe(true)
	expect(calls.map(({ body }) => body)).toEqual(
		retry ? [singleJson, singleJson, batchJson, batchJson] : [singleJson, batchJson],
	)
	for (const call of calls) {
		const headers = new Headers(call.headers)
		expect(headers.get('Content-Type')).toBe('application/json')
		expect(headers.has('Content-Encoding')).toBe(false)
		expect(typeof call.body).toBe('string')
		expect(new TextEncoder().encode(call.body as string).byteLength).toBeGreaterThan(1024)
	}
	signal.dispose()
})

test('rejects unsupported ingest compression at configuration time', () => {
	const { ingest, calls } = ingestTransport()
	const create = () =>
		createSignalBrowser({ ingest: { ...ingest, compression: 'deflate' as never } })
	expect(create).toThrow(TypeError)
	expect(create).toThrow('Signal ingest compression must be "gzip" or "none"')
	expect(calls).toHaveLength(0)
})

test('trackBatch sends exactly 500 events without splitting the request', async () => {
	const { signal, calls } = gzipBrowser()
	const events = Array.from({ length: 500 }, (_, i) => ({ ...gzipEvent, eventId: `batch:${i}` }))
	expect(await signal.trackBatch(events)).toBe(true)
	expect(calls).toHaveLength(1)
	const body = calls[0]?.init?.body
	if (!(body instanceof ArrayBuffer)) throw new Error('Expected a buffered gzip body')
	const wire = JSON.parse(gunzipSync(new Uint8Array(body)).toString('utf8'))
	expect(wire.events.map((event: { event_id: string }) => event.event_id)).toEqual(
		events.map((event) => event.eventId),
	)
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

for (const method of ['track', 'trackBatch'] as const) {
	test.each([
		'denied',
		'denied then granted',
		'analytics_only',
		'gpc',
		'disposed',
	] as const)(`${method}: cancels delivery when collection becomes %s during compression`, async (action) => {
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
		const event = { ...gzipEvent, properties: { note: noteAtSize(1024) } }
		const sent =
			method === 'track'
				? signal.track(event)
				: signal.trackBatch([event, { ...gzipEvent, eventId: 'second' }])
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
}

describe('Signal browser foreground heartbeats', () => {
	afterEach(() => jest.useRealTimers())
	function foreground() {
		jest.useFakeTimers()
		const env = browser()
		const doc = Object.assign(new EventTarget(), { visibilityState: 'visible' })
		override('document', doc)
		let now = 0
		override('performance', { now: () => now })
		return {
			...env,
			...ingestTransport(),
			advance(ms: number, elapsed = ms) {
				now += elapsed
				jest.advanceTimersByTime(ms)
			},
			visibility(state: string) {
				doc.visibilityState = state
				doc.dispatchEvent(new Event('visibilitychange'))
			},
		}
	}
	test.each([
		[undefined, true],
		[false, true],
		[true, false],
	] as const)('is inactive with heartbeat=%s and ingest=%s', (heartbeat, hasEndpoint) => {
		const { ingest, calls, advance, win } = foreground()
		const options = { region: 'US', heartbeat }
		const signal = createSignalBrowser(hasEndpoint ? { ...options, ingest } : options)
		advance(300_000)
		win.dispatchEvent(new Event('pagehide'))
		expect(jest.getTimerCount()).toBe(0)
		expect(calls).toHaveLength(0)
		signal.dispose()
	})
	test.each([
		[true, 60_000],
		[{}, 60_000],
		[{ intervalSeconds: Infinity }, 60_000],
		[{ intervalSeconds: 22 }, 22_000],
		[{ intervalSeconds: 5 }, 15_000],
		[{ intervalSeconds: 600 }, 300_000],
	] as const)('uses the configured, default or clamped interval (%j)', (heartbeat, ms) => {
		const { ingest, calls, batches, advance } = foreground()
		const signal = createSignalBrowser({ region: 'US', ingest, heartbeat })
		advance(ms - 1)
		expect(calls).toHaveLength(0)
		advance(1)
		advance(ms, 100_000_000)
		const events = batches().flatMap((batch) => batch.events)
		expect(events.map(({ event_name, properties }) => [event_name, properties])).toEqual([
			['$heartbeat', { fg_ms: ms }],
			['$heartbeat', { fg_ms: 86_400_000 }],
		])
		signal.dispose()
	})
	test('flushes once per hide and excludes hidden and suspended time', () => {
		const { ingest, batches, advance, visibility, win } = foreground()
		const signal = createSignalBrowser({ region: 'US', ingest, heartbeat: true })
		advance(1_234, 1_234.6)
		visibility('hidden')
		win.dispatchEvent(new Event('pagehide'))
		expect(jest.getTimerCount()).toBe(0)
		advance(60_000)
		visibility('visible')
		advance(2_345)
		win.dispatchEvent(new Event('pagehide'))
		win.dispatchEvent(new Event('pagehide'))
		advance(60_000)
		win.dispatchEvent(new Event('pageshow'))
		advance(60_000)
		const durations = batches().map((batch) => batch.events[0].properties.fg_ms)
		expect(durations).toEqual([1_235, 2_345, 60_000])
		signal.dispose()
	})
	test.each(['denied', 'storage', 'reset', 'dispose'])('discards on %s', (action) => {
		const { ingest, batches, calls, advance, values, win, visibility } = foreground()
		const signal = createSignalBrowser({ requireExplicitConsent: true, ingest, heartbeat: true })
		advance(60_000)
		expect(jest.getTimerCount()).toBe(0)
		signal.setConsent('analytics_only')
		advance(60_000)
		expect(batches()[0].events[0].properties).toEqual({ fg_ms: 60_000 })
		advance(5_000)
		if (action === 'denied') signal.setConsent('denied')
		if (action === 'storage') {
			values.delete(consentStorageSlot)
			win.dispatchEvent(Object.assign(new Event('storage'), { key: consentStorageSlot }))
		}
		if (action === 'reset') signal.reset()
		if (action === 'dispose') signal.dispose()
		expect(jest.getTimerCount()).toBe(0)
		visibility('hidden')
		advance(60_000)
		expect(calls).toHaveLength(1)
		if (action !== 'dispose') signal.setConsent('analytics_only')
		visibility('visible')
		advance(60_000)
		const durations = batches().map((batch) => batch.events[0].properties.fg_ms)
		expect(durations).toEqual(action === 'dispose' ? [60_000] : [60_000, 60_000])
		signal.dispose()
	})
	test('waits for region resolution despite a stored grant and stops under GPC', async () => {
		const { ingest, batches, calls, advance, values, nav } = foreground()
		values.set(consentStorageSlot, 'granted')
		const trace = Promise.withResolvers<Response>()
		override('fetch', () => trace.promise)
		const signal = createSignalBrowser({ ingest, heartbeat: true })
		advance(60_000)
		expect(calls).toHaveLength(0)
		trace.resolve(new Response('loc=US\n'))
		await signal.ready
		advance(60_000)
		expect(batches()[0].events[0].properties).toEqual({ fg_ms: 60_000 })
		nav.globalPrivacyControl = true
		advance(60_000)
		expect(calls).toHaveLength(1)
		expect(jest.getTimerCount()).toBe(0)
		signal.dispose()
	})
	test('shares the active anonymous identity, session and erasure commitment', async () => {
		const { ingest, calls, batches, advance } = foreground()
		const sent = Promise.withResolvers<void>()
		const fetcher = ((url, init) => {
			const response = ingest.fetch?.(url, init)
			if (calls.length === 2) sent.resolve()
			return response
		}) as typeof fetch
		const signal = createSignalBrowser({
			requireExplicitConsent: true,
			heartbeat: true,
			ingest: { ...ingest, fetch: fetcher, linkAnonymousForErasure: true },
		})
		signal.setConsent('analytics_only')
		expect(await signal.track({ eventName: '$heartbeat' })).toBe(false)
		expect(await signal.track({ eventName: 'visit' })).toBe(true)
		advance(60_000)
		await sent.promise
		const [first, second] = batches().map((batch) => batch.events[0])
		expect(first.anonymous_commitment).toMatch(/^[a-f0-9]{64}$/)
		expect(second).toMatchObject({
			event_name: '$heartbeat',
			properties: { fg_ms: 60_000 },
			distinct_id: first.distinct_id,
			session_id: first.session_id,
			anonymous_commitment: first.anonymous_commitment,
		})
		signal.dispose()
	})
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

test('opt-in identify sends one proof of an acknowledged anonymous lifecycle', async () => {
	const { location, values, sessions, cookieWrites } = browser()
	location.href = 'https://example.com/landing'
	values.set(consentStorageSlot, 'analytics_only')
	const { ingest, calls, batches } = ingestTransport()
	const signal = createSignalBrowser({
		requireExplicitConsent: true,
		ingest: { ...ingest, linkAnonymousForErasure: true },
	})
	expect(await signal.track({ eventName: 'visit' })).toBe(true)
	expect(await signal.track({ eventName: 'browse' })).toBe(true)
	const [first, second] = batches().map((b) => b.events[0])
	expect(first.anonymous_commitment).toMatch(/^[a-f0-9]{64}$/)
	expect(second.anonymous_commitment).toBe(first.anonymous_commitment)
	expect(signal.identify('user_a')).toBe(true)
	expect(signal.identify('user_b')).toBe(true)
	expect(calls).toHaveLength(3)
	const envelope = batches()[2]
	expect(envelope).toEqual({
		consent: 'granted',
		identify: {
			anonymous_id: first.distinct_id,
			user_id: 'user_a',
			proof: expect.stringMatching(/^[a-f0-9]{64}$/),
		},
	})
	const hex = envelope.identify.proof as string
	const digest = await crypto.subtle.digest('SHA-256', Buffer.from(hex, 'hex'))
	expect(Buffer.from(digest).toString('hex')).toBe(first.anonymous_commitment)
	expect(String(calls[0]?.init?.body)).not.toContain(hex)
	expect(calls[2]?.init).toMatchObject({
		credentials: 'omit',
		redirect: 'error',
		referrerPolicy: 'no-referrer',
	})
	expect(await signal.track({ eventName: 'signed_in' })).toBe(true)
	expect(batches()[3].events[0].distinct_id).toBe('user_b')
	expect(batches()[3].events[0]).not.toHaveProperty('anonymous_commitment')
	signal.reset()
	expect(await signal.track({ eventName: 'after_logout' })).toBe(true)
	expect(batches()[4].events[0].distinct_id).not.toBe(first.distinct_id)
	expect(batches()[4].events[0].anonymous_commitment).not.toBe(first.anonymous_commitment)
	expect([...values.keys()]).toEqual([consentStorageSlot, anonymousStorageSlot])
	expect(JSON.parse(values.get(anonymousStorageSlot) ?? 'null')).toEqual({
		id: batches()[4].events[0].distinct_id,
		timestamp: expect.any(Number),
	})
	expect(JSON.parse(sessions.get(sessionStorageSlot) ?? 'null')).toEqual({
		id: batches()[4].events[0].session_id,
		timestamp: expect.any(Number),
	})
	expect(cookieWrites()).toBe(0)
	signal.dispose()
})

for (const canceled of [
	false,
	'reset',
	'consent',
	'removed consent',
	'gpc',
	'dispose',
	'account switch',
] as const) {
	test(`identify proof waits for its anonymous beacon, then observes ${canceled}`, async () => {
		const { values, nav, win } = browser()
		values.set(consentStorageSlot, 'granted')
		let started!: () => void
		const began = new Promise<void>((resolve) => {
			started = resolve
		})
		let finish!: (response: Response) => void
		const result = new Promise<Response>((resolve) => {
			finish = resolve
		})
		const sender = mock(async (_url: string, init?: RequestInit) => {
			if (JSON.parse(String(init?.body)).identify) return new Response(null, { status: 503 })
			started()
			return result
		})
		const signal = createSignalBrowser({
			requireExplicitConsent: true,
			ingest: {
				apiKey: key,
				fetch: sender as unknown as typeof fetch,
				linkAnonymousForErasure: true,
			},
		})
		const pending = signal.track({ eventName: 'visit' })
		await began
		expect(signal.identify('user_a')).toBe(true)
		expect(sender).toHaveBeenCalledTimes(1)
		if (canceled === 'reset') signal.reset()
		if (canceled === 'consent') signal.setConsent('denied')
		if (canceled === 'removed consent') {
			values.delete(consentStorageSlot)
			win.dispatchEvent(Object.assign(new Event('storage'), { key: consentStorageSlot }))
			signal.setConsent('granted')
		}
		if (canceled === 'account switch') {
			signal.identify('user_b')
			signal.identify('user_a')
		}
		if (canceled === 'gpc') nav.globalPrivacyControl = true
		if (canceled === 'dispose') signal.dispose()
		finish(new Response(null, { status: 202 }))
		expect(await pending).toBe(true)
		await new Promise((resolve) => setTimeout(resolve, 0))
		expect(sender).toHaveBeenCalledTimes(canceled ? 1 : 2)
		signal.dispose()
	})
}

test('configured IDs are never marked or sent as a browser-generated alias', async () => {
	const { values } = browser()
	values.set(consentStorageSlot, 'granted')
	const { ingest, batches } = ingestTransport()
	const signal = createSignalBrowser({
		requireExplicitConsent: true,
		ingest: { ...ingest, distinctId: 'application-id', linkAnonymousForErasure: true },
	})
	expect(await signal.track({ eventName: 'visit' })).toBe(true)
	expect(signal.identify('user_a')).toBe(true)
	expect(batches()).toHaveLength(1)
	expect(batches()[0].events[0]).not.toHaveProperty('anonymous_commitment')
	signal.dispose()
})

test.each([
	false,
	true,
])('keeps a retained ID unmarked on reload (mark before reload: %s)', async (enabled) => {
	const { values, location } = browser()
	location.href = 'https://example.com/landing'
	values.set(consentStorageSlot, 'analytics_only')
	const { ingest, batches } = ingestTransport()
	const options = {
		requireExplicitConsent: true,
		ingest: { ...ingest, linkAnonymousForErasure: true },
	}
	const first = createSignalBrowser({
		...options,
		ingest: { ...options.ingest, linkAnonymousForErasure: enabled },
	})
	expect(await first.track({ eventName: 'visit' })).toBe(true)
	const before = batches()[0].events[0]
	expect(Object.hasOwn(before, 'anonymous_commitment')).toBe(enabled)
	first.dispose()
	const reloaded = createSignalBrowser(options)
	expect(await reloaded.track({ eventName: 'browse' })).toBe(true)
	const after = batches()[1].events[0]
	expect(after.distinct_id).toBe(before.distinct_id)
	expect(after.session_id).toBe(before.session_id)
	expect(after).not.toHaveProperty('anonymous_commitment')
	expect(reloaded.identify('user_a')).toBe(true)
	expect(batches()).toHaveLength(2)
	reloaded.dispose()
})
