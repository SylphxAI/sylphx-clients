//! The Sylphx MCP server (docs/specs/one-platform/resource-api-and-clients.md §8.5).
//!
//! Tools come from the generated manifest (`generated/tools.json`, emitted by
//! `sylphx-gen` from the one schema):
//!
//! - **core tools**, one per method marked `mcp = CORE` (`access_whoami`,
//!   `data_databases_create`, …), each with its request's input schema;
//! - **three meta tools** that reach every other method without flooding the
//!   context: `sylphx_search_methods`, `sylphx_describe_method`, and
//!   `sylphx_call`.
//!
//! Every call goes through the generated Rust SDK's one dynamic entry point
//! ([`sylphx::Client::invoke`]); destructive methods require `confirm: true`.
//! [`Server::handle`] is transport-agnostic (one JSON-RPC message in, at most
//! one out): the Resource API front mounts it for the remote server at
//! `https://api.sylphx.com/mcp`. Over stdio (`sylphx mcp`, `npx @sylphx/mcp`)
//! the protocol is rmcp's, the official Rust MCP SDK, through
//! `sylphx-mcp-kit` ([`serve_stdio`], feature `stdio`), the same stack as
//! our other MCP servers.

use std::sync::{Arc, OnceLock};

use serde_json::{json, Map, Value};
use sylphx::{Client, Error, HttpRequest, Transport};

#[cfg(feature = "stdio")]
mod stdio;
#[cfg(feature = "stdio")]
pub use stdio::{serve_stdio, serve_transport, setup, SetupOptions};

/// The generated tool manifest.
pub const MANIFEST: &str = include_str!("../generated/tools.json");

/// MCP protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

const INSTRUCTIONS: &str = "Sylphx is one API for every Sylphx service. Call access_whoami \
first to learn your org, project, and env. Resource names are full paths such as \
orgs/{org}/projects/{project}/envs/{env}/databases/{id}. Use sylphx_search_methods to find a \
method, sylphx_describe_method for its input schema, and sylphx_call to call it; destructive \
methods need confirm: true.";

/// How many search hits `sylphx_search_methods` returns.
const SEARCH_LIMIT: usize = 20;

/// Who the server answers: a local stdio session (the user's own key, every
/// served method) or the remote door (a connected app: read-only methods
/// only until write step-up ships).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Local,
    RemoteReadOnly,
}

/// The parsed manifest, shared by every server of the process. The remote
/// door builds a [`Server`] per request, so the manifest is parsed once.
#[derive(Debug)]
pub struct Catalog {
    /// Core and meta tools of served methods, as `tools/list` returns them
    /// locally.
    local_tools: Vec<Value>,
    /// The same, minus every method that is not read-only, with the meta
    /// tools described as read-only.
    remote_tools: Vec<Value>,
    catalog: Vec<Value>,
}

/// An entry with no `served` field is served; only `served: false` is not.
fn is_served(entry: &Value) -> bool {
    entry["served"] != false
}

/// Read permissions the remote door never serves, whatever the bearer holds:
/// a prompt-injected connector must not read a secret (remote-mcp design D8).
const REMOTE_EXCLUDED_PERMISSIONS: &[&str] = &["secrets:read"];

/// Whether the remote door may show and call `entry`: a read-only method
/// whose permission is a read scope (or whoami), less the D8 exclusions. An
/// `effect: read` method guarded by a write or exec permission (a database's
/// `:connect`, which returns its credentials; a sandbox file read) is not, so
/// a plain Access key on the door reaches no more than an OAuth grant would.
fn remote_callable(entry: &Value) -> bool {
    let permission = entry["permission"].as_str().unwrap_or_default();
    entry["effect"] == "read"
        && (permission == "access:whoami" || permission.ends_with(":read"))
        && !REMOTE_EXCLUDED_PERMISSIONS.contains(&permission)
}

impl Catalog {
    fn parse() -> Self {
        let manifest: Value = serde_json::from_str(MANIFEST).expect("generated/tools.json is JSON");
        let catalog: Vec<Value> = manifest["catalog"].as_array().cloned().unwrap_or_default();
        let entry_of = |tool: &str| catalog.iter().find(|e| e["tool"] == tool);
        let local_tools: Vec<Value> = manifest["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|mut t| {
                // The method id is ours; MCP clients see the standard fields only.
                if let Some(o) = t.as_object_mut() {
                    o.remove("x-sylphx-method");
                    if let Some(title) = o.get("annotations").and_then(|a| a.get("title")).cloned()
                    {
                        o.insert("title".into(), title);
                    }
                }
                t
            })
            .filter(|t| entry_of(t["name"].as_str().unwrap_or_default()).is_none_or(is_served))
            .collect();
        let remote_tools = local_tools
            .iter()
            .filter_map(|t| {
                let name = t["name"].as_str().unwrap_or_default();
                if name == "sylphx_call" {
                    let mut t = t.clone();
                    t["description"] = json!(
                        "Call any served, read-only Sylphx method by id with arguments matching its input schema. Methods that change anything are not available to connected apps yet; use an API key."
                    );
                    t["annotations"] = json!({
                        "title": "Call a read-only Sylphx method",
                        "readOnlyHint": true, "destructiveHint": false,
                        "idempotentHint": true, "openWorldHint": false
                    });
                    t["title"] = t["annotations"]["title"].clone();
                    return Some(t);
                }
                match entry_of(name) {
                    Some(e) if !remote_callable(e) => None,
                    _ => Some(t.clone()),
                }
            })
            .collect();
        Self {
            local_tools,
            remote_tools,
            catalog,
        }
    }

    /// The process-wide catalog, parsed on first use.
    pub fn shared() -> Arc<Catalog> {
        static SHARED: OnceLock<Arc<Catalog>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(Catalog::parse())))
    }
}

