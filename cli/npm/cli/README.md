# sylphx CLI

Every Sylphx API method as a command, generated from the one schema, plus a
small hand-written porcelain on the generated Rust SDK.

```sh
curl -fsSL https://github.com/SylphxAI/sylphx-clients/releases/latest/download/install.sh | sh   # or: npm i -g @sylphx/cli · cargo install sylphx-cli
sylphx login                          # approve at sylphx.com/device; the org key goes to the OS keychain
sylphx login --api-key -              # agents and CI: an Access key on stdin (or SYLPHX_API_KEY)
sylphx token --scope hosting:deploy   # one short-lived token (15 min) for one scope, on stdout only
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

## Lease request files

`--from-file` contains the **whole method request**, not just the HTTP body.
Lease request field names are checked against the schema before defaults or API
calls. Unknown or misplaced fields fail with `UNKNOWN_FIELD` and the dotted
field path; caller-owned map keys remain open.

For example, create an Android lease lasting 1800 seconds:

```json
{
  "parent": "orgs/ORG/projects/PROJECT/envs/ENV",
  "lease": {
    "spec": {
      "kind": "android",
      "image": "template:android",
      "shape": "linux-l",
      "ttl": "1800s"
    }
  }
}
```

Run `sylphx sandboxes leases create --from-file android.json --output json`.
A top-level `spec` in this file is invalid: the path is `lease.spec`. The
server's REST body for `POST /v1/{parent}/leases` is the Lease itself, so its
body uses a top-level `spec` instead. Do not interchange the two documents.
After creation the CLI prints the server-resolved kind, image, size and TTL
on stderr; stdout retains the selected output format.
