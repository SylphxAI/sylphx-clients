# sylphx CLI

Every Sylphx API method as a command, generated from the one schema, plus a
small hand-written porcelain on the generated Rust SDK.

```sh
curl -fsSL https://github.com/SylphxAI/sylphx-clients/releases/latest/download/install.sh | sh   # or: npm i -g @sylphx/cli · cargo install sylphx-cli
sylphx login                          # approve at sylphx.com/device; the org key goes to the OS keychain
                                      #   (your role's scopes, follows role changes, expires in 30 days; `sylphx logout` revokes it)
sylphx login --api-key -              # agents and CI: an Access key on stdin (or SYLPHX_API_KEY)
sylphx token --scope hosting:deploy   # one short-lived token (15 min) for one scope, on stdout only
sylphx logout                         # revokes the key and forgets it
sylphx link --env orgs/…/envs/…       # defaults for this directory (.sylphx/project.json)
sylphx data databases create main --spec.compute-units 2
sylphx data databases list -o json
sylphx devices run --app app.apk       # a smoke test on a fresh Android device: device-results/
sylphx build run -- cargo test        # this work tree's command on a remote build machine
eval "$(sylphx build cache env)"      # point local sccache and Turbo at the project's shared build cache
sylphx build run --region gra --queue-timeout 120s -- cargo check   # in region gra, or exit 125 after 2 min
sylphx events listen --forward localhost:3000/hook   # a topic's events, signed like a webhook, to a local URL
sylphx data docs search --index docs rate limits    # Markdown docs in a search index: path:line hits
sylphx mcp                            # the MCP server over stdio
sylphx mcp setup                      # register it with Claude Code, Codex, Cursor, VS Code, …
```

- `sylphx <service> <collection> <verb> [NAME|PARENT|ID] [--flags]`: flags
  are the spec fields in dotted kebab-case (`--spec.compute-units`);
  `--from-file` takes a whole request; `--output table|json|yaml|name`.
- A bare id (`main`) expands below the linked env; parents default to the
  linked project, else the key's scope. The link is the nearest
  `.sylphx/project.json` naming an org, project or env; a file in another
  shape (such as `{orgId, projectId}`) is skipped with a warning.
  `SYLPHX_ENVIRONMENT=orgs/…/projects/…/envs/…` (a full name) overrides the
  link for that process, so a CI job or a build wrapper never depends on the
  checkout's `.sylphx/project.json`.
- Mutations wait for their Operation (`--no-wait` to skip); `--dry-run` sets
  `validate_only`; updates send the etag they read and a mask of the flags
  given; destructive calls ask first (`--yes` to skip).
- `sylphx api GET /v1/whoami` is the raw escape hatch.
- `sylphx data docs sync|search|read` keeps a Markdown tree in one of the
  environment's search indexes. `sync [GLOB…] --index ID --revision REV`
  (run it on each merge) writes one document per `#` to `###` section with
  its path, heading and line numbers, then deletes the documents it wrote for
  that source at any other revision (nothing else in the index); a run matching no file is refused. `search QUERY` prints the
  best sections as `path:line` with the matching line; `read PATH[:LINE]
  [--lines A-B]` prints a file or a range of it. `--index` and `--source`
  default to `SYLPHX_DOCS_INDEX` and `SYLPHX_DOCS_SOURCE`, which also give
  `sylphx mcp` its `docs_search` and `docs_read` tools.
