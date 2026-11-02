//! The MCP server over a recording transport: the handshake, the tool list,
//! core and meta tools, confirmation of destructive calls, and errors.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sylphx::{Client, Error, HttpRequest, HttpResponse, Transport};
use sylphx_mcp::Server;

#[derive(Clone, Default)]
struct Recorder {
    sent: Arc<Mutex<Vec<HttpRequest>>>,
    replies: Arc<Mutex<VecDeque<(u16, Value)>>>,
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

#[tokio::test]
async fn exec_requires_a_saved_key_and_returns_recoverable_pending_or_terminal_unknown() {
    let rec = Recorder::default();
    rec.replies.lock().unwrap().extend([
        (
            409,
            json!({"code":"IDEMPOTENCY_IN_PROGRESS","retryable":true,"effect":"applied"}),
        ),
        (
            200,
            json!({"exit_code":7,"execution_id":"one-dispatch","idempotency_key":"saved-key"}),
        ),
        (
            409,
            json!({"code":"EXEC_OUTCOME_UNKNOWN","retryable":false,"effect":"unknown"}),
        ),
    ]);
    let s = Server::new(Some(Client::new(rec.clone())));
    let mut args =
        json!({"name":"orgs/o/projects/p/envs/e/leases/l","command":["true"],"timeout":"180s"});
    let invoke = |args| json!({"name":"sylphx_call","arguments":{"method_id":"sandboxes.leases.exec","args":args}});
    let missing = call(&s, "tools/call", invoke(args.clone())).await;
    assert_eq!(missing["result"]["isError"], true);
    assert!(rec.sent.lock().unwrap().is_empty());
    args["idempotency_key"] = json!("saved-key");
    let pending = call(&s, "tools/call", invoke(args.clone())).await;
    assert_eq!(pending["result"]["isError"], false);
    assert_eq!(pending["result"]["structuredContent"]["status"], "pending");
    assert_eq!(pending["result"]["structuredContent"]["request"], args);
    let terminal = call(&s, "tools/call", invoke(args.clone())).await;
    assert_eq!(
        terminal["result"]["structuredContent"]["execution_id"],
        "one-dispatch"
    );
    let unknown = call(&s, "tools/call", invoke(args)).await;
    assert_eq!(unknown["result"]["isError"], true);
    let sent = rec.sent.lock().unwrap();
    assert_eq!(sent.len(), 3);
    assert!(sent.iter().all(|request| request == &sent[0]));
}

#[tokio::test(start_paused = true)]
async fn exec_observation_deadline_returns_the_original_request_without_a_second_dispatch() {
    struct Silent;
    impl Transport for Silent {
        async fn send(&self, _: HttpRequest) -> Result<HttpResponse, Error> {
            std::future::pending().await
        }
    }
    let s = Server::new(Some(Client::new(Silent)));
    let args = json!({"name":"orgs/o/projects/p/envs/e/leases/l","command":["sleep","130"],"timeout":"180s","idempotency_key":"saved-key"});
    let start = tokio::time::Instant::now();
    let result = call(
        &s,
        "tools/call",
        json!({"name":"sylphx_call","arguments":{"method_id":"sandboxes.leases.exec","args":args}}),
    )
    .await;
    assert_eq!(start.elapsed(), std::time::Duration::from_secs(25));
    assert_eq!(result["result"]["isError"], false);
    assert_eq!(result["result"]["structuredContent"]["status"], "pending");
    assert_eq!(result["result"]["structuredContent"]["request"], args);
}

async fn call<T: Transport>(s: &Server<T>, method: &str, params: Value) -> Value {
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

fn remote(replies: Vec<(u16, Value)>) -> Server<Recorder> {
    let rec = Recorder::default();
    rec.replies.lock().unwrap().extend(replies);
    Server::remote(Some(Client::new(rec)))
}

#[tokio::test]
async fn both_protocol_eras_negotiate_and_mint_no_session() {
    let s = remote(vec![]);
    for v in ["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"] {
        let r = call(&s, "initialize", json!({"protocolVersion": v})).await;
        assert_eq!(r["result"]["protocolVersion"], v);
        assert!(r["result"].get("sessionId").is_none());
    }
    assert_eq!(sylphx_mcp::PROTOCOL_VERSIONS[0], "2026-07-28");
    // `_meta` on a stateless request is accepted and ignored.
    let r = call(
        &s,
        "tools/list",
        json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}),
    )
    .await;
    assert!(r["result"]["tools"].is_array());
}

