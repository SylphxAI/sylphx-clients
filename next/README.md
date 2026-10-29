# @sylphx/next

Next.js caching that works when your app runs on more than one replica. It
stores the ISR page cache, the data cache (`fetch`), route handler responses
and `'use cache'` entries in your Sylphx KV namespace, and puts large bodies in
a Sylphx Bucket. Every replica reads the same cache, and `revalidateTag` /
`revalidatePath` on one replica takes effect on all of them.

Works with Next.js 14.1 and later (`cacheHandler`), and Next.js 16
(`cacheHandlers` for `'use cache'`). Node.js runtime only.

## Set up

1. Create a KV namespace and bind it to the Service. The platform then sets
   `REDIS_URL` (also `CACHE_URL`) to the namespace's private Valkey address, so
   a cache lookup stays inside the cluster and is one network round trip.
   Optionally create a Bucket for large pages.
2. Set these environment variables on the Service if you use a Bucket, or a
   namespace that is not bound:

   | Variable | Value |
   | --- | --- |
   | `NEXT_CACHE_BUCKET` | optional: the Bucket id for bodies over 256 KiB. Without it every body stays in KV. Uses the Data API with `SYLPHX_SECRET_KEY` (needs `data:read`, `data:write`) |
   | `NEXT_CACHE_KV_NAME` | only when `REDIS_URL` is not set: the namespace's full name, `orgs/*/projects/*/envs/*/kv_namespaces/*`; its endpoint is fetched once with `connect` |
3. Install and configure:

   ```sh
   npm install @sylphx/next redis            # KV over Valkey
   npm install @sylphx/sdk                    # only for a Bucket or NEXT_CACHE_KV_NAME
   ```

   ```js
   // next.config.js
   module.exports = {
     cacheHandler: require.resolve('@sylphx/next/cache-handler'),
     cacheMaxMemorySize: 0, // keep the per-replica memory cache off so replicas agree
     // Next.js 16 only, for 'use cache':
     cacheHandlers: {
       default: require.resolve('@sylphx/next/use-cache-handler'),
       remote: require.resolve('@sylphx/next/use-cache-handler'),
     },
   }
   ```

   `redis` (node-redis) is used because it is the client the Valkey and Redis
   projects maintain, it pipelines commands on one socket, and it speaks TLS
   (`rediss://`) without extra setup.

## How it works

- Each entry is one KV row. A body larger than `blobThresholdBytes` is written
  to the Bucket and the KV row points to it.
- `revalidateTag(tag)` writes the current time to a KV row for that tag. Every
  `get` reads the tag rows for the entry and treats an entry written before that
  time as stale, so no replica has to be told. A stale page is served once while
  it is rebuilt; a stale `fetch` entry is refetched.
- Entries of a different build are never read, so a new release does not serve
  old HTML that points at removed chunks. The build id is the first that
  exists of `SYLPHX_IMAGE_DIGEST`, `SYLPHX_DEPLOYMENT_ID`,
  `SYLPHX_GIT_COMMIT_SHA`, `.next/BUILD_ID`; with none the server fails to start
  with a clear error (there is no shared default). Tag times are shared.
- A lookup is one `MGET` for the entry and its tags. A replica remembers each
  key's tags after it first reads it; the first read of a key, or of an entry
  whose tags changed, costs a second `MGET`.
- Entries are kept for 30 days (`retentionSeconds`). A page with a `revalidate`
  window is served past that window and rebuilt in the background, on every
  replica, from the entry's own write time; `revalidate: false` never expires.
  Such an entry is kept at least its window plus the retention.
- If KV or the Bucket is unreachable, a read counts as a miss (the page is
  rendered) and a failed write is logged. `revalidateTag` throws, so you learn
  that an invalidation did not land.
- Replica clocks should agree within a second or so; tag and write times are
  compared across replicas.

## Options

To change a default, make your own file and point `cacheHandler` at it:

