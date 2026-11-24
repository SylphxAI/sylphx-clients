import type { AuthLocale, SignInMessagesOverride } from './messages.js'

/** Use the host's explicit language tag when the host also provides its copy. */
export function signInLanguage(
	fallback: AuthLocale,
	locale: string | undefined,
	messages: SignInMessagesOverride | undefined,
): string {
	if (locale !== undefined && messages !== undefined) {
		try {
			return new Intl.Locale(locale).toString()
		} catch {
			// An invalid host locale falls back to the selected bundled locale.
		}
	}
	return fallback
}