#[test]
fn header_validation_follows_the_body() {
    use sylphx_mcp::validate_headers;
    let body = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"});
    assert!(validate_headers(Some("2026-07-28"), Some("tools/list"), &body).is_ok());
    assert!(validate_headers(None, None, &body).is_ok());
    let e = validate_headers(None, Some("tools/call"), &body).unwrap_err();
    assert_eq!(e["error"]["code"], -32020);
    assert_eq!(e["id"], 7);
    let e = validate_headers(Some("1999-01-01"), None, &body).unwrap_err();
    assert_eq!(e["error"]["code"], -32020);
}

fn manifest() -> Value {
    serde_json::from_str(sylphx_mcp::MANIFEST).unwrap()
}

#[tokio::test]
async fn remote_lists_only_served_read_only_tools() {
    let m = manifest();
    let cat = m["catalog"].as_array().unwrap();
    let s = remote(vec![]);
    let r = call(&s, "tools/list", json!({})).await;
    let tools = r["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    for t in tools {
        let name = t["name"].as_str().unwrap();
        if let Some(e) = cat.iter().find(|e| e["tool"] == name) {
            assert_ne!(e["served"], false, "{name} is unserved");
            assert_eq!(e["effect"], "read", "{name} is not read-only");
            let p = e["permission"].as_str().unwrap();
            assert!(
                (p.ends_with(":read") || p == "access:whoami") && p != "secrets:read",
                "{name}: {p}"
            );
        }
    }
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"access_whoami"));
    assert!(names.contains(&"sylphx_call"));
    assert!(!names.contains(&"data_databases_create"));
    assert!(!names.contains(&"access_projects_create"));
    let call_tool = tools.iter().find(|t| t["name"] == "sylphx_call").unwrap();
    assert_eq!(call_tool["annotations"]["readOnlyHint"], true);
}

#[tokio::test]
async fn local_never_lists_an_unserved_tool() {
    let m = manifest();
    let cat = m["catalog"].as_array().unwrap();
    let s = server(vec![]);
    let r = call(&s, "tools/list", json!({})).await;
    let names: Vec<&str> = r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"data_databases_create"));
    for n in names {
        if let Some(e) = cat.iter().find(|e| e["tool"] == n) {
            assert_ne!(e["served"], false, "{n}");
        }
    }
}