```js
// cache-handler.mjs
import { createCacheHandler } from '@sylphx/next'
export default createCacheHandler({
  prefix: 'next:web', // give each app sharing a namespace its own prefix
  blobThresholdBytes: 512 * 1024,
  retentionSeconds: 7 * 24 * 3600,
})
```

`kv` and `objects` accept any object with the `KvStore` / `ObjectStore` shape
(see `src/stores.ts`): `redisKv(client)` is the default; `sdkKv(client, namespaceId)`
is the Data API item routes as an alternative, which is also how the tests
run without a network.
Use `createUseCacheHandler` the same way for `cacheHandlers`.

Bodies removed from the Bucket when replaced are deleted on a best-effort
basis. If a replica dies between the two writes an unreferenced object can
remain; set a Bucket lifecycle rule if that matters.

## Customer-branded sign-in (App Router)

The public `@sylphx/next/auth` export provides a Server Component for Next.js
15+ with React 19. Install `@sylphx/sdk@^0.32.1` alongside this package. Auth is
independent of the cache handlers; no Redis connection is needed.

```tsx
// app/sign-in/page.tsx — a Server Component, never add 'use client'
import { SignIn } from '@sylphx/next/auth'

export default async function Page({ searchParams }: {
  searchParams: Promise<{ auth_ticket?: string; auth_error?: string }>
}) {
  const query = await searchParams
  return <>
    {query.auth_error && <p role="alert">Sign-in was not completed. Please try again.</p>}
    <SignIn
      auth={{
        baseUrl: process.env.AUTH_API_ORIGIN!,
        publishableKey: process.env.AUTH_PUBLISHABLE_KEY!,
        secretKey: process.env.AUTH_SECRET_KEY!,
        returnUrl: 'https://shop.example.com/sign-in',
      }}
      ticket={query.auth_ticket}
      appearance={{
        variables: {
          colorPrimary: '#334155', colorText: '#111827',
          colorBackground: '#ffffff', colorDanger: '#b91c1c',
          borderRadius: '12px', fontFamily: 'Inter, sans-serif',
        },
        elements: { root: 'account-card', input: 'account-input',
          button: 'account-submit', socialButton: 'account-provider' },
      }}
    />
  </>
}
```

Configure the instance's brand and enabled methods in Auth. Google and Apple
buttons appear only when the client config reports a usable provider. Add the
app's HTTPS return origin to the instance's redirect allowlist. `baseUrl` and
`returnUrl` must be trusted HTTPS URLs, not values derived from incoming Host
headers or a submitted redirect field. Never prefix `AUTH_SECRET_KEY` with
`NEXT_PUBLIC_` or pass it to a Client Component.

Continue submits the identifier first and renders the returned password or
email-code step. Password users can explicitly request an email code when
that method is enabled. All sign-in paths use server mode. OAuth navigates
through a server action; on return the one-time `auth_ticket` is submitted to
the same server redemption path (a visible button also permits manual submission).
The API secret and the browser User-Agent are sent only from the app server.
The session is stored in `__Host-session`: HttpOnly, Secure, SameSite=Lax,
Path=/, no Domain attribute, with the returned expiry. The bearer is never
returned in action state or put in browser storage. Use HTTPS in development
as well as production. The app can read this cookie from its own server code;
protect authenticated resources there rather than treating rendered UI as authorization.

Branding comes from client config, not a hosted vendor page. There is no vendor
footer or hosted-account link. `appearance.variables` controls colors, corner
radius and font; `appearance.elements` accepts classes for `root`, `logo`,
`title`, `form`, `label`, `input`, `button`, `socialButton`, and `error`.
Labels, browser autocomplete, natural keyboard order, step focus and an error
live region are retained. MFA/captcha challenges require an application-specific
flow; this component fails closed with a generic error rather than returning
a session or displaying raw upstream errors.

Verification: build the sibling SDK first, then in `clients/next` run
`bun install --no-save && bun run test && bun run typecheck && bun run build`.
The auth tests use the generated Continue mock; social tests exercise the
server redirect and returned-ticket contract, not external provider credentials.
