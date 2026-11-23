import { expect, test } from 'bun:test'
import { authSecondsFormat, resolveAuthLocale } from './locale.js'
import { catalogs, localeNames } from './messages.js'

test('chooses the first supported canonical locale and infers Chinese script and region', () => {
	const cases = [
		[[], 'en'],
		[['de-DE', 'fr'], 'en'],
		[['bad_locale', 'JA-JP', 'en'], 'ja'],
		[['zh-hant-hk-u-nu-latn'], 'zh-Hant-HK'],
		[['zh-MO'], 'zh-Hant-HK'],
		[['zh-TW'], 'zh-Hant-TW'],
		[['zh-SG'], 'zh-Hans'],
		[['zh-Hans-TW'], 'zh-Hans'],
		[['zh-Hant-CN'], 'zh-Hant-TW'],
		[['EN-gb', 'ja'], 'en'],
	] as const
	for (const [preferences, locale] of cases) expect(resolveAuthLocale(preferences)).toBe(locale)
})

test('has complete same-language catalogs and localizes countdown units', () => {
	expect(Object.keys(localeNames).sort()).toEqual(Object.keys(catalogs).sort())
	for (const [locale, messages] of Object.entries(catalogs)) {
		expect(Object.keys(messages).sort()).toEqual(Object.keys(catalogs.en).sort())
		expect(Object.keys(messages.errors).sort()).toEqual(Object.keys(catalogs.en.errors).sort())
		if (locale !== 'en') {
			for (const key of Object.keys(messages.errors) as Array<keyof typeof messages.errors>)
				expect(messages.errors[key]).not.toBe(catalogs.en.errors[key])
		}
	}
	expect(authSecondsFormat('en').format(30)).toBe('30 sec')
	expect(authSecondsFormat('ja').format(30)).toBe('30 秒')
	expect(authSecondsFormat('zh-Hans').format(30)).toBe('30秒')
})
