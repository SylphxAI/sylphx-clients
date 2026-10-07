//! The stdio server end to end: an rmcp client talks to it over a pipe, so
//! the handshake, tool list and call results are checked against the
//! official SDK rather than our own JSON-RPC.

use std::collections::VecDeque;
use std::sync::Mutex;

use rmcp::model::CallToolRequestParams;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use sylphx::{Client, Error, HttpRequest, HttpResponse, Transport};
use sylphx_mcp::Server;

#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<String>>,
    replies: Mutex<VecDeque<(u16, Value)>>,
}

impl Transport for Recorder {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, Error> {
        self.sent.lock().unwrap().push(request.path.clone());
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

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_rmcp_client_lists_and_calls_tools() {
    let rec = Recorder::default();
    rec.replies
        .lock()
        .unwrap()
        .push_back((200, json!({"principal": "users/u_1", "org": "orgs/org_a"})));
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(sylphx_mcp::serve_transport(
        Server::new(Some(Client::new(rec))),
        server_io,
    ));
    let client = ().serve(client_io).await.expect("client connects");

    let info = client.peer_info().expect("server info");
    let server = info.server_info.as_ref().expect("server implementation");
    assert_eq!(server.name, "sylphx");
    assert_eq!(server.version, env!("CARGO_PKG_VERSION"));
    assert!(info
        .instructions
        .as_deref()
        .is_some_and(|i| i.contains("access_whoami")));
    assert!(info.capabilities.tools.is_some());

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for want in [
        "access_whoami",
        "sylphx_search_methods",
        "sylphx_describe_method",
        "sylphx_call",
    ] {
        assert!(names.contains(&want), "{want} in tools/list");
    }

    // A core tool reaches its method and returns structured content.
    let r = client
        .call_tool(CallToolRequestParams::new("access_whoami"))
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(false));
    assert_eq!(r.structured_content.unwrap()["org"], "orgs/org_a");

    // Meta tools work without touching the API.
    let r = client
        .call_tool(
            CallToolRequestParams::new("sylphx_search_methods")
                .with_arguments(args(json!({"query": "databases create"}))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(false));
    assert!(r.structured_content.unwrap()["methods"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["tool"] == "data_databases_create"));

    // A destructive method without confirm is a readable tool error, not a call.
    let r = client
        .call_tool(
            CallToolRequestParams::new("sylphx_call").with_arguments(args(json!({
                "method_id": "access.api_keys.revoke",
                "args": {"name": "orgs/org_a/projects/p/envs/e/api_keys/k"}
            }))),
        )
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));
    assert!(r.content[0]
        .as_text()
        .unwrap()
        .text
        .contains("confirm: true"));

    // An unknown tool is a tool error too.
    let r = client
        .call_tool(CallToolRequestParams::new("no_such_tool"))
        .await
        .unwrap();
    assert_eq!(r.is_error, Some(true));

    client.cancel().await.unwrap();
}
