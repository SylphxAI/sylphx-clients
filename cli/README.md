# sylphx CLI

Every Sylphx API method as a command, generated from the one schema, plus a
small hand-written porcelain on the generated Rust SDK.

```sh
curl -fsSL https://github.com/SylphxAI/sylphx-clients/releases/latest/download/install.sh | sh   # or: npm i -g @sylphx/cli · cargo install sylphx-cli
sylphx login                          # approve at sylphx.com/device; the org key goes to the OS keychain
                                      #   (your role's scopes, follows role changes, expires in 30 days; `sylphx logout` revokes it)
sylphx login --api-key -              # agents and CI: an Access key on stdin (or SYLPHX_API_KEY)
sylphx logout                         # revokes the key and forgets it
sylphx link --env orgs/…/envs/…       # defaults for this directory (.sylphx/project.json)
sylphx data databases create main --spec.compute-units 2
sylphx data databases list -o json
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