#[tokio::test]
async fn remote_refuses_writes_and_unserved_methods_and_hides_them() {
    let m = manifest();
    let cat = m["catalog"].as_array().unwrap();
    let unserved = cat
        .iter()
        .find(|e| e["served"] == false && e["effect"] == "read")
        .unwrap()["method_id"]
        .as_str()
        .unwrap()
        .to_string();
    let s = remote(vec![]);
    let r = call(&s, "tools/call", json!({"name": "sylphx_call", "arguments": {"method_id": "access.projects.create", "args": {}}})).await;
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("API key"));
    let r = call(
        &s,
        "tools/call",
        json!({"name": "sylphx_call", "arguments": {"method_id": unserved, "args": {}}}),
    )
    .await;
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("not available yet"));
    // Hidden from describe and search.
    let r = call(&s, "tools/call", json!({"name": "sylphx_describe_method", "arguments": {"method_id": "access.projects.create"}})).await;
    assert_eq!(r["result"]["isError"], true);
    let r = call(
        &s,
        "tools/call",
        json!({"name": "sylphx_search_methods", "arguments": {"query": unserved}}),
    )
    .await;
    let hits = r["result"]["structuredContent"]["methods"]
        .as_array()
        .unwrap();
    assert!(hits.iter().all(|h| h["method_id"] != unserved.as_str()));
    // Read-effect methods that return credentials or secrets are refused
    // and hidden, whatever key the bearer is: a database's connection
    // credentials (`data:write`), a secret's value, a sandbox file.
    for id in [
        "data.databases.connect",
        "secrets.secrets.get",
        "secrets.secret_versions.get",
        "sandboxes.leases.read_file",
    ] {
        assert!(cat.iter().any(|e| e["method_id"] == id), "{id}");
        let r = call(
            &s,
            "tools/call",
            json!({"name": "sylphx_call", "arguments": {"method_id": id, "args": {}}}),
        )
        .await;
        assert_eq!(r["result"]["isError"], true, "{id}");
        assert!(
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("credentials or secrets"),
            "{id}"
        );
        let r = call(
            &s,
            "tools/call",
            json!({"name": "sylphx_search_methods", "arguments": {"query": id}}),
        )
        .await;
        let hits = r["result"]["structuredContent"]["methods"]
            .as_array()
            .unwrap();
        assert!(hits.iter().all(|h| h["method_id"] != id), "{id}");
    }
    // A write core tool is unknown remotely.
    let r = call(
        &s,
        "tools/call",
        json!({"name": "data_databases_create", "arguments": {}}),
    )
    .await;
    assert!(r["error"].is_object());
    // No request ever left.
    assert!(s
        .tools()
        .iter()
        .all(|t| t["name"] != "data_databases_create"));
}

#[tokio::test]
async fn skill_load_returns_one_pinned_skill_not_the_other_instructions() {
    let version = "orgs/o/projects/p/envs/e/agents/a/agent_versions/v";
    let s = server(vec![(
        200,
        json!({"name":version,"spec":{"skills":[
        {"name":"delivery","instructions":"immutable delivery instructions"},
        {"name":"other","instructions":"not requested"}]}}),
    )]);
    let r = call(
        &s,
        "tools/call",
        json!({"name":"skill_load","arguments":{
        "agent_version":version,"name":"delivery"}}),
    )
    .await;
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(
        r["result"]["structuredContent"]["skill"]["instructions"],
        "immutable delivery instructions"
    );
    assert_eq!(r["result"]["structuredContent"]["agent_version"], version);
    assert!(!r["result"]["content"].to_string().contains("not requested"));
}

#[tokio::test]
async fn skill_load_rejects_mutable_agent_reference() {
    let s = server(vec![]);
    let r = call(
        &s,
        "tools/call",
        json!({"name":"skill_load","arguments":{
        "agent_version":"orgs/o/projects/p/envs/e/agents/a","name":"delivery"}}),
    )
    .await;
    assert_eq!(r["result"]["isError"], true);
}

#[tokio::test]
async fn knowledge_search_uses_generated_recall_with_citations() {
    let s = server(vec![(
        200,
        json!({"context":{"citations":[{"episode":"readable"}]}}),
    )]);
    let r = call(&s,"tools/call",json!({"name":"knowledge_search","arguments":{
        "parent":"orgs/o/projects/p/envs/e","query":"customer preference","max_hops":0,"max_facts":2}})).await;
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(
        r["result"]["structuredContent"]["context"]["citations"][0]["episode"],
        "readable"
    );
}

#[tokio::test]
async fn remote_knowledge_tools_are_read_only() {
    let s = Server::remote(None::<Client<Recorder>>);
    let names: Vec<Value> = s
        .tools()
        .into_iter()
        .map(|tool| tool["name"].clone())
        .collect();
    assert!(names.contains(&json!("knowledge_search")));
    assert!(names.contains(&json!("knowledge_recall")));
    assert!(names.contains(&json!("skill_load")));
    assert!(!names.contains(&json!("knowledge_remember")));
}
