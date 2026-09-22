//! `sylphx-mcp`: the Sylphx MCP server over stdio. Credentials come from
//! `SYLPHX_API_KEY` (and optional `SYLPHX_BASE_URL`); without them, search
//! and describe still work. `sylphx mcp` runs the same server with the CLI's
//! stored login.

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let client = sylphx::Client::from_env().ok();
    sylphx_mcp::serve_stdio(sylphx_mcp::Server::new(client)).await
}
