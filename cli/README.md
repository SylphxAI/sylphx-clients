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
sylphx mcp                            # the MCP server over stdio
```

- `sylphx <service> <collection> <verb> [NAME|PARENT|ID] [--flags]`: flags
  are the spec fields in dotted kebab-case (`--spec.compute-units`);
  `--from-file` takes a whole request; `--output table|json|yaml|name`.
- A bare id (`main`) expands below the linked env; parents default to the
  linked project, else the key's scope.
- Mutations wait for their Operation (`--no-wait` to skip); `--dry-run` sets
  `validate_only`; updates send the etag they read and a mask of the flags
  given; destructive calls ask first (`--yes` to skip).
- `sylphx api GET /v1/whoami` is the raw escape hatch.
- `sylphx devices run` installs an APK on a fresh Android device lease,
  launches it (or a `--game-loop`), watches it with screenshots, and writes
  `result.json`, `junit.xml`, `logcat.txt`, and `crash.txt` into `--out`
  (`device-results/`). Exit 0 passed, 1 failed, 2 infrastructure; the lease is
  released on the way out unless `--keep`.

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
