//! The generated SDK speaks the wire of spec §3: typed calls and `invoke`
//! through a recording transport, and the HTTP transport against a local
//! server (retries, idempotency, problem bodies).

// Build requests the way a consumer outside the crate must build
// `#[non_exhaustive]` types: from `Default`, field by field.
#![allow(clippy::field_reassign_with_default)]

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

use serde_json::json;
use sylphx::runtime::{self, Client, Error, HttpRequest, HttpResponse, Transport};
use sylphx::{access, common};

#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<HttpResponse>>,
}

impl Recorder {
    fn reply(self, status: u16, body: serde_json::Value) -> Self {
        self.replies.lock().unwrap().push_back(HttpResponse {
            status,
            request_id: Some("req_test".into()),
            body: serde_json::to_vec(&body).unwrap(),
        });
        self
    }
}

impl Transport for Recorder {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, Error>> + Send {
        self.sent.lock().unwrap().push(request);
        let reply = self.replies.lock().unwrap().pop_front();
        async move { reply.ok_or_else(|| Error::Transport("no reply queued".into())) }
    }
}

/// The recorder's futures are always ready, so one poll loop suffices.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

const PROJECT: &str = "orgs/org_hk0101j9/projects/prj_hk0101j9";
const ENV: &str = "orgs/org_hk0101j9/projects/prj_hk0101j9/envs/env_hk0101j9";

#[test]
fn create_sends_the_resource_as_the_body() {
    let client = Client::new(Recorder::default().reply(
        200,
        json!({
            "name": PROJECT,
            "uid": "prj_hk0101j9",
            "meta": {"generation": "3", "etag": "\"k3\"", "labels": {"team": "core"}},
            "spec": {"slug": "web"},
            "status": {"observed_generation": "3", "conditions": [{"type": "Ready", "status": "true"}]},
            "future_field": 1
        }),
    ));
    let mut project = access::Project::default();
    let mut spec = access::ProjectSpec::default();
    spec.slug = "web".into();
    project.spec = Some(spec);
    let mut meta = common::ResourceMeta::default();
    meta.labels.insert("team".into(), "core".into());
    project.meta = Some(meta);
    let mut request = access::CreateProjectRequest::default();
    request.parent = "orgs/org_hk0101j9".into();
    request.project = Some(project);

    let created = block_on(client.access().projects().create(request)).unwrap();
    assert_eq!(
        created.meta.as_ref().unwrap().generation,
        3,
        "int64 arrives as a string"
    );
    let status = created.status.unwrap();
    assert_eq!(status.observed_generation, 3);
    assert_eq!(
        status.conditions[0].status,
        Some(common::ConditionStatus::True)
    );

    let sent = client_sent(&client);
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/orgs/org_hk0101j9/projects");
    assert!(sent.query.is_empty(), "unset validate_only is not sent");
    assert!(sent.mutation);
    assert_eq!(
        sent.body,
        Some(json!({"meta": {"labels": {"team": "core"}}, "spec": {"slug": "web"}}))
    );
}

#[test]
fn a_malformed_name_never_leaves_the_client() {
    let client = Client::new(Recorder::default());
    let mut request = access::GetProjectRequest::default();
    request.name = "projects/prj_hk0101j9".into();
    let err = block_on(client.access().projects().get(request)).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    assert!(sent_all(&client).is_empty());
}

#[test]
fn list_all_follows_page_tokens() {
    let client = Client::new(
        Recorder::default()
            .reply(200, json!({"api_keys": [{"name": format!("{ENV}/api_keys/key_a")}], "next_page_token": "p2"}))
            .reply(200, json!({"api_keys": [{"name": format!("{ENV}/api_keys/key_b")}]})),
    );
    let mut request = access::ListApiKeysRequest::default();
    request.parent = ENV.into();
    request.page_size = 1;
    let keys = block_on(client.access().api_keys().list_all(request)).unwrap();
    assert_eq!(keys.len(), 2);
    let sent = sent_all(&client);
    assert_eq!(sent[0].path, format!("/v1/{ENV}/api_keys"));
    assert_eq!(
        sent[0].query,
        vec![("page_size".to_string(), "1".to_string())]
    );
    assert_eq!(
        sent[1].query,
        vec![
            ("page_size".to_string(), "1".to_string()),
            ("page_token".to_string(), "p2".to_string())
        ]
    );
    assert!(!sent[0].mutation);
}

