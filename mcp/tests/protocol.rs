//! The MCP server over a recording transport: the handshake, the tool list,
//! core and meta tools, confirmation of destructive calls, and errors.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{json, Value};
use sylphx::{Client, Error, HttpRequest, HttpResponse, Transport};
use sylphx_mcp::Server;

#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl Transport for Recorder {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, Error> {
        self.sent.lock().unwrap().push(request);
        let (status, body) = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::Transport("no reply queued".into()))?;
        Ok(HttpResponse {
            status,
            request_id: Some("req_t".into()),
            body: serde_json::to_vec(&body).unwrap(),
        })
    }
}

fn server(replies: Vec<(u16, Value)>) -> Server<Recorder> {
    let rec = Recorder::default();
    rec.replies.lock().unwrap().extend(replies);
    Server::new(Some(Client::new(rec)))
}

async fn call(s: &Server<Recorder>, method: &str, params: Value) -> Value {
    s.handle(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .await
        .expect("a request gets a response")
}

#[tokio::test]
async fn initialize_negotiates_the_protocol_version() {
    let s = server(vec![]);
    let r = call(&s, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})).await;
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(r["result"]["serverInfo"]["name"], "sylphx");
    assert!(r["result"]["capabilities"]["tools"].is_object());
    let r = call(&s, "initialize", json!({"protocolVersion": "1999-01-01"})).await;
    assert_eq!(
        r["result"]["protocolVersion"],
        sylphx_mcp::PROTOCOL_VERSIONS[0]
    );
    assert!(s
        .handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await
        .is_none());
}

#[tokio::test]
async fn tools_list_has_core_and_meta_tools() {
    let s = server(vec![]);
    let r = call(&s, "tools/list", json!({})).await;
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "access_whoami",
        "data_databases_create",
        "sylphx_search_methods",
        "sylphx_describe_method",
        "sylphx_call",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    for t in r["result"]["tools"].as_array().unwrap() {
        assert!(t.get("x-sylphx-method").is_none());
        assert_eq!(t["inputSchema"]["type"], "object");
        assert!(t["name"].as_str().unwrap().len() <= 60);
    }
}

#[tokio::test]
async fn a_core_tool_calls_its_method() {
    let s = server(vec![(
        200,
        json!({"principal": "principal_a", "org": "orgs/org_a"}),
    )]);
    let r = call(
        &s,
        "tools/call",
        json!({"name": "access_whoami", "arguments": {}}),
    )
    .await;
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(r["result"]["structuredContent"]["org"], "orgs/org_a");
}

#[tokio::test]
async fn search_describe_and_call_reach_every_method() {
    let s = server(vec![(
        200,
        json!({"name": "orgs/org_a/projects/p/envs/e/api_keys/k"}),
    )]);
    let r = call(
        &s,
        "tools/call",
        json!({"name": "sylphx_search_methods", "arguments": {"query": "revoke api key"}}),
    )
    .await;
    let hits = r["result"]["structuredContent"]["methods"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(hits[0]["method_id"], "access.api_keys.revoke");

    let r = call(&s, "tools/call", json!({"name": "sylphx_describe_method", "arguments": {"method_id": "access.api_keys.revoke"}})).await;
    assert_eq!(r["result"]["structuredContent"]["effect"], "destructive");
    assert_eq!(
        r["result"]["structuredContent"]["input_schema"]["required"],
        json!(["name", "confirm"])
    );

    let args = json!({"method_id": "access.api_keys.revoke", "args": {"name": "orgs/org_a/projects/p/envs/e/api_keys/k"}});
    let r = call(
        &s,
        "tools/call",
        json!({"name": "sylphx_call", "arguments": args}),
    )
    .await;
    assert_eq!(
        r["result"]["isError"], true,
        "destructive without confirm is refused"
    );

    let mut args = args;
    args["confirm"] = json!(true);
    let r = call(
        &s,
        "tools/call",
        json!({"name": "sylphx_call", "arguments": args}),
    )
    .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
}

#[tokio::test]
async fn a_problem_body_is_a_tool_error() {
    let s = server(vec![(
        403,
        json!({"code": "PERMISSION_DENIED", "status": 403, "detail": "missing scope data:write", "retryable": false}),
    )]);
    let r = call(&s, "tools/call", json!({"name": "sylphx_call", "arguments": {"method_id": "data.databases.get", "args": {"name": "orgs/o/projects/p/envs/e/databases/main"}}})).await;
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("PERMISSION_DENIED"));
}

#[tokio::test]
async fn without_credentials_calls_explain_how_to_sign_in() {
    let s: Server<Recorder> = Server::new(None);
    let r = call(
        &s,
        "tools/call",
        json!({"name": "access_whoami", "arguments": {}}),
    )
    .await;
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("SYLPHX_API_KEY"));
    let r = call(&s, "nope/nope", json!({})).await;
    assert_eq!(r["error"]["code"], -32601);
}
