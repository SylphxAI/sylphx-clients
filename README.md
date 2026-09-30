# clients — the generated Sylphx clients

Everything a customer or an app uses to call Sylphx, generated from one schema
([contracts/](../contracts/README.md)) by one generator
([tools/sylphx-gen](../tools/sylphx-gen/README.md)); design:
[resource-api-and-clients.md §8](../docs/specs/one-platform/resource-api-and-clients.md).

| Path | Package | Registry | Hand-written |
| --- | --- | --- | --- |
| `rust/` | `sylphx` | crates.io | `Cargo.toml`, `README.md`, `tests/` (`src/` is generated) |
| `typescript/` | `@sylphx/sdk` | npm (dist-tag `next` until the Access cutover) | `package.json`, `tsconfig*.json`, `README.md`, `test/` (`src/` is generated) |
| `next/` | `@sylphx/next` | npm (dist-tag `next`) | everything: a Next.js cache handler for ISR and `'use cache'` on Sylphx KV and Buckets; it is not generated and calls the SDK |

`generated/` and `src/` of every client are `sylphx-gen` output: never edit
them; change `contracts/` or the generator, then `cargo run -- generate` in
`tools/sylphx-gen`. CI fails when the committed tree differs from the
generator's output.

Releases: `.github/workflows/release-clients.yml` on a `clients-v*` tag
publishes each client whose manifest version is not on its registry yet, and
reads it back. Bump the version in the client's manifest in the same PR as
the change it ships.
