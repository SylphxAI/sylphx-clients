import assert from 'node:assert/strict'
import { execFile } from 'node:child_process'
import { readFile, writeFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { promisify } from 'node:util'
import { generateHydrationScript, renderToString } from 'solid-js/web'
import { build } from 'vite'
import solid from 'vite-plugin-solid'

const widget = 'auth-package-smoke-solid.tsx'
const common = () => ({ configFile: false, plugins: [solid({ ssr: true })] })
await build({
	...common(),
	build: {
		ssr: widget,
		outDir: 'out-ssr',
		rollupOptions: { output: { entryFileNames: 'entry.mjs' } },
	},
})
const { page } = await import(pathToFileURL(resolve('out-ssr/entry.mjs')).href)
const rendered = renderToString(page)
assert.match(rendered, /<section[^>]*class="auth-sign-in-panel"/)
assert.match(rendered, /<h1[^>]*>Package sign-in fixture<\/h1>/)
assert.match(rendered, /Continue with Google/)
assert.match(rendered, /Email me a code/)
assert.match(rendered, /Use password instead|Use password/)
assert.match(rendered, /Last used/)
assert.match(rendered, /<button[^>]*type="submit"[^>]* disabled>/)
assert.match(rendered, /<input[^>]*autocomplete="username webauthn"/)
assert.match(rendered, /<form[^>]*action="\/account\/auth\/email"/)
assert.doesNotMatch(rendered, /Loading sign-in/)

// Hydrate the very markup emitted by the installed server entry and read the
// installed stylesheet through its public subpath, with no resolver aliases.
await writeFile(
	'index.html',
	`<!doctype html><html><head><meta charset="utf-8">
${generateHydrationScript()}</head><body><main id="root">${rendered}</main>
<script type="module">
import { hydrate } from 'solid-js/web'
import { page } from './${widget}'
import '@sylphx/auth-solid/sign-in.css'
const root = document.getElementById('root')
const ssr = root.querySelector('section')
const field = ssr.querySelector('input[autocomplete="username webauthn"]')
const requests = []
let conditionalSignal
let explicitSignal
let verify
let enrollmentAttempts = 0
let enrolled
navigator.credentials.create = async ({ publicKey }) => {
	if (publicKey.rp.id !== location.hostname || publicKey.authenticatorSelection.userVerification !== 'required' || publicKey.authenticatorSelection.residentKey !== 'required') {
		throw new Error('Wrong enrollment parameters')
	}
	const bytes = new Uint8Array([1, 2, 3]).buffer
	const response = Object.create(AuthenticatorAttestationResponse.prototype)
	Object.defineProperties(response, { clientDataJSON: { value: bytes }, attestationObject: { value: bytes }, getTransports: { value: () => ['internal'] } })
	const credential = Object.create(PublicKeyCredential.prototype)
	Object.defineProperties(credential, { rawId: { value: bytes }, response: { value: response } })
	return credential
}
PublicKeyCredential.isConditionalMediationAvailable = async () => true
navigator.credentials.get = async ({ mediation, signal, publicKey }) => {
	if (publicKey.allowCredentials.length || !publicKey.challenge.byteLength) throw new Error('Not a discoverable challenge')
	if (mediation === 'conditional') {
		conditionalSignal = signal
		return new Promise((_resolve, reject) => signal.addEventListener('abort', () => reject(signal.reason), { once: true }))
	}
	explicitSignal = signal
	const bytes = new Uint8Array([1, 2, 3]).buffer
	const response = Object.assign(Object.create(AuthenticatorAssertionResponse.prototype), {})
	for (const key of ['clientDataJSON', 'authenticatorData', 'signature', 'userHandle']) {
		Object.defineProperty(response, key, { value: bytes })
	}
	const assertion = Object.create(PublicKeyCredential.prototype)
	Object.defineProperties(assertion, { rawId: { value: bytes }, response: { value: response } })
	return assertion
}
window.fetch = async (url, init) => {
	const data = JSON.parse(init.body)
	requests.push({ url, data })
	if (url === '/account/auth/passkey/enroll') {
		if (data.action === 'begin') {
			if (++enrollmentAttempts === 1) return Response.json({ error: 'method_unavailable' }, { status: 403 })
			return Response.json({ challenge: 'AQID', challengeId: 'enroll-challenge', rpId: location.hostname, userId: 'AQID', userName: 'fixture@example.test', algorithms: [-7] })
		}
		if (data.action === 'verify') {
			enrolled = data
			return Response.json({ enrolled: true })
		}
	}
	if (url !== '/account/auth/email' || data.acceptance?.terms !== 'v2') throw new Error('Wrong passkey request')
	if (data.action === 'otp_start') return Response.json({ error: 'unavailable' }, { status: 503 })
	if (data.action === 'recovery_start') return Response.json({ step: 'recovery_sent' }, { status: 202 })
	if (data.action === 'passkey_start') {
		return Response.json({ challenge: 'AQID', challengeId: 'fixture-challenge', rpId: 'localhost', credentials: [] })
	}
	if (data.action === 'passkey_verify') {
		verify = data
		return Response.json({})
	}
	throw new Error('Unexpected request')
}
const until = (predicate) => new Promise((resolve, reject) => {
	const check = () => predicate() ? resolve() : setTimeout(check, 15)
	check()
	setTimeout(() => reject(new Error('Browser assertion deadline')), 3000)
})
hydrate(page, root)
try {
	const buttonNamed = (text, scope = ssr) => [...scope.querySelectorAll('button')].find((item) => item.textContent.trim().startsWith(text))
	const checkbox = ssr.querySelector('input[type=checkbox]')
	const emailForm = field.form
	if (!buttonNamed('Email me a code').disabled || !buttonNamed('Continue with Google').disabled) throw new Error('Agreement did not disable auth controls')
	emailForm.dispatchEvent(new SubmitEvent('submit', { bubbles: true, cancelable: true }))
	await until(() => ssr.textContent.includes('Accept the current terms'))
	if (requests.length) throw new Error('An unaccepted agreement sent auth traffic')
	checkbox.click()
	await until(() => conditionalSignal)
	if (ssr !== root.querySelector('section') || field !== root.querySelector('input[type=email]') || getComputedStyle(ssr).display !== 'grid') {
		throw new Error('SSR markup was replaced or package stylesheet is inactive')
	}
	const button = [...ssr.querySelectorAll('button')].find((item) => item.textContent === 'Use a passkey')
	button.click()
	await until(() => verify)
	if (!conditionalSignal.aborted || !explicitSignal || requests.length !== 3 || verify.credential_id !== 'AQID' || verify.challenge_id !== 'fixture-challenge') {
		throw new Error('Explicit passkey did not abort Conditional UI and verify the assertion')
	}
	const _setEmail = () => {
		field.value = 'fixture@example.test'
		field.dispatchEvent(new InputEvent('input', { bubbles: true }))
	}
	_setEmail()
	buttonNamed('Email me a code').click()
	await until(() => ssr.textContent.includes('We couldn’t connect'))
	buttonNamed('Use password').click()
	await until(() => ssr.querySelector('input[type=password]'))
	if (requests.filter(({ data }) => data.action === 'otp_start').length !== 1) throw new Error('Password fallback required an OTP retry')
	buttonNamed('Forgot your password?').click()
	buttonNamed('Send reset link').click()
	await until(() => ssr.textContent.includes('If an account exists for this address'))
	const enroll = buttonNamed('Adicionar uma chave de acesso', root)
	if (!enroll) throw new Error('Missing localized enrollment action')
	enroll.click()
	await until(() => root.textContent.includes('Este método de login não está disponível'))
	if (enrolled) throw new Error('Rejected enrollment produced a credential')
	enroll.click()
	await until(() => root.textContent.includes('Chave de acesso adicionada'))
	if (enrolled?.challenge_id !== 'enroll-challenge' || enrolled?.attestation_object !== 'AQID' || enrolled.transports[0] !== 'internal') throw new Error('Enrollment lost the attestation')
	document.body.dataset.authPackageSmoke = 'passed'
} catch (error) {
	document.body.dataset.authPackageSmoke = String(error)
}
</script></body></html>`,
)
const bundle = await build({ ...common(), build: { outDir: 'out-browser' } })
const output = [bundle].flatMap((result) => result.output)
const manifest = JSON.parse(await readFile('node_modules/@sylphx/auth-solid/package.json', 'utf8'))
const widgetDir = 'node_modules/@sylphx/auth-solid'
const modules = new Set(
	output.flatMap((result) => (result.type === 'chunk' ? result.moduleIds : [])),
)
assert.ok(
	modules.has(resolve(widgetDir, manifest.exports['.'].browser)),
	'Vite did not select browser export',
)
assert.ok(
	!modules.has(resolve(widgetDir, manifest.exports['.'].default)),
	'Vite bundled the SSR export',
)
assert.ok(
	output.some(
		(file) => file.fileName.endsWith('.css') && String(file.source).includes('.auth-sign-in-panel'),
	),
	'CSS missing from production bundle',
)
console.info(
	'Solid: Vite SSR first-frame Google/email and WebAuthn input; browser export and CSS included',
)

const chromium = process.argv[2]
if (chromium) {
	const route = new Map([
		['/', ['text/html; charset=utf-8', await readFile('out-browser/index.html')]],
	])
	for (const file of output) {
		if (file.type === 'chunk' || file.fileName.endsWith('.css')) {
			route.set(`/${file.fileName}`, [
				file.type === 'chunk' ? 'text/javascript' : 'text/css',
				await readFile(`out-browser/${file.fileName}`),
			])
		}
	}
	const server = createServer((request, response) => {
		const file = route.get(request.url)
		if (!file) return response.writeHead(404).end()
		response.writeHead(200, { 'Content-Type': file[0], 'Cache-Control': 'no-store' }).end(file[1])
	})
	await new Promise((listen, reject) => server.once('error', reject).listen(0, '127.0.0.1', listen))
	try {
		const { stdout } = await promisify(execFile)(
			chromium,
			[
				'--headless',
				'--disable-background-networking',
				'--no-first-run',
				`--user-data-dir=${resolve('chromium-profile')}`,
				'--virtual-time-budget=10000',
				'--dump-dom',
				`http://127.0.0.1:${server.address().port}/`,
			],
			{ timeout: 30_000 },
		)
		assert.match(stdout, /<body data-auth-package-smoke="passed">/)
		console.info(
			'Chromium: hydrated seeded Google/email HTML without refetch; verified agreement gate, passkey assertion and enrollment, OTP 503 password fallback, and recovery',
		)
	} finally {
		server.closeAllConnections()
		await new Promise((close) => server.close(close))
	}
}