#[test]
fn problem_details_become_typed_errors() {
    let client = Client::new(Recorder::default().reply(
        409,
        json!({
            "type": "https://sylphx.com/docs/errors/etag-mismatch",
            "title": "The resource changed since you read it.",
            "status": 409,
            "detail": "etag mismatch",
            "instance": "req_01",
            "code": "ETAG_MISMATCH",
            "grpc_status": "ABORTED",
            "retryable": true,
            "effect": "none",
            "details": [{"@type": "sylphx.common.v1.PreconditionFailure", "current_etag": "\"k3\""}]
        }),
    ));
    let mut env = access::Environment::default();
    env.name = ENV.into();
    let mut request = access::UpdateEnvironmentRequest::default();
    request.environment = Some(env);
    request.update_mask = "meta.labels".into();
    match block_on(client.access().envs().update(request)).unwrap_err() {
        Error::Api {
            code,
            status,
            retryable,
            effect,
            request_id,
            ..
        } => {
            assert_eq!(code, common::ErrorCode::EtagMismatch);
            assert_eq!(status, 409);
            assert!(retryable);
            assert_eq!(effect, Some(common::ErrorEffect::None));
            assert_eq!(request_id.as_deref(), Some("req_test"));
        }
        other => panic!("{other}"),
    }
    let sent = client_sent(&client);
    assert_eq!(sent.method, "PATCH");
    assert_eq!(sent.path, format!("/v1/{ENV}"));
    assert_eq!(
        sent.query,
        vec![("update_mask".to_string(), "meta.labels".to_string())]
    );
    assert_eq!(sent.body, Some(json!({"name": ENV})));
}

#[test]
fn a_long_running_delete_waits_for_its_operation() {
    let client = Client::new(
        Recorder::default()
            .reply(200, json!({"name": "orgs/org_hk0101j9/operations/op_1", "target": "orgs/org_hk0101j9", "verb": "delete", "done": false}))
            .reply(200, json!({"name": "orgs/org_hk0101j9/operations/op_1", "done": true,
                               "response": {"@type": "type.googleapis.com/google.protobuf.Empty"}})),
    );
    let mut request = access::DeleteOrgRequest::default();
    request.name = "orgs/org_hk0101j9".into();
    request.force = true;
    let op = block_on(client.access().orgs().delete(request)).unwrap();
    assert_eq!(op.operation.verb, Some(common::OperationVerb::Delete));
    let _: runtime::Empty = block_on(op.wait()).unwrap();
    let sent = sent_all(&client);
    assert_eq!(sent[0].method, "DELETE");
    assert_eq!(
        sent[0].query,
        vec![("force".to_string(), "true".to_string())]
    );
    assert_eq!(sent[1].path, "/v1/orgs/org_hk0101j9/operations/op_1:wait");
}

#[test]
fn a_custom_method_sends_the_request_minus_its_path() {
    let client = Client::new(
        Recorder::default().reply(200, json!({"name": format!("{ENV}/api_keys/key_a")})),
    );
    let mut request = access::RevokeApiKeyRequest::default();
    request.name = format!("{ENV}/api_keys/key_a");
    request.etag = "\"k3\"".into();
    block_on(client.access().api_keys().revoke(request)).unwrap();
    let sent = client_sent(&client);
    assert_eq!(sent.path, format!("/v1/{ENV}/api_keys/key_a:revoke"));
    assert_eq!(sent.body, Some(json!({"etag": "\"k3\""})));
}

