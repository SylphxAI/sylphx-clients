import { type AuthLocale, catalogs } from './messages.js'

export const DEFAULT_AUTH_LOCALE: AuthLocale = 'en'

/** Select a shipped language without comparing raw locale strings or relying on the host's locale. */
export function resolveAuthLocale(preferences: readonly string[]): AuthLocale {
	for (const preference of preferences) {
		let locale: Intl.Locale
		try {
			locale = new Intl.Locale(preference)
		} catch {
			continue
		}
		if (Object.hasOwn(catalogs, locale.baseName)) return locale.baseName as AuthLocale
		if (locale.language === 'en' || locale.language === 'ja') return locale.language
		if (locale.language === 'zh') {
			const script = locale.maximize().script
			if (script === 'Hans') return 'zh-Hans'
			if (script === 'Hant')
				return /^(HK|MO)$/.test(locale.region ?? '') ? 'zh-Hant-HK' : 'zh-Hant-TW'
		}
	}
	return DEFAULT_AUTH_LOCALE
}

/** Format a countdown without assuming digit shapes, spacing or translated unit order. */
export function authSecondsFormat(locale: AuthLocale): Intl.NumberFormat {
	return new Intl.NumberFormat(locale, { style: 'unit', unit: 'second', unitDisplay: 'short' })
}
