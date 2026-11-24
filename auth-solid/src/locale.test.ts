import { expect, test } from 'bun:test'
import { authSecondsFormat, resolveAuthLocale } from './locale.js'
import { catalogs, localeNames, resolveSignInMessages } from './messages.js'

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
		[['ko'], 'ko'],
		[['es'], 'es'],
		[['es-MX'], 'es'],
		[['pt-br'], 'pt-BR'],
		[['pt-PT', 'es'], 'es'],
		[['fr-FR', 'KO-kr', 'ja'], 'ko'],
		[['ja-JP', 'ko-KR'], 'ja'],
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

test('supports partial overrides without replacing the selected locale or its other errors', () => {
	expect(catalogs.es.signIn).toBe('Inicia sesión')
	expect(catalogs['pt-BR'].signIn).toBe('Entrar')
	const host = resolveSignInMessages('pt-BR', {
		signIn: 'Minha conta',
		errors: { invalid_code: 'Confira o código.' },
	})
	expect(host.signIn).toBe('Minha conta')
	expect(host.email).toBe('Endereço de e-mail')
	expect(host.errors.invalid_code).toBe('Confira o código.')
	expect(host.errors.unavailable).toBe(catalogs['pt-BR'].errors.unavailable)
	expect(catalogs['pt-BR'].signIn).toBe('Entrar')
})
