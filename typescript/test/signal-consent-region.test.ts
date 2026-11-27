import { expect, test } from 'bun:test'
import { resolveEffectiveConsent, type SignalConsent } from '../src/signal.js'

const denied: SignalConsent = {
	analytics_storage: 'denied',
	ad_storage: 'denied',
	ad_user_data: 'denied',
	ad_personalization: 'denied',
}
const granted: SignalConsent = {
	analytics_storage: 'granted',
	ad_storage: 'granted',
	ad_user_data: 'granted',
	ad_personalization: 'granted',
}

for (const region of ['KR', 'kr', ' KR ']) {
	test(`${JSON.stringify(region)} requires an explicit grant`, () => {
		expect(resolveEffectiveConsent({ region })).toEqual(denied)
		expect(resolveEffectiveConsent({ region, storedChoice: 'granted' })).toEqual(granted)
	})
}

test('KR honors analytics-only consent and GPC overrides a stored grant', () => {
	expect(resolveEffectiveConsent({ region: 'KR', storedChoice: 'analytics_only' })).toEqual({
		...denied,
		analytics_storage: 'granted',
	})
	const privacy = { region: 'KR', storedChoice: 'granted' } as const
	expect(resolveEffectiveConsent({ ...privacy, gpc: true })).toEqual(denied)
	expect(resolveEffectiveConsent(privacy, new Headers({ 'Sec-GPC': '1' }))).toEqual(denied)
})

test('US remains opt-out, and unknown or missing regions remain strict', () => {
	expect(resolveEffectiveConsent({ region: 'US' })).toEqual(granted)
	expect(resolveEffectiveConsent({ region: 'US', storedChoice: 'denied' })).toEqual(denied)
	expect(resolveEffectiveConsent({ region: 'ZZ' })).toEqual(denied)
	expect(resolveEffectiveConsent({})).toEqual(denied)
})

const existingOptIn =
	'AT BE BG HR CY CZ DK EE FI FR DE GR HU IE IT LV LT LU MT NL PL PT RO SK SI ES SE IS LI NO GB UK CH EU EEA AX GF GP MQ MF RE YT'
for (const region of existingOptIn.split(' ')) {
	test(`${region} remains opt-in`, () => {
		expect(resolveEffectiveConsent({ region })).toEqual(denied)
	})
}