- `sylphx events listen [TOPIC] --forward URL` relays a topic (default: the
  environment's whole bus) to a local URL while you develop. It creates a
  temporary Queue and a Subscription of the topic into it, long-polls the
  Queue, and POSTs each event as the CloudEvent JSON body a webhook endpoint
  receives, with `webhook-id`, `webhook-timestamp` and `webhook-signature`
  (Standard Webhooks) under the `whsec_…` secret printed at start: `--secret`,
  else `SYLPHX_LISTEN_SECRET`, else one generated once and kept in the config
  directory (`listen-secret`, 0600). An event the receiver answers is acked
  and printed with its status; one it cannot reach is tried again after 3 s.
  `--types` and `--sources` filter; `--count N` exits after N events; `-o
  json` prints one JSON line per event. Ctrl-C deletes the Queue and the
  Subscription.
- `sylphx devices run` installs an APK on a fresh Android device lease,
  launches it (or a `--game-loop`), watches it with screenshots, and writes
  `result.json`, `junit.xml`, `logcat.txt`, and `crash.txt` into `--out`
  (`device-results/`). Exit 0 passed, 1 failed, 2 infrastructure; the lease is
  released on the way out unless `--keep`.
- `sylphx build run [PATH] -- <command…>` runs the command on a remote build
  machine against this git work tree and behaves like the local command: the
  output streams back, the exit code is the command's own, and `--artifact
  GLOB` files are copied into `--out` (`.sylphx/out/<run>/`). A warm workspace
  (a pool of up to 10 Volumes per project and repository) keeps the tree,
  `target/`, the Cargo registry, sccache and toolchains, so a repeat run sends
  only the changed files. The region's workspaces are a cache: when it has no
  room for a new one (`NO_CAPACITY`), the run deletes the least recently used
  free workspace of any repository once and tries again. Exit codes: the command's own, 2 usage, 124
  `--timeout`, 125 platform failure (`retryable` in the `-o json` `result`
  event; anything that fails before the command starts, such as the sync or the
  toolchain install or a toolchain that fails its check, is 125, and a machine
  lost or a stream broken before the command starts is retried once after a
  short backoff first), 130 interrupted, 137 the machine was lost after the
  command started. A started command is never run twice: a broken output
  stream is reattached to the running command. Cargo builds as CI does
  (`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`) unless `--env` sets
  either. `--region REGION` runs in that region's Cell with
  that region's own warm workspaces (default: the project's home region); a
  region whose Cell offers no Volumes refuses the workspace at once, and the
  run builds on the machine's own disk instead (always cold, nothing left
  behind, no local sccache); `--queue-timeout DURATION` (default 30m) is the longest wait for a machine,
  after which the run releases its lease and exits 125, retryable, so a caller
  can try another region. `-o json` writes NDJSON events (`queued`,
  `running`, `sync`, `stdout`, `stderr`, `artifact`, `result`); `--dry-run`
  prints what a cold run would send. Files: `git ls-files` (tracked and
  untracked, not ignored) minus `.sylphxignore`.
- Cargo git dependencies come with the run: every git commit a `Cargo.lock`
  of the tree locks is fetched here with your git credentials (depth 1, kept
  under the repository's git directory), sent to a mirror on the workspace
  only when it lacks it (`git_deps` event), and Cargo's fetch reads that
  mirror, so a private dependency builds with no egress and no token on the
  machine. One that cannot be fetched here stops the run with 125 before a
  machine is leased. Git prints `warning: rejected refs/commit/… because
  shallow roots are not allowed to be updated` once per new commit; the
  build is unaffected.
- The run also gets the project's shared build cache (sccache, Turbo): the
  CLI mints a cache token with your key before the command starts and merges
  the cache's environment into the command's (your `--env` wins). If the
  cache cannot be reached the run builds without it and prints one
  `sylphx: warning: build cache unavailable (…)` line; `--no-cache` skips it.
  The same holds when sccache's server will not start on the machine because
  the cache stops answering after the token is minted: the command then runs
  without `RUSTC_WRAPPER` and the warning says so, rather than every compile
  failing with sccache's exit 2.
  The token is never printed, logged or put in an event.
- `sylphx build cache env [--project ID] [-o json]` mints a read-only token
  (12 h) and prints `export NAME='value'` lines for a POSIX shell (plus
  `export RUSTC_WRAPPER=sccache` when sccache is installed and the variable
  is unset); `-o json` prints the environment as an object. The output holds
  a credential. Exit 0; 2 usage; 125 platform failure. The gateway is
  `https://build-cache.sylphx.net`, or `SYLPHX_BUILD_CACHE_URL`.

## `sylphx ai top`

The operator live view of the AI gateway's seats, replacing `janus status`,
`top`, `doctor` and `capacity`. It is read-only over `GET /v1/operator/seats`
and needs a platform key with `ai:operator:seats:read` (`--base-url` points it
at the gateway if `api.sylphx.com` does not route it).

```sh
sylphx ai top            # a terminal: refreshes every 10 s (--interval), Ctrl-C quits
sylphx ai top --once     # print once (also the default off a terminal)
sylphx ai top --json     # one JSON document (same as -o json)
```

It shows usable, spent and out-of-rotation seats; when the next spent seat
comes back and the earliest weekly reset; and, at the 24h and 6h pace, the
demand, seats needed, seats to add (with 25% headroom) and the simulated
runway. Anything that needs a person is a red line: a seat that needs a login
(`reauth_required`), is on hold (`subscription_required`), is quarantined or
has no fresh reading, no usable seat, a pool that runs dry, seats to add.

The pace is measured from the weekly-window readings each run keeps in
`<config dir>/ai-top-history.json` (a week, at most one reading per 4
minutes), so it reads `n/a` until about 6 hours of readings exist; leave a
terminal open or run `--once` from a timer. Sessions and subagents (seat,
model, effort, cache hit) and the API-equivalent value read `n/a` until the
gateway's receipts carry them; `--json` lists the missing fields.

## `sylphx token --scope <scope>`

Prints one token and a newline on stdout, for tools that need a credential for
a single scope (cargo, deploy scripts). Everything else goes to stderr; the
exit code is non-zero, with a one-line reason, when you are not signed in, the
login may not grant the scope, or the scope is not registered.

- The token is a child Access key in your login's own org/project/env, with
  only that scope and a 15 minute life (label `token:<scope>`). Your login
  key is never printed. Scopes are the registered ones (`packages:read`,
  `packages:publish`, `hosting:deploy`, `ai:inference`, ...); an unregistered scope is refused.
- It is cached in `<config dir>/token-cache/` (mode 0600) per login and
  scope, and reused until 2 minutes before it expires, so a cargo build mints
  one key, not one per crate. `sylphx logout` clears the cache.
- With `SYLPHX_API_KEY` (CI) it mints from that key the same way. A key that
  may not mint keys is printed as-is only when it carries the scope and
  expires within 60 minutes (the key a GitHub Actions OIDC exchange issues);
  otherwise the command refuses.
- A login that may not grant the scope is refused; a per-scope step-up device
  login needs server support that is not there yet.

```toml
# .cargo/config.toml
[registries.example]
credential-provider = ["cargo:token-from-stdout", "sylphx", "token", "--scope", "packages:read"]
```

Use `--scope packages:publish` for publishing. Neither is covered by the
`*:read` / `*:write` wildcards, so a login must hold them by name.

`generated/commands.json` is `sylphx-gen` output; never edit it.

## Container image

`clients/cli/Dockerfile` builds the CLI into a distroless image (no shell,
CA certificates, uid 65532, writable `/tmp`); the Release v2 apply Job runs
`sylphx apply ...` from it inside tenant environments. The build context is
the repository root: `docker build -f clients/cli/Dockerfile .`.

`.github/workflows/cli-image.yml` publishes it on every push to `main` that
changes anything under `clients/`, as `registry.sylphx.com/library/sylphx-cli:<commit sha>`, and
prints the digest reference in the run summary. Tenant namespaces pull it with
the platform `registry-pull-secret`.

The release controller pins the image by digest through one environment
variable, set in `SylphxAI/infra` (`infra/addons/release-controller`, the
controller Deployment's `env`):

```yaml
- name: SYLPHX_APPLY_CLI_IMAGE
  value: registry.sylphx.com/library/sylphx-cli@sha256:<digest from the run summary>
```
