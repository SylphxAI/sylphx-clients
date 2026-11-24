import { catalogs } from './messages.js'

const errors = catalogs.en.errors
export const unavailable = errors.unavailable
export type SignInErrorCode = keyof typeof errors

export function isErrorCode(value: unknown): value is SignInErrorCode {
	return typeof value === 'string' && Object.hasOwn(errors, value)
}

export class SignInError extends Error {
	constructor(readonly code: SignInErrorCode) {
		super(code)
	}
}

export async function postAuthJSON(
	path: string,
	body: Record<string, unknown>,
	signal: AbortSignal,
): Promise<unknown> {
	signal.throwIfAborted()
	const res = await fetch(path, {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		credentials: 'same-origin',
		referrerPolicy: 'origin',
		redirect: 'error',
		signal: AbortSignal.any([signal, AbortSignal.timeout(45_000)]),
		body: JSON.stringify(body),
	})
	const data: unknown = await res.json()
	if (!res.ok) {
		const code = data && typeof data === 'object' && 'error' in data ? data.error : undefined
		throw new SignInError(isErrorCode(code) ? code : 'unavailable')
	}
	return data
}
