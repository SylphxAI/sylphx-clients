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
- Entries are kept for 30 days (`retentionSeconds`). Next.js decides when a
  page is due for revalidation from its own `revalidate` time, so a stale page
  stays available to serve while it is rebuilt.
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
