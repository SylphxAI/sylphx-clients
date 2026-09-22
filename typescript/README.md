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
- Per-service subpath imports tree-shake: `import { DataApi } from '@sylphx/sdk/data'`.

This package is generated from the Sylphx schema registry by `sylphx-gen`; do
not edit it by hand. Source: `clients/typescript` in the Sylphx monorepo,
published from its MIT mirror.
