# @sylphx/sdk

The Sylphx SDK for TypeScript and JavaScript: one API
(`https://api.sylphx.com`), one key, one namespace per service. Zero runtime
dependencies (`fetch` only); runs on Node 20+, Bun, Deno, workers, and
browsers (browsers with publishable keys only).

```sh
npm install @sylphx/sdk
```

```ts
import { Sylphx } from '@sylphx/sdk'

const sylphx = new Sylphx() // reads SYLPHX_API_KEY (and SYLPHX_BASE_URL)
const me = await sylphx.access.whoami({})
for await (const project of sylphx.access.projects.listAll({ parent: me.org })) {
	console.log(project.name)
}
```

- Every service is a namespace (`sylphx.data`, `sylphx.hosting`, …) and every
  collection an object on it (`sylphx.data.databases`), with `get`, `list`,
  `listAll`, `create`, `update`, `delete`, and each custom method.
- Types are camelCase; the wire is snake_case JSON, converted by a generated
  codec.
- Mutations of reconciled Resources return an `Operation`; `await op.wait()`
  resolves to the settled Resource.
- Errors are `SylphxError` (`code`, `status`, `retryable`, `effect`, `detail`,
  `requestId`) from the one RFC 9457 problem body.
- Retryable answers are retried with full-jitter backoff, honouring
  `Retry-After`; each logical mutation sends one `Idempotency-Key`.
- `sylphx.invoke('data.databases.get', { name })` calls any method by id with
  wire JSON.
- In a browser, use the environment's publishable key
  (`SYLPHX_PUBLISHABLE_KEY`, delivered by the platform to every service and
  safe to ship in a bundle). It calls only browser-safe methods, such as error
  capture:

  ```ts
  const sylphx = new Sylphx({ apiKey: PUBLISHABLE_KEY }) // e.g. from a public build-time variable
  window.addEventListener('error', (e) =>
  	sylphx.observability.errorGroups.capture({
  		parent: 'orgs/-/projects/-/envs/-',
  		errorEvent: { exceptionType: e.error?.name ?? 'Error', message: e.message, stack: e.error?.stack, release: RELEASE },
  	}),
  )
  ```
- Some services also have a data plane served at their own host, with the same
  key: Sylphx Data's objects, key-value entries, and search documents are
  `sylphx.data.objects`, `sylphx.data.kv` (strings and counters, plus
  `getMany`, hashes, lists, sorted sets, `scan`, and `expire`),
  `sylphx.data.documents`, and `sylphx.data.search` (at
  `https://api.data.sylphx.com`). Every such call
  carries one `Sylphx-Effect-Id`, reused on its retries, so a retried write is
  applied once.
- `SYLPHX_URL` and `SYLPHX_SECRET_URL` (per-project `<project>.api.sylphx.com`
  hosts, retired on 2026-09-03) are not read; a base URL on such a host is
  ignored with a warning.
- Per-service subpath imports tree-shake: `import { DataApi } from '@sylphx/sdk/data'`.

```ts
// Bytes travel base64 (`body`, `value`, `documentJson`); the bucket, namespace,
// and index are Data Resources of the environment (`sylphx.data.buckets`,
// `sylphx.data.kvNamespaces`, `sylphx.data.searchIndexes`).
await sylphx.data.objects.put({ bucketId: 'uploads', key: `${orgId}/cv.pdf`, body: base64, contentType: 'application/pdf' })
const { body } = await sylphx.data.objects.get({ bucketId: 'uploads', key: `${orgId}/cv.pdf` })
const hits = await sylphx.data.kv.increment({ namespaceId: 'ratelimit', key: `ip:${ip}` })
await sylphx.data.kv.zsetAdd({ namespaceId: 'board', key: 'weekly', members: [{ member: userId, score: 120 }] })
const { score } = await sylphx.data.kv.zsetScore({ namespaceId: 'board', key: 'weekly', member: userId })
await sylphx.data.documents.put({ indexId: 'articles', documentId: slug, documentJson: btoa(JSON.stringify(article)) })
const { hits: found } = await sylphx.data.search.query({ indexId: 'articles', query: 'pricing', limit: 10 })
```

This package is generated from the Sylphx schema registry by `sylphx-gen`; do
not edit it by hand. Source: `clients/typescript` in the Sylphx monorepo,
published from its MIT mirror.

## Browser sign-in: `createAuthClient`

A small client for Sylphx Auth's client API (`/v1/client/*`) in a browser app.
It holds only a publishable key (`sylphx_pk_...`, public by design); a secret
key given to it throws.

```ts
import { createAuthClient } from '@sylphx/sdk/auth/client' // also exported from '@sylphx/sdk'

const auth = createAuthClient({ publishableKey: 'sylphx_pk_live_...' })
const config = await auth.config() // methods, social providers, branding
const { session, user } = await auth.signInWithPassword({ email, password })
await auth.getSession()
await auth.signOut()

// Social sign-in is a navigation; the callback appends ?auth_ticket=...
// (config().ticketParam names it)
location.assign(auth.oauthStartUrl('google', { redirectUrl: location.href }))
await auth.redeemTicket(new URLSearchParams(location.search).get('auth_ticket')!)
```

- Every call carries `publishable_key` (or `instance`, the public slug) in the
  query string, same-origin or not: a browser preflight has no `Authorization`,
  and the query is where Auth learns the environment before it answers CORS.
  An existing query is kept and the parameter is never duplicated. The key is
  never sent as a Bearer.
- Options: `publishableKey` or `instance` (exactly one), `baseUrl`
  (default `https://api.sylphx.com`), `fetch`, `credentials` (default `'omit'`;
  `'include'` is opt-in), `sessionToken` (to restore a session).
- Add your app's origin to `allowed_origins` on the environment's Auth Config
  to make browsers refuse every origin you did not name.
- Errors are `AuthClientError` subclasses keyed on the service's `code`
  (`AuthUnauthorizedError`, `AuthMfaRequiredError`, `AuthRateLimitedError`
  with `retryAfterSeconds`, `AuthNetworkError`, ...), not `SylphxError`.

### The session is a bearer token in JavaScript (XSS trade-off)

There is no cookie mode. The client asks for `session_mode: "browser"`, so the
session token is a bearer value that page script holds. A script injected into
your page (XSS) can read it. Keep the token out of URLs and logs, and keep a
strict Content-Security-Policy.

If your app has a backend and needs an HttpOnly cookie, use the server (BFF)
pattern instead: the page only ever holds a one-time ticket, and your server
redeems it with the secret key and sets the cookie on your own origin.

```ts
// Browser: sign in with the default server mode; hand the ticket to your backend.
const res = await fetch(`https://api.sylphx.com/v1/client/sign-in/password?publishable_key=${pk}`, {
	method: 'POST',
	headers: { 'content-type': 'application/json' },
	body: JSON.stringify({ email, password }), // session_mode defaults to "server"
})
const { ticket } = await res.json()
await fetch('/auth/session', { method: 'POST', body: JSON.stringify({ ticket }) })

// Your server (secret key stays here; forward the browser's User-Agent):
const redeemed = await fetch('https://api.sylphx.com/v1/client/tickets:redeem', {
	method: 'POST',
	headers: {
		authorization: `Bearer ${process.env.SYLPHX_AUTH_SECRET_KEY}`,
		'content-type': 'application/json',
		'user-agent': request.headers.get('user-agent') ?? '',
	},
	body: JSON.stringify({ ticket }),
})
const { session } = await redeemed.json()
// Set-Cookie: session=<session.token>; HttpOnly; Secure; SameSite=Lax
```

`@sylphx/nextjs` ships this pattern as a route handler.
