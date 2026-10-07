//! The stdio server and client setup, on `sylphx-mcp-kit`: rmcp (the
//! official Rust MCP SDK) speaks the protocol, so version negotiation,
//! cancellation, pagination and per-version result shapes track the MCP spec
//! with the SDK instead of a hand-written JSON-RPC loop.

use mcp_kit::rmcp::model::CallToolResult;
use mcp_kit::server::{App, Call, Info};
use serde_json::Value;
use sylphx::Transport;

use crate::{tool_error, Server, INSTRUCTIONS};

/// The name clients register the server under, and its npm package.
const NAME: &str = "sylphx";
const PACKAGE: &str = "@sylphx/mcp";

/// A [`Server`] as an mcp-kit app.
struct Stdio<T>(Server<T>);

impl<T: Transport + Send + Sync + 'static> App for Stdio<T> {
    fn info(&self) -> Info {
        Info {
            name: NAME.into(),
            title: "Sylphx".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            website: "https://sylphx.com/docs".into(),
            instructions: INSTRUCTIONS.into(),
        }
    }

    fn tools(&self) -> Vec<Value> {
        self.0.tools()
    }

    fn call(&self, name: &str, args: &Value, _call: &Call) -> Result<String, String> {
        Err(format!("unknown tool: {name} ({args})"))
    }

    /// The kit runs this on a blocking thread of the server's runtime, so the
    /// async SDK call is driven on that runtime's handle.
    fn call_result(&self, name: &str, args: &Value, _call: &Call) -> CallToolResult {
        let handle = tokio::runtime::Handle::current();
        let result = handle
            .block_on(self.0.call_tool(name, args.clone()))
            .unwrap_or_else(|(_, message)| tool_error(&message));
        serde_json::from_value(result).unwrap_or_else(|e| {
            serde_json::from_value(tool_error(&format!("bad tool result: {e}")))
                .expect("a text tool error is a CallToolResult")
        })
    }
}

/// Serves `server` over stdin/stdout until the client disconnects.
pub async fn serve_stdio<T: Transport + Send + Sync + 'static>(
    server: Server<T>,
) -> std::io::Result<()> {
    serve_transport(server, mcp_kit::rmcp::transport::stdio()).await
}

/// Serves `server` over any rmcp transport (a pipe in tests).
pub async fn serve_transport<T, IO, E, M>(server: Server<T>, transport: IO) -> std::io::Result<()>
where
    T: Transport + Send + Sync + 'static,
    IO: mcp_kit::rmcp::transport::IntoTransport<mcp_kit::rmcp::RoleServer, E, M>,
    E: std::error::Error + Send + Sync + 'static,
{
    mcp_kit::server::serve(Stdio(server), transport)
        .await
        .map_err(std::io::Error::other)
}

/// What `sylphx mcp setup` changes.
#[derive(Clone, Debug, Default)]
pub struct SetupOptions {
    /// Print the changes without writing them.
    pub dry_run: bool,
    /// Remove the entry instead of adding it.
    pub remove: bool,
    /// Only these clients (`claude-code`, `codex`, `cursor`, `vscode`,
    /// `vscode-insiders`, `claude-desktop`, `windsurf`, `gemini`); all
    /// installed ones when `None`.
    pub clients: Option<Vec<String>>,
}

/// Registers the Sylphx MCP server (`npx -y @sylphx/mcp`) with the MCP
/// clients on this machine (Claude Code, Codex, Cursor, VS Code, Claude
/// Desktop, Windsurf, Gemini CLI). Safe to repeat. Returns how many client
/// configurations changed.
pub fn setup(options: &SetupOptions) -> std::io::Result<usize> {
    let server = mcp_kit::setup::Server {
        name: NAME.into(),
        package: PACKAGE.into(),
        args: Vec::new(),
    };
    mcp_kit::setup::run(
        &server,
        &mcp_kit::setup::Options {
            dry_run: options.dry_run,
            remove: options.remove,
            clients: options.clients.clone(),
        },
    )
    .map_err(std::io::Error::other)
}
