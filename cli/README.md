# sylphx CLI

Every Sylphx API method as a command, generated from the one schema, plus a
small hand-written porcelain on the generated Rust SDK.

```sh
npm install -g @sylphx/cli@next      # or: cargo install sylphx-cli --version 0.24.0-rc.1
sylphx login                          # stores an Access key (verified with whoami)
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
