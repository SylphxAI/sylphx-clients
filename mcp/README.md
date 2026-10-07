# sylphx-mcp

The Sylphx MCP server: every Sylphx API method available to an agent, from
the same schema as the SDKs and the CLI.

```sh
npx @sylphx/mcp          # or: sylphx mcp, or: cargo install sylphx-mcp && sylphx-mcp
```

Register it with every MCP client on this machine (Claude Code, Codex,
Cursor, VS Code, Claude Desktop, Windsurf, Gemini CLI); safe to repeat,
`--remove` undoes it:

```sh
sylphx mcp setup         # or one client: sylphx mcp setup --client cursor
```

Or by hand (stdio):

```json
{ "mcpServers": { "sylphx": { "command": "npx", "args": ["-y", "@sylphx/mcp"], "env": { "SYLPHX_API_KEY": "sylphx_sk_…" } } } }
```

- Core tools for the lead offer (`access_whoami`, `access_projects_create`,
  `data_databases_create`, `hosting_services_create`, …), each with its input
  schema.
- `sylphx_search_methods`, `sylphx_describe_method`, and `sylphx_call` reach
  every other method without flooding the context.
- Tool annotations come from each method's effect; destructive calls need
  `confirm: true`.
- `docs_search` and `docs_read`, read-only, when `SYLPHX_DOCS_INDEX` names a
  search index of the key's environment (and `SYLPHX_DOCS_SOURCE` a source,
  when the index holds several): Markdown docs chunked by heading, answered
  as `path:line` hits and read back by path and line range. The index is
  filled by `sylphx data docs sync` on each merge of the docs' repository,
  so a reader needs `data:read` on that environment, not the repository.

```sh
sylphx data docs sync --index docs --revision "$GITHUB_SHA" README.md 'docs/**/*.md'
sylphx data docs search --index docs rate limits    # docs/api/limits.md:42  Limits > Rate limits
sylphx data docs read --index docs docs/api/limits.md --lines 40-60
```

The stdio protocol is [rmcp](https://github.com/modelcontextprotocol/rust-sdk),
the official Rust MCP SDK, through
[`sylphx-mcp-kit`](https://github.com/SylphxAI/mcp-kit), the stack every
Sylphx MCP server shares.

Generated from the Sylphx schema registry; `generated/tools.json` is
`sylphx-gen` output.
