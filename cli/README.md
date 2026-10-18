# sylphx CLI

Every Sylphx API method as a command, generated from the one schema, plus a
small hand-written porcelain on the generated Rust SDK.

```sh
curl -fsSL https://github.com/SylphxAI/sylphx-clients/releases/latest/download/install.sh | sh   # or: npm i -g @sylphx/cli · cargo install sylphx-cli
sylphx login                          # approve selected orgs at sylphx.com/device; one role-scoped key per org
                                      #   keys go to the OS keychain (or an explicitly chosen 0600 file), never stdout
sylphx login --api-key -              # agents and CI: an Access key on stdin (or SYLPHX_API_KEY)
sylphx token --scope hosting:deploy   # one short-lived token (15 min) for one scope, on stdout only
sylphx org use acme                   # active org when no --org, resource name, or linked context selects another
sylphx whoami --org beta              # use beta's stored credential for this command only
sylphx logout                         # revoke and forget the selected org; keep other org logins
sylphx logout --all                   # revoke and forget every stored credential
sylphx auth status                   # first use discovers/provisions the default project and production environment
sylphx link --env orgs/…/envs/…       # optional advanced target override for this directory
sylphx data databases create main --spec.compute-units 2
sylphx data databases list -o json
sylphx devices run --app app.apk       # a smoke test on a fresh Android device: device-results/
sylphx build run -- cargo test        # this work tree's command on a remote build machine
sylphx build image                     # build and publish to the linked project; no registry setup
eval "$(sylphx build cache env)"      # point local sccache and Turbo at the project's shared build cache
sylphx build run --size standard -- cargo check   # home region; keep the default 30m cold-fallback wait
sylphx events listen --forward localhost:3000/hook   # a topic's events, signed like a webhook, to a local URL
sylphx data docs search --index docs rate limits    # Markdown docs in a search index: path:line hits
sylphx mcp                            # the MCP server over stdio
sylphx mcp setup                      # register it with Claude Code, Codex, Cursor, VS Code, …
```

- Login needs no organization up front: select one, several, or all memberships
  in the browser. `sylphx login --org acme` locks approval to that organization.
  The CLI stores every issued key separately; there is no cross-tenant key.
  Commands choose credentials by `--org` (slug, id, or `orgs/{id}`), a full
  resource name, `SYLPHX_ENVIRONMENT` / the nearest project link, then the active
  org set by `sylphx org use <slug>`. Switching active org does not rewrite a
  checkout's link; specify the project/environment when working in another org.
  `--key` / `--api-key` and `SYLPHX_API_KEY` still override stored credentials.
  Existing single-key credentials files and keychain entries remain readable.
  Re-login merges approved org credentials without deleting other orgs;
  `logout --org <slug>` removes just that org, while `logout --all` removes all.
- `sylphx <service> <collection> <verb> [NAME|PARENT|ID] [--flags]`: flags
  are the spec fields in dotted kebab-case (`--spec.compute-units`);
  `--from-file` takes a whole request; `--output table|json|yaml|name`.
- First use follows the Vercel/Railway project-discovery pattern. After sign-in,
  an environment-scoped command (including `auth status`) discovers the
  selected org's `default` project and its `production` environment. When
  there is no standard target, a single existing target is reused; an empty
  collection is provisioned through public Access. Creation uses stable
  idempotency keys, and a concurrent slug conflict is rediscovered.
  The resolved full names are kept in `.sylphx/project.json`; reruns reuse them.
  Automatic persistence is best-effort: if the destination cannot be written,
  the command warns and uses the resolved context in memory; subsequent runs
  rediscover it. An explicit `sylphx link` still fails if it cannot write the link.
  Build's project-scoped commands bootstrap the project the same way.
  Sylphx's own projects use this same public path, with no tenant special case.
- Multiple existing projects/environments without a standard default require
  a choice: the error lists their full names. Select one with
  `sylphx link --project web --env production`, or pass an explicit full name.
  Discovery never crosses the selected org/project, and `--org` does not
  overwrite a checkout's link to another organization.