#[test]
fn unknown_enum_values_survive_a_round_trip() {
    let kind: access::OrgKind = serde_json::from_value(json!("reseller")).unwrap();
    assert_eq!(kind, access::OrgKind::Unknown("reseller".into()));
    assert_eq!(serde_json::to_value(&kind).unwrap(), json!("reseller"));
    assert_eq!(
        serde_json::to_value(access::OrgKind::PartnerSub).unwrap(),
        json!("partner_sub")
    );
}

fn sent_all(client: &Client<Recorder>) -> Vec<HttpRequest> {
    client.transport().sent.lock().unwrap().clone()
}

fn client_sent(client: &Client<Recorder>) -> HttpRequest {
    let all = sent_all(client);
    assert_eq!(all.len(), 1, "{all:?}");
    all.into_iter().next().unwrap()
}

#[test]
fn whoami_is_a_service_level_get() {
    let client = Client::new(Recorder::default().reply(
        200,
        json!({"principal": "principal_hk01", "org": "orgs/org_hk0101j9", "scopes": ["access:read"]}),
    ));
    let me = block_on(client.access().whoami(Default::default())).unwrap();
    assert_eq!(me.org, "orgs/org_hk0101j9");
    let sent = client_sent(&client);
    assert_eq!((sent.method, sent.path.as_str()), ("GET", "/v1/whoami"));
    assert!(sent.body.is_none() && !sent.mutation);
}

#[test]
fn invoke_builds_the_same_request_as_the_typed_call() {
    let client = Client::new(
        Recorder::default().reply(200, json!({"name": format!("{ENV}/api_keys/key_a")})),
    );
    let out = block_on(client.invoke(
        "access.api_keys.revoke",
        json!({"name": format!("{ENV}/api_keys/key_a"), "etag": "\"k3\""}),
    ))
    .unwrap();
    assert_eq!(out["name"], json!(format!("{ENV}/api_keys/key_a")));
    let sent = client_sent(&client);
    assert_eq!(sent.path, format!("/v1/{ENV}/api_keys/key_a:revoke"));
    assert_eq!(sent.body, Some(json!({"etag": "\"k3\""})));
    assert!(sent.mutation);
}

#[test]
fn invoke_puts_the_resource_field_in_the_body_and_the_rest_in_the_query() {
    let client = Client::new(Recorder::default().reply(200, json!({})));
    block_on(client.invoke(
        "access.envs.update",
        json!({"environment": {"name": ENV, "meta": {"display_name": "Prod"}}, "update_mask": "meta.display_name"}),
    ))
    .unwrap();
    let sent = client_sent(&client);
    assert_eq!(sent.method, "PATCH");
    assert_eq!(sent.path, format!("/v1/{ENV}"));
    assert_eq!(
        sent.query,
        vec![("update_mask".to_string(), "meta.display_name".to_string())]
    );
    assert_eq!(
        sent.body,
        Some(json!({"name": ENV, "meta": {"display_name": "Prod"}}))
    );
}

#[test]
fn invoke_rejects_unknown_methods_and_bad_names() {
    let client = Client::new(Recorder::default());
    assert!(matches!(
        block_on(client.invoke("access.nothing.get", json!({}))),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        block_on(client.invoke("access.projects.get", json!({"name": "projects/p"}))),
        Err(Error::InvalidArgument(_))
    ));
    assert!(sent_all(&client).is_empty());
}

#[test]
fn every_method_is_in_the_table_once_and_sorted() {
    let ids: Vec<&str> = sylphx::methods::METHODS.iter().map(|m| m.id).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(ids, sorted);
    assert!(sylphx::methods::method("access.whoami").is_some());
}