/// One MCP server over a Sylphx client. Without a client (no credentials),
/// search and describe still work and every call explains how to sign in.
pub struct Server<T> {
    client: Option<Client<T>>,
    shared: Arc<Catalog>,
    mode: Mode,
}

/// Checks the headers Streamable HTTP mirrors from the message (revision
/// 2026-07-28): `MCP-Protocol-Version` must be a revision this server speaks
/// and `Mcp-Method` must name the body's method. The error is a JSON-RPC
/// error body for an HTTP 400 (`-32020 HeaderMismatch`).
pub fn validate_headers(
    protocol_version: Option<&str>,
    mcp_method: Option<&str>,
    body: &Value,
) -> Result<(), Value> {
    let mismatch = |message: String| {
        Err(json!({
            "jsonrpc": "2.0", "id": body.get("id").cloned().unwrap_or(Value::Null),
            "error": { "code": -32020, "message": message }
        }))
    };
    if let Some(v) = protocol_version {
        if !PROTOCOL_VERSIONS.contains(&v) {
            return mismatch(format!("unsupported MCP-Protocol-Version {v}"));
        }
    }
    if let (Some(h), Some(m)) = (mcp_method, body.get("method").and_then(Value::as_str)) {
        if h != m {
            return mismatch(format!("Mcp-Method {h} does not match the body method {m}"));
        }
    }
    Ok(())
}

impl<T: Transport> Server<T> {
    /// A local server: every served method, for the user's own key.
    pub fn new(client: Option<Client<T>>) -> Self {
        Self::with_catalog(client, Catalog::shared(), Mode::Local)
    }

    /// The remote door's server: read-only methods only.
    pub fn remote(client: Option<Client<T>>) -> Self {
        Self::with_catalog(client, Catalog::shared(), Mode::RemoteReadOnly)
    }

    pub fn with_catalog(client: Option<Client<T>>, shared: Arc<Catalog>, mode: Mode) -> Self {
        Self {
            client,
            shared,
            mode,
        }
    }

    /// The tools `tools/list` returns.
    pub fn tools(&self) -> &[Value] {
        match self.mode {
            Mode::Local => &self.shared.local_tools,
            Mode::RemoteReadOnly => &self.shared.remote_tools,
        }
    }

    /// Catalog entries this server may show and call.
    fn visible(&self, e: &Value) -> bool {
        is_served(e) && (self.mode == Mode::Local || remote_callable(e))
    }