- Linking follows [Vercel project linking](https://vercel.com/docs/cli/link):
  an explicit target is authoritative, not a hint. `link --env` accepts a full
  environment name without `--project`; a slug or environment id uses the
  selected project. `--project` accepts a slug, id, or exact project name.
  Conflicting parents and malformed names fail before the link is written.
  Without target flags, linking reuses `SYLPHX_ENVIRONMENT`, then the nearest
  link in the selected organization, then the credential scope and public
  discovery above. Changing project clears the old environment; changing only
  environment retains the selected project. No list-order choice is made.
  A successful link writes all three full names (`org`, `project`, `env`) to
  `.sylphx/project.json`; an incomplete selection is never reported as success.
  Explicit linking intentionally persists the selected process context; other
  commands never persist a process-only override. Build reads this same context
  and leaves machine shape, region, grants, and admission to the public platform.
- A bare id (`main`) expands below the resolved environment. Explicit resource
  names and `--env` remain advanced overrides; `SYLPHX_ENVIRONMENT=orgs/…/projects/…/envs/…`
  overrides the nearest link for that process; only an explicit `link` persists it. A link
  in another shape (such as `{orgId, projectId}`) is skipped with a warning.
- Mutations wait for their Operation (`--no-wait` to skip); `--dry-run` sets
  `validate_only`; updates send the etag they read and a mask of the flags
  given; destructive calls ask first (`--yes` to skip).
- `sylphx apply` reads `sylphx.toml` (or `--manifest PATH`, including a Release's
  `declarations.json`) and uses the shared Resource planner and writer.
  `--offline` checks without a key or network; `--dry-run` plans without
  writing; `-o json` prints the Resource actions and unsettled names.
  `--detailed-exitcode` returns 0 for no changes, 2 for changes, and 1 for
  errors. `--timeout SECONDS` defaults to 600 per Resource. `--release ID`
  and `--source REPO@SHA:PATH` record provenance; a Release run also writes
  its compact JSON result to `/dev/termination-log`. `--force` allows a
  standalone run to write a Resource managed by a Release. Eligible kinds
  are listed in `clients/apply/declarable.toml`; this list remains empty
  until each kind's writer cutover is complete.
- `sylphx build image [PATH]` publishes to a managed project repository by
  default and returns an immutable Artifact name and a digest-pinned
  `source_ref` for Hosting. Its registry credentials are minted on the public
  customer API, kept only in the lease's private directory, renewed during
  long builds, and scrubbed from output. `--push REF --registry-auth FILE`
  overrides that default for an external registry; `--oci-out FILE` copies
  the OCI archive back. Neither override is a prerequisite.
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
- Build workspaces are owned by the admitted lease, not a persistent client
  pool. A queued Build holds no workspace PVC reservation. After Kueue admits
  the suspended Job, Kubernetes creates the Pod's generic ephemeral raw-block
  workspace claim; lease end removes it through normal workload garbage
  collection after the guest unmounts. The catalog retains the current 64Gi
  workspace request. Explicit customer Sandboxes Volumes are unchanged.
- `sylphx build run [PATH] -- <command…>` runs the command on a remote build
  machine against an ordinary source directory or its enclosing Git work tree
  when present. No Git initialization or manifest is required. It behaves like
  the local command: output streams back, the exit code is the command's own,
  and `--artifact
  GLOB` files are copied into `--out` (`.sylphx/out/<run>/`). Every lease gets
  a fresh `/workspace`, including with an explicit `--region`; the CLI never
  lists, creates, evicts or attaches a workspace Volume before admission.
  Cross-lease `target/`, local toolchains and BuildKit state are not retained.
  The existing scoped remote build cache and dependency mirrors remain;
  they do not replace all local BuildKit layers or mutable cache mounts.
  Fresh workspaces materialize source from content-addressed tree packs in
  the Build store: the CLI checks which packs are absent and sends only those.
  Versioned manifest keys reuse unchanged archives across clients; path-anchored
  boundaries keep edits local. No cache or infrastructure setup is required.
  Packs are verified before publication; missing/corrupt packs, links and unusual
  filenames use direct compressed transfer. `--no-cache` uses that direct path
  for the whole tree. This applies to both `build run` and `build image`.
  Exit codes: the command's own, 2 usage, 124
  `--timeout`, 125 platform failure (`retryable` in the `-o json` `result`
  event; sync or known toolchain setup failures are 125). A lost machine or
  broken sync stream before the guest action launches is retried once after a
  short backoff. Once the action launches it is never replayed: a broken stream
  reattaches to that process, and losing it is 137 because the command may have
  run even if its start marker was lost. Reattachment recovers admission from
  a per-action guest monotonic timestamp; it restores `running` and the original
  command deadline without treating marker-free setup exits as user exits.
  Setup diagnostics precede `running`; the command's own status remains its
  exit code. Cargo uses
  `CARGO_INCREMENTAL=0` with sccache when a remote cache is available, and
  `CARGO_PROFILE_DEV_DEBUG=0`; `--env` overrides these defaults and an explicit
  compiler wrapper is preserved. `--region REGION` pins placement, not a
  persistent workspace. Automatic placement remains the Sandboxes API's
  responsibility. `--queue-timeout DURATION` (default 30m) is the longest wait
  for a machine. Keep the default budget for admission and cold boot; override
  it only when the caller needs a different wait limit. The budget covers
  creating the lease (or, with `--no-tree`, the Build) and the SDK's
  retries of retryable API answers while doing so, not just the wait for
  READY. Past it the run releases its lease and exits 125, retryable, with
  the server's own message when the API answered, so a caller can try
  another region. `-o json` writes NDJSON events (`queued`,
  `running`, `sync`, `stdout`, `stderr`, `artifact`, `result`); `--dry-run`
  prints what a cold run would send. Git sources include `git ls-files`
  (tracked and untracked, not ignored); ordinary directories include files
  under PATH, including hidden files, honoring nested `.gitignore` files.
  Optional `.sylphxignore` (Gitignore syntax) subtracts files from either
  source type; negations do not reinclude Git-ignored files. Git metadata and
  local `.sylphx` output/state are not uploaded from ordinary directories.
  Both source types use the same content hashing, CAS transfer, and lease
  execution path. For ordinary directories, stat and pack-action indexes live
  outside the source under the user cache directory, keyed by canonical PATH;
  unchanged inputs reuse digests and archives, with CAS presence checked each run.
  `--region`, `--size`,
  `--env`, `--no-cache`, and `--no-tree` remain optional advanced overrides.
  `--source-history` still requires actual Git history.
- Ungranted Cargo git dependencies come with the run: each locked commit is
  fetched here with your git credentials (depth 1, no tags).
  Like Cargo's locked source cache, immutable URL-and-commit archives live in
  the existing per-source user cache (`$XDG_CACHE_HOME/sylphx/build/sources`,
  otherwise `~/.cache/sylphx/build/sources`), outside the source Git directory.
  Fetches run in a fresh owned bare repository, never the caller's repository
  or shared history. Only the pinned commit and its tree travel, not unrelated
  refs, ancestor history, or Git credentials. The archive is uploaded to the
  cold lease (`git_deps` event) and installed by the guest action. Cargo's fetch
  reads those mirrors, with no git-host egress or credential on the machine.
  A lock that names the `ssh://` spelling of a GitHub repository is
  retried over `https://` when the machine has no ssh key for it, since the two
  spellings name the same repository. One that cannot be fetched here stops the
  run with 125 before the command starts. Git prints `warning: rejected
  refs/commit/… because shallow roots are not allowed to be updated` once per
  new commit; the build is unaffected.
- Public dependency routing is supplied by the managed guest before any command
  or toolchain provisioning, independently of the compiler-cache token. No
  registry environment is required from the caller. npm/Bun, pip/uv and Go
  managed hints become native config defaults rather than registry
  environment overrides, so project config and explicit environment/CLI
  choices keep normal precedence. Existing user registry/index choices are
  preserved. Cargo `CARGO_HOME` and `CARGO_TARGET_DIR` overrides are retained.
  `--no-cache` skips compiler/result caches, not the guest dependency mirrors.
- GitHub source reads are derived from the synced Git origin and locked Cargo
  GitHub sources. The CLI mints repository-scoped sessions through the public
  gateway using your Access/Broker grants; the accepted lease selects the
  reachable public or cluster route. Indexed Git config composes with dependency
  mirrors, and GitHub remains the push and review origin. `--no-cache` and
  explicit `--env GIT_CONFIG_…` source overrides skip automatic source routing.
  Without a source grant, private Cargo dependencies retain the existing shallow
  mirrors fetched with your local Git credentials and synced to the lease. No
  other identity is tried. Ungranted dependencies that cannot be fetched fail
  before the command starts; disabled source routing prepares them before leasing.
- The run also gets the project's shared build cache (sccache, Turbo): the
  CLI mints a cache token with your key concurrently with admission. Its
  lifetime includes the queue budget, setup and command; if the gateway's
  lifetime cap removes admission headroom, it refreshes once at READY. It merges
  the cache's environment into the command's (your `--env` wins). If the
  cache cannot be reached the run builds without it and prints one
  `sylphx: warning: build cache unavailable (…)` line; `--no-cache` skips it.
  Cache-server startup is lazy: `echo OK` does not start it. Cargo receives
  native source replacement without a network preflight, so aliases and global
  options cannot bypass routing and version/help queries never probe a mirror.
  The first compile initializes sccache once across concurrent
  compiler processes; on outage the wrapper executes the compiler directly
  and prints one warning on an actual compile, not a metadata probe Cargo
  suppresses. An explicit `RUSTC_WRAPPER`, including an empty value, wins.
  Provisioning, offline git inputs, source history and credential permissions
  are one guest start action after source sync, through public Sandboxes APIs.
  The token is never printed, logged or put in an event.
- `sylphx build cache env [--project ID] [-o json]` mints a read-only token
  (12 h) and prints `export NAME='value'` lines for a POSIX shell (plus
  `export RUSTC_WRAPPER=sccache` when sccache is installed and the variable
  is unset); `-o json` prints the environment as an object. The output holds
  a credential. Exit 0; 2 usage; 125 platform failure. The gateway is
  `https://build-cache.sylphx.net`, or `SYLPHX_BUILD_CACHE_URL`.
- `sylphx build source env OWNER/REPO [-o json]` works before cloning: evaluate
  its shell exports, then clone the
  ordinary GitHub URL. It derives the default public Broker policy and gateway
  from the existing cache locator, with no infrastructure setting prerequisite.
  `--project` selects a project and `--trust-policy` is an optional advanced
  override; the linked/key project is the default. JSON output is an environment object. The short-lived source token
  travels in a path-scoped HTTP header, never a URL. Keep output out of logs and
  the synced tree. Exit 125 on missing sign-in or denied source authority.
  Build runs keep source bearer headers in a private lease-owned Git include file
  and renew through the existing client keepalive before provider expiry. Each
  Git invocation rereads the file; no caller Access key goes to the lease. Source
  credentials are removed before releasing the lease, and output redacts tokens
  even when a process splits them across output events.
  In CI, `scripts/ci/setup-source.py` configures the same Git environment before
  checkout using the job’s own `GITHUB_TOKEN` and `id-token: write` OIDC identity
  (audience `sylphx-build-source`), with tokens masked and runner env files private.
  The default CI/Verify entrypoints bootstrap it at the workflow’s signed revision
  before checkout, rather than installing source settings after the source fetch.
  Source sessions are bounded by the signed OIDC expiry. A private Git wrapper
  renews on network Git commands only when the session nears expiry, using fresh
  job OIDC and the same workflow credential. A renewal denial fails the fetch;
  it never switches identities or bypasses source routing. The helper/state live
  in private `RUNNER_TEMP` files, cleared by the Actions runner at job end.
  The private Cargo action uses its own existing job-scoped App token to mint
  additional repository sessions through that same public endpoint, bound to
  locked revisions when present. Those scoped sessions join the workflow
  repository’s Git environment and renew on their own repository’s fetch demand.
  A pinned secondary checkout uses its existing workflow credential only after
  provider read access and the same Access organization are verified.
  If initial source setup finds an absent endpoint (404), transport failure or
  server outage (5xx), it warns once and keeps the normal checkout/private-helper
  path using the same job’s credentials. Authentication denials (401/403) fail;
  an unavailable cache is never reported as a cache hit.
  `BUILD_CACHE=false` or an existing indexed Git config preserves advanced CI
  source overrides.

## `sylphx work runner`

A self-hosted runner for Sylphx Work: it takes ready steps from your
workspace and runs your command for each one on this machine, as GitHub's
self-hosted runners take jobs.

```sh
sylphx work runner --label kind:build,member:group:builder -- ./run-step.sh   # one step, then exit (default)
sylphx work runner --loop --label kind:build,member:group:builder,cap:repo-write -- ./run-step.sh
```

- It long-polls Work (`POST /v2/claim` with `n: 1`, its labels, its runner
  name and `wait`) for a ready step whose required labels `--label` covers.
  The runner (`--name`, default the host name) is catalogued on that claim, so
  the workspace's coverage counts it as a live runner for each
  `member:<group>` label it advertises.
- The command gets the claim (item, checkpoint, context, `profile`,
  `workspaceProfile`, token) as JSON on standard input and in
  `$SYLPHX_WORK_CLAIM_FILE`, and the claim token, scoped to that one item, in
  `$SYLPHX_WORK_CLAIM_TOKEN`; `$SYLPHX_WORK_URL`, `$SYLPHX_WORK_ITEM`,
  `$SYLPHX_WORK_GENERATION` and `$SYLPHX_WORK_RUNNER` name the rest.
  `SYLPHX_API_KEY` is removed from its environment: the step acts with the
  claim token, never the runner's key. `SYLPHX_CONFIG_DIR` points to an empty
  per-step directory rather than the runner's saved login; it is removed after
  the step. Each beat writes the renewed token to
  `$SYLPHX_WORK_CLAIM_TOKEN_FILE`. Work names no harness or model: the command
  is yours.
- While the command runs the runner beats the claim at half its
  time-to-live (`--ttl`, default the step kind's lease). A beat that answers
  `cancel` (dropped, reassigned or revoked) stops the command's process group
  (SIGTERM, SIGKILL after 10 s).
- When the command ends the runner releases the step with a checkpoint that
  names its exit status, unless the command already handed it over with the
  claim token. With `--ephemeral` (the default) it then exits 0, whatever the
  command's status; `--loop` takes the next step. Ctrl-C stops the command,
  hands the step over and exits 130. `-o json` prints one JSON line per step
  (`item`, `generation`, `exitCode`, `cancelled`, `handedOverBy`).
- Work's address is `--work-url`, else `SYLPHX_WORK_URL`, else
  `https://work.sylphx.com`; the key is your login, `SYLPHX_API_KEY` or
  `--api-key`.

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
