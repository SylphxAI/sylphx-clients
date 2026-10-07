//! `sylphx-mcp`: the Sylphx MCP server over stdio. Credentials come from
//! `SYLPHX_API_KEY` (and optional `SYLPHX_BASE_URL`); without them, search
//! and describe still work. `sylphx mcp` runs the same server with the CLI's
//! stored login. `SYLPHX_DOCS_INDEX` (and `SYLPHX_DOCS_SOURCE`) add the
//! `docs_search` and `docs_read` tools over that search index.

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let client = sylphx::Client::from_env().ok();
    sylphx_mcp::serve_stdio(
        sylphx_mcp::Server::new(client).with_docs(sylphx_mcp::docs::DocsIndex::from_env()),
    )
    .await
}