    /// Handles one JSON-RPC message (or a batch). Returns the response, or
    /// `None` for a notification.
    pub async fn handle(&self, message: Value) -> Option<Value> {
        if let Value::Array(batch) = message {
            let mut out = Vec::new();
            for m in batch {
                if let Some(r) = Box::pin(self.handle(m)).await {
                    out.push(r);
                }
            }
            return (!out.is_empty()).then_some(Value::Array(out));
        }
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notifications (`notifications/initialized`, `notifications/cancelled`) need no answer.
            return None;
        };
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools() })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                self.call_tool(name, args).await
            }
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let version = PROTOCOL_VERSIONS
            .iter()
            .find(|v| **v == asked)
            .unwrap_or(&PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "sylphx", "title": "Sylphx", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        })
    }

    /// `tools/call`: a tool-level failure is a result with `isError`, never a
    /// JSON-RPC error, so the model can read and correct it.
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, (i64, String)> {
        let Value::Object(mut args) = args else {
            return Ok(tool_error("arguments must be a JSON object"));
        };
        match name {
            "sylphx_search_methods" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(tool_ok(json!({ "methods": self.search(query) })))
            }
            "sylphx_describe_method" => {
                let id = args
                    .get("method_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                Ok(match self.entry(id) {
                    Some(e) => tool_ok(e.clone()),
                    None => tool_error(&format!(
                        "unknown method_id `{id}`; find one with sylphx_search_methods"
                    )),
                })
            }
            "sylphx_call" => {
                let id = args
                    .get("method_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let confirm = args
                    .get("confirm")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let call_args = args.remove("args").unwrap_or_else(|| json!({}));
                Ok(self.call_method(&id, call_args, confirm).await)
            }
            tool => {
                let Some(entry) = self.shared.catalog.iter().find(|e| e["tool"] == tool) else {
                    return Err((-32602, format!("unknown tool: {tool}")));
                };
                if !self.tools().iter().any(|t| t["name"] == tool) {
                    return Err((-32602, format!("unknown tool: {tool}; use sylphx_call")));
                }
                let id = entry["method_id"].as_str().unwrap_or_default().to_string();
                let confirm = args
                    .remove("confirm")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(self.call_method(&id, Value::Object(args), confirm).await)
            }
        }
    }

    fn entry(&self, method_id: &str) -> Option<&Value> {
        self.shared
            .catalog
            .iter()
            .find(|e| e["method_id"] == method_id && self.visible(e))
    }

    /// Every catalog entry scored by how many query words its id, tool name,
    /// or description contains; best first.
    pub fn search(&self, query: &str) -> Vec<Value> {
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        let mut hits: Vec<(usize, &Value)> = self
            .shared
            .catalog
            .iter()
            .filter(|e| self.visible(e))
            .filter_map(|e| {
                let hay = format!(
                    "{} {} {}",
                    e["method_id"].as_str().unwrap_or_default(),
                    e["tool"].as_str().unwrap_or_default(),
                    e["description"].as_str().unwrap_or_default()
                )
                .to_lowercase();
                let score = words.iter().filter(|w| hay.contains(w.as_str())).count();
                (words.is_empty() || score > 0).then_some((score, e))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1["method_id"].as_str().cmp(&b.1["method_id"].as_str()))
        });
        hits.into_iter()
            .take(SEARCH_LIMIT)
            .map(|(_, e)| {
                json!({
                    "method_id": e["method_id"],
                    "tool": e["tool"],
                    "effect": e["effect"],
                    "long_running": e["long_running"],
                    "summary": e["description"].as_str().unwrap_or_default().split("\n\n").next().unwrap_or_default().replace('\n', " "),
                })
            })
            .collect()
    }

    async fn call_method(&self, method_id: &str, args: Value, confirm: bool) -> Value {
        let Some(entry) = self.entry(method_id) else {
            // Say why when the method exists but this server may not call it.
            if let Some(e) = self
                .shared
                .catalog
                .iter()
                .find(|e| e["method_id"] == method_id)
            {
                return tool_error(&if !is_served(e) {
                    format!("{method_id} is not available yet (its service is not served)")
                } else if e["effect"] != "read" {
                    format!(
                        "{method_id} changes data: not available to connected apps yet; use an API key"
                    )
                } else {
                    format!(
                        "{method_id} returns credentials or secrets: not available to connected apps; use an API key"
                    )
                });
            }
            return tool_error(&format!(
                "unknown method_id `{method_id}`; find one with sylphx_search_methods"
            ));
        };
        if entry["effect"] == "destructive" && !confirm {
            return tool_error(&format!(
                "{method_id} is destructive: repeat the call with confirm: true once the user has agreed"
            ));
        }
        let Some(client) = &self.client else {
            return tool_error(
                "no Sylphx credentials: set SYLPHX_API_KEY (an Access key) or run `sylphx login`, then restart the server",
            );
        };
        match client.invoke(method_id, args).await {
            Ok(value) if entry["long_running"] == true => match wait_once(client, value).await {
                Ok(v) => tool_ok(v),
                Err(e) => error_result(e),
            },
            Ok(value) => tool_ok(value),
            Err(e) => error_result(e),
        }
    }
}

/// Waits up to 30 s for an Operation; returns it as it stands then (the model
/// can call again), or its settled response.
async fn wait_once<T: Transport>(client: &Client<T>, operation: Value) -> Result<Value, Error> {
    if operation["done"] == true {
        return Ok(settled(operation));
    }
    let Some(name) = operation["name"].as_str().filter(|n| !n.is_empty()) else {
        return Ok(operation);
    };
    let op: Value = client
        .call(HttpRequest {
            method: "POST",
            path: format!("/v1/{name}:wait"),
            query: vec![("timeout".into(), "30s".into())],
            body: Some(json!({})),
            mutation: false,
            origin: None,
            effect_ids: false,
        })
        .await?;
    Ok(if op["done"] == true { settled(op) } else { op })
}

/// A done Operation's response (without `@type`), or the Operation itself
/// when it carries an error.
fn settled(op: Value) -> Value {
    match op.get("response") {
        Some(Value::Object(r)) => {
            let mut r = r.clone();
            r.remove("@type");
            Value::Object(r)
        }
        _ => op,
    }
}

fn tool_ok(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_default();
    let mut out = Map::new();
    out.insert("content".into(), json!([{ "type": "text", "text": text }]));
    if value.is_object() {
        out.insert("structuredContent".into(), value);
    }
    out.insert("isError".into(), json!(false));
    Value::Object(out)
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn error_result(e: Error) -> Value {
    match e {
        Error::Api { problem, .. } => {
            let p = serde_json::to_value(&*problem).unwrap_or_default();
            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&p).unwrap_or_default() }], "isError": true })
        }
        other => tool_error(&other.to_string()),
    }
}