mod http {
    //! The production transport against a local HTTP/1.1 server.

    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use sylphx::{Client, Error, HttpTransport};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Debug, Clone)]
    struct Seen {
        head: String,
        body: String,
    }

    /// Serves `replies` in order, one per connection, recording requests.
    async fn serve(
        replies: Vec<(u16, &'static str, serde_json::Value)>,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, extra, body) in replies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body_in) = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let head = text[..i].to_string();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let n = sock.read(&mut chunk).await.unwrap();
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (
                            head,
                            String::from_utf8_lossy(&buf[i + 4..i + 4 + len]).to_string(),
                        );
                    }
                };
                log.lock().unwrap().push(Seen {
                    head,
                    body: body_in,
                });
                let payload = body.to_string();
                let ctype = if status >= 400 {
                    "application/problem+json"
                } else {
                    "application/json"
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nsylphx-request-id: req_{status}\r\nconnection: close\r\n{extra}\r\n{payload}",
                    payload.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
            }
        });
        (url, seen)
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    fn client(url: &str) -> Client {
        Client::new(
            HttpTransport::builder()
                .base_url(url)
                .api_key("sylphx_sk_test")
                .build()
                .unwrap(),
        )
    }

    fn problem(code: &str, status: u16, retryable: bool) -> serde_json::Value {
        json!({"type": "about:blank", "title": code, "status": status, "detail": "d", "code": code,
               "grpc_status": "UNAVAILABLE", "retryable": retryable, "effect": "none"})
    }

    #[tokio::test]
    async fn a_mutation_retries_with_the_same_idempotency_key() {
        let (url, seen) = serve(vec![
            (503, "retry-after: 0\r\n", problem("UNAVAILABLE", 503, true)),
            (
                200,
                "",
                json!({"name": "orgs/org_a/projects/prj_a", "spec": {"slug": "web"}}),
            ),
        ])
        .await;
        let mut req = sylphx::access::CreateProjectRequest::default();
        req.parent = "orgs/org_a".into();
        let project = client(&url).access().projects().create(req).await.unwrap();
        assert_eq!(project.name, "orgs/org_a/projects/prj_a");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        let k1 = header(&seen[0].head, "idempotency-key").unwrap();
        let k2 = header(&seen[1].head, "idempotency-key").unwrap();
        assert_eq!(k1, k2, "one key per logical call");
        assert_eq!(uuid_version(k1), '7');
        assert_eq!(
            header(&seen[0].head, "authorization"),
            Some("Bearer sylphx_sk_test")
        );
        assert!(seen[0].head.starts_with("POST /v1/orgs/org_a/projects "));
        assert_eq!(seen[0].body, "{}");
    }

    #[tokio::test]
    async fn a_non_retryable_problem_is_returned_at_once() {
        let (url, seen) = serve(vec![(404, "", {
            let mut p = problem("RESOURCE_NOT_FOUND", 404, false);
            p["grpc_status"] = json!("NOT_FOUND");
            p
        })])
        .await;
        let mut req = sylphx::access::GetOrgRequest::default();
        req.name = "orgs/org_missing".into();
        match client(&url).access().orgs().get(req).await.unwrap_err() {
            Error::Api {
                status,
                retryable,
                request_id,
                ..
            } => {
                assert_eq!(status, 404);
                assert!(!retryable);
                assert_eq!(request_id.as_deref(), Some("req_404"));
            }
            other => panic!("{other}"),
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            header(&seen[0].head, "idempotency-key").is_none(),
            "a GET carries no key"
        );
    }

    #[tokio::test]
    async fn retries_stop_after_the_limit() {
        let (url, seen) = serve(vec![
            (
                429,
                "retry-after: 0\r\n",
                problem("RATE_LIMITED", 429, true),
            ),
            (
                429,
                "retry-after: 0\r\n",
                problem("RATE_LIMITED", 429, true),
            ),
        ])
        .await;
        let c = Client::new(
            HttpTransport::builder()
                .base_url(&url)
                .max_retries(1)
                .build()
                .unwrap(),
        );
        let err = c.invoke("access.whoami", json!({})).await.unwrap_err();
        assert!(matches!(err, Error::Api { status: 429, .. }), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    fn uuid_version(s: &str) -> char {
        s.chars().nth(14).unwrap()
    }
}
