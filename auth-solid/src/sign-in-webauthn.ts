export function decodeBase64Url(value: string) {
	return Uint8Array.from(atob(value.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0))
}

export function encodeBase64Url(value: ArrayBuffer) {
	return btoa(Array.from(new Uint8Array(value), (byte) => String.fromCharCode(byte)).join(''))
		.replace(/\+/g, '-')
		.replace(/\//g, '_')
		.replace(/=+$/, '')
}
