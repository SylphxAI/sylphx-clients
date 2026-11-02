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
fn a_multi_pattern_resource_takes_every_declared_parent() {
    // An org-wide key (AIP-123: `orgs/{org}/api_keys/{api_key}` beside the
    // environment's), through the typed call and through `invoke`.
    let client = Client::new(Recorder::default().reply(200, json!({})));
    let mut request = access::CreateApiKeyRequest::default();
    request.parent = "orgs/org_hk0101j9".into();
    block_on(client.access().api_keys().create(request)).unwrap();
    let sent = client_sent(&client);
    assert_eq!(
        (sent.method, sent.path.as_str()),
        ("POST", "/v1/orgs/org_hk0101j9/api_keys")
    );
    let client = Client::new(Recorder::default().reply(200, json!({})));
    block_on(client.invoke(
        "access.api_keys.revoke",
        json!({"name": "orgs/org_hk0101j9/api_keys/key_a"}),
    ))
    .unwrap();
    assert_eq!(
        client_sent(&client).path,
        "/v1/orgs/org_hk0101j9/api_keys/key_a:revoke"
    );
    let client = Client::new(Recorder::default().reply(200, json!({})));
    block_on(client.invoke(
        "access.api_keys.revoke",
        json!({"name": format!("{PROJECT}/api_keys/key_a")}),
    ))
    .unwrap();
    assert_eq!(
        client_sent(&client).path,
        format!("/v1/{PROJECT}/api_keys/key_a:revoke")
    );
    // An incomplete key name is refused before the wire.
    let client = Client::new(Recorder::default());
    assert!(matches!(
        block_on(client.invoke(
            "access.api_keys.revoke",
            json!({"name": "orgs/o/projects/p/api_keys"})
        )),
        Err(Error::InvalidArgument(_))
    ));
    assert!(sent_all(&client).is_empty());
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

    enum Reply {
        Json {
            status: u16,
            extra: &'static str,
            body: serde_json::Value,
            delay: std::time::Duration,
        },
        StallHeaders,
        StallBody,
        DisconnectBody,
    }

    /// Serves `replies` in order, one per connection, recording requests.
    async fn serve(
        replies: Vec<(u16, &'static str, serde_json::Value)>,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        serve_script(
            replies
                .into_iter()
                .map(|(status, extra, body)| Reply::Json {
                    status,
                    extra,
                    body,
                    delay: std::time::Duration::ZERO,
                })
                .collect(),
        )
        .await
    }

    async fn serve_script(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for reply in replies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body_in) = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    assert_ne!(n, 0, "client disconnected before sending its request");
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
                            assert_ne!(n, 0, "client disconnected before sending its body");
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
                let (status, extra, body) = match reply {
                    Reply::Json {
                        status,
                        extra,
                        body,
                        delay,
                    } => {
                        tokio::time::sleep(delay).await;
                        (status, extra, body)
                    }
                    Reply::DisconnectBody => {
                        sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{").await.unwrap();
                        sock.shutdown().await.ok();
                        continue;
                    }
                    Reply::StallHeaders | Reply::StallBody => {
                        if matches!(reply, Reply::StallBody) {
                            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{").await.unwrap();
                        }
                        // Keep the response incomplete until the client deadline
                        // closes the socket, without a server-side timer.
                        let _ = sock.read_to_end(&mut Vec::new()).await;
                        continue;
                    }
                };
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
                if sock.write_all(resp.as_bytes()).await.is_err() {
                    break;
                }
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

    fn build_request() -> serde_json::Value {
        json!({"parent": "orgs/org_a/projects/prj_a", "command": ["true"], "timeout": "180s"})
    }

    #[tokio::test]
    async fn build_pending_is_observation_not_an_error_retry() {
        for max_retries in [0, 3] {
            let mut replies = vec![
                (
                    409,
                    "retry-after: 0\r\n",
                    problem("IDEMPOTENCY_IN_PROGRESS", 409, true)
                );
                6
            ];
            replies.push((
                200,
                "",
                json!({"name": "orgs/org_a/operations/op_a", "done": false}),
            ));
            let (url, seen) = serve(replies).await;
            let client = Client::new(
                HttpTransport::builder()
                    .base_url(&url)
                    .max_retries(max_retries)
                    .build()
                    .unwrap(),
            );
            let result = client
                .invoke("build.builds.run", build_request())
                .await
                .unwrap();
            assert_eq!(result["name"], "orgs/org_a/operations/op_a");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 7);
            for sent in seen.iter() {
                assert_eq!(sent.body, seen[0].body);
                assert_eq!(
                    header(&sent.head, "idempotency-key"),
                    header(&seen[0].head, "idempotency-key")
                );
                assert!(header(&sent.head, "idempotency-key").is_some());
                assert!(header(&sent.head, "sylphx-request-timeout").is_none());
            }
        }
    }

    #[tokio::test]
    async fn pending_does_not_reset_or_spend_the_transient_budget() {
        let mut replies = vec![(503, "retry-after: 0\r\n", problem("UNAVAILABLE", 503, true))];
        replies.extend(vec![
            (
                409,
                "retry-after: 0\r\n",
                problem("IDEMPOTENCY_IN_PROGRESS", 409, true)
            );
            6
        ]);
        replies.push((503, "retry-after: 0\r\n", problem("UNAVAILABLE", 503, true)));
        let (url, seen) = serve(replies).await;
        let client = Client::new(
            HttpTransport::builder()
                .base_url(&url)
                .max_retries(1)
                .build()
                .unwrap(),
        );
        let err = client
            .invoke("build.builds.run", build_request())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Api { status: 503, .. }), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn pending_requires_exact_status_code_and_retryable_and_honors_deadline() {
        for (status, code, retryable, extra) in [
            (409, "OUTCOME_UNKNOWN", false, ""),
            (409, "IDEMPOTENCY_IN_PROGRESS", false, ""),
            (409, "ETAG_MISMATCH", true, ""),
            (503, "IDEMPOTENCY_IN_PROGRESS", true, ""),
            (409, "IDEMPOTENCY_IN_PROGRESS", true, "retry-after: 30\r\n"),
        ] {
            let (url, seen) = serve(vec![(status, extra, problem(code, status, retryable))]).await;
            let client = Client::new(
                HttpTransport::builder()
                    .base_url(&url)
                    .max_retries(0)
                    .timeout(std::time::Duration::from_millis(100))
                    .build()
                    .unwrap(),
            );
            let error = client
                .invoke("build.builds.run", build_request())
                .await
                .unwrap_err();
            match error {
                Error::Api { code: actual, .. } => assert_eq!(actual.as_str(), code),
                other => panic!("{other}"),
            }
            assert_eq!(seen.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn pending_honors_retry_after_and_the_original_deadline() {
        use std::time::{Duration, Instant};
        let (url, seen) = serve(vec![
            (
                409,
                "retry-after: 1\r\n",
                problem("IDEMPOTENCY_IN_PROGRESS", 409, true),
            ),
            (
                409,
                "retry-after: 1\r\n",
                problem("IDEMPOTENCY_IN_PROGRESS", 409, true),
            ),
        ])
        .await;
        let client = Client::new(
            HttpTransport::builder()
                .base_url(&url)
                .max_retries(0)
                .build()
                .unwrap(),
        )
        .with_deadline(Instant::now() + Duration::from_millis(1800));
        let start = Instant::now();
        let error = client
            .invoke("build.builds.run", build_request())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Api { status: 409, .. }), "{error}");
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert!(start.elapsed() < Duration::from_millis(1800));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let first: u64 = header(&seen[0].head, "sylphx-request-timeout")
            .unwrap()
            .parse()
            .unwrap();
        let second: u64 = header(&seen[1].head, "sylphx-request-timeout")
            .unwrap()
            .parse()
            .unwrap();
        assert!(second + 900 < first);
    }

    #[tokio::test]
    async fn body_disconnect_reconnects_the_same_build_request_and_key() {
        let (url, seen) = serve_script(vec![
            Reply::DisconnectBody,
            Reply::Json {
                status: 200,
                extra: "",
                body: json!({"name": "orgs/org_a/operations/op_a"}),
                delay: std::time::Duration::ZERO,
            },
        ])
        .await;
        let client = Client::new(
            HttpTransport::builder()
                .base_url(&url)
                .max_retries(1)
                .build()
                .unwrap(),
        );
        let result = client
            .invoke("build.builds.run", build_request())
            .await
            .unwrap();
        assert_eq!(result["name"], "orgs/org_a/operations/op_a");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].body, seen[1].body);
        assert_eq!(
            header(&seen[0].head, "idempotency-key"),
            header(&seen[1].head, "idempotency-key")
        );
    }

    #[tokio::test]
    async fn cancelling_pending_observation_stops_resends() {
        use std::time::Duration;
        let (url, seen) = serve(vec![
            (
                409,
                "retry-after: 1\r\n",
                problem("IDEMPOTENCY_IN_PROGRESS", 409, true)
            );
            2
        ])
        .await;
        let handle = tokio::spawn(async move {
            client(&url)
                .invoke("build.builds.run", build_request())
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while seen.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
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

    #[tokio::test]
    async fn a_shared_deadline_bounds_retries_and_keeps_the_server_answer() {
        let mut stale = problem("ACCESS_SNAPSHOT_STALE", 503, true);
        stale["detail"] = json!("the access snapshot is stale");
        let (url, seen) = serve(vec![(503, "retry-after: 30\r\n", stale)]).await;
        let c = client(&url)
            .with_deadline(std::time::Instant::now() + std::time::Duration::from_millis(300));
        let t = std::time::Instant::now();
        let err = c.invoke("access.whoami", json!({})).await.unwrap_err();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "the 30s Retry-After must not be slept out: {:?}",
            t.elapsed()
        );
        match err {
            Error::Api {
                status,
                retryable,
                detail,
                ..
            } => {
                assert_eq!(status, 503);
                assert!(retryable);
                assert_eq!(detail, "the access snapshot is stale");
            }
            other => panic!("{other}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_shorter_configured_timeout_still_wins_over_a_shared_deadline() {
        let mut stale = problem("ACCESS_SNAPSHOT_STALE", 503, true);
        stale["detail"] = json!("the access snapshot is stale");
        let (url, seen) = serve(vec![(503, "retry-after: 30\r\n", stale)]).await;
        let c = Client::new(
            HttpTransport::builder()
                .base_url(&url)
                .api_key("sylphx_sk_test")
                .timeout(std::time::Duration::from_millis(300))
                .build()
                .unwrap(),
        )
        .with_deadline(std::time::Instant::now() + std::time::Duration::from_secs(60));
        let t = std::time::Instant::now();
        let err = c.invoke("access.whoami", json!({})).await.unwrap_err();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(5),
            "the shared deadline must not widen the 300ms timeout: {:?}",
            t.elapsed()
        );
        match err {
            Error::Api { status, detail, .. } => {
                assert_eq!(status, 503);
                assert_eq!(detail, "the access snapshot is stale");
            }
            other => panic!("{other}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    async fn deadline_read(client: &Client, protocol: bool) -> Result<(), Error> {
        if protocol {
            let mut request = sylphx::data::GetValueRequest::default();
            request.namespace_id = "deadlines".into();
            request.key = "test".into();
            client.data().kv().get(request).await.map(|_| ())
        } else {
            client.invoke("access.whoami", json!({})).await.map(|_| ())
        }
    }

    #[tokio::test]
    async fn socket_deadlines_cover_stalled_headers_and_bodies_on_every_origin() {
        use std::time::{Duration, Instant};

        for protocol in [false, true] {
            for custom_http in [false, true] {
                for shared_deadline in [false, true] {
                    for stall_body in [false, true] {
                        let reply = if stall_body {
                            Reply::StallBody
                        } else {
                            Reply::StallHeaders
                        };
                        let (url, seen) = serve_script(vec![reply]).await;
                        let mut builder = HttpTransport::builder()
                            .base_url(&url)
                            .origin("https://api.data.sylphx.com", &url)
                            .api_key("sylphx_sk_test")
                            .api_version("2026-10-01")
                            .max_retries(0);
                        if custom_http {
                            // No client-level timeout: the transport's call budget
                            // must work even with a caller-supplied HTTP client.
                            builder = builder.http_client(
                                reqwest::Client::builder().no_proxy().build().unwrap(),
                            );
                        }
                        if !shared_deadline {
                            builder = builder.timeout(Duration::from_millis(100));
                        }
                        let client = Client::new(builder.build().unwrap()).with_deadline(
                            Instant::now()
                                + if shared_deadline {
                                    Duration::from_millis(100)
                                } else {
                                    Duration::from_secs(60)
                                },
                        );
                        let error = tokio::time::timeout(
                            Duration::from_secs(2),
                            deadline_read(&client, protocol),
                        )
                        .await
                        .expect("a stalled socket must obey the call deadline")
                        .unwrap_err();
                        assert!(matches!(error, Error::Transport(_)), "{error}");
                        let seen = seen.lock().unwrap();
                        assert_eq!(seen.len(), 1);
                        assert_eq!(header(&seen[0].head, "sylphx-version").is_some(), !protocol);
                        let budget = header(&seen[0].head, "sylphx-request-timeout");
                        if protocol {
                            assert!(
                                budget.is_none(),
                                "protocol headers stay separate from local deadlines"
                            );
                            assert!(header(&seen[0].head, "idempotency-key").is_none());
                        } else {
                            assert!(budget.unwrap().parse::<u64>().unwrap() <= 100);
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn a_long_call_deadline_does_not_replace_a_shorter_socket_timeout() {
        use std::time::{Duration, Instant};

        for body in [false, true] {
            let (url, seen) = serve_script(vec![if body {
                Reply::StallBody
            } else {
                Reply::StallHeaders
            }])
            .await;
            let client = Client::new(
                HttpTransport::builder()
                    .base_url(&url)
                    .http_client(
                        reqwest::Client::builder()
                            .no_proxy()
                            .timeout(Duration::from_millis(250))
                            .build()
                            .unwrap(),
                    )
                    .max_retries(0)
                    .build()
                    .unwrap(),
            )
            .with_deadline(Instant::now() + Duration::from_secs(60));
            let error = tokio::time::timeout(Duration::from_secs(2), deadline_read(&client, false))
                .await
                .expect("the caller's shorter HTTP timeout must still apply")
                .unwrap_err();
            assert!(matches!(error, Error::Transport(_)), "{error}");
            // The HTTP timeout may fire before headers are fully sent on a busy
            // runtime; it must still win over the 60s logical-call deadline.
            assert!(seen.lock().unwrap().len() <= 1);
        }
    }

    #[tokio::test]
    async fn retries_share_the_remaining_socket_budget_on_every_origin() {
        use std::time::Duration;

        for protocol in [false, true] {
            let (url, seen) = serve_script(vec![
                Reply::Json {
                    status: 503,
                    extra: "retry-after: 0\r\n",
                    body: json!({}),
                    delay: Duration::from_millis(600),
                },
                // This succeeds if each attempt incorrectly gets a fresh 1200ms.
                Reply::Json {
                    status: 200,
                    extra: "",
                    body: json!({}),
                    delay: Duration::from_millis(900),
                },
            ])
            .await;
            let client = Client::new(
                HttpTransport::builder()
                    .base_url(&url)
                    .origin("https://api.data.sylphx.com", &url)
                    .http_client(reqwest::Client::builder().no_proxy().build().unwrap())
                    .timeout(Duration::from_millis(1200))
                    .max_retries(1)
                    .build()
                    .unwrap(),
            );
            let error =
                tokio::time::timeout(Duration::from_secs(3), deadline_read(&client, protocol))
                    .await
                    .expect("retries must not extend the call budget")
                    .unwrap_err();
            assert!(matches!(error, Error::Transport(_)), "{error}");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2, "the retry must actually reach the server");
            if protocol {
                assert!(seen
                    .iter()
                    .all(|request| header(&request.head, "sylphx-request-timeout").is_none()));
            } else {
                let first: u64 = header(&seen[0].head, "sylphx-request-timeout")
                    .unwrap()
                    .parse()
                    .unwrap();
                let second: u64 = header(&seen[1].head, "sylphx-request-timeout")
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!(second < first, "retry budget must shrink");
            }
        }
    }

    #[tokio::test]
    async fn protocol_retry_after_outside_the_budget_keeps_the_server_answer() {
        use std::time::Duration;

        let (url, seen) = serve(vec![(
            503,
            "retry-after: 30\r\n",
            json!({"error":{"code":"unavailable","message":"retry later"}}),
        )])
        .await;
        let client = Client::new(
            HttpTransport::builder()
                .origin("https://api.data.sylphx.com", &url)
                .timeout(Duration::from_millis(100))
                .max_retries(3)
                .build()
                .unwrap(),
        );
        let error = tokio::time::timeout(Duration::from_secs(2), deadline_read(&client, true))
            .await
            .expect("Retry-After must not be slept past the deadline")
            .unwrap_err();
        assert!(matches!(error, Error::Api { status: 503, .. }), "{error}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    fn data_client(url: &str, retries: u32) -> Client {
        Client::new(
            HttpTransport::builder()
                .origin("https://api.data.sylphx.com", url)
                .api_key("sylphx_sk_test")
                .max_retries(retries)
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn the_data_plane_sends_one_effect_id_per_call_and_reuses_it_on_retry() {
        let object = json!({"bucketId": "uploads", "key": "org_1/cv 2026+final.pdf",
                            "contentType": "application/pdf", "size": "5", "version": "3"});
        let (url, seen) = serve(vec![
            (
                503,
                "retry-after: 0\r\n",
                json!({"error": {"code": "unavailable", "message": "retry"}}),
            ),
            (200, "", json!({"object": object})),
        ])
        .await;
        let mut req = sylphx::data::PutObjectRequest::default();
        req.bucket_id = "uploads".into();
        req.key = "org_1/cv 2026+final.pdf".into();
        req.body = "aGVsbG8=".into();
        req.content_type = "application/pdf".into();
        let out = data_client(&url, 1)
            .data()
            .objects()
            .put(req)
            .await
            .unwrap();
        // Data answers ProtoJSON: camelCase names and string uint64s decode.
        let stored = out.object.unwrap();
        assert_eq!(stored.content_type, "application/pdf");
        assert_eq!((stored.size, stored.version), (5, 3));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert!(seen[0]
            .head
            .starts_with("PUT /v1/objects/uploads/org_1/cv%202026%2Bfinal.pdf "));
        let e1 = header(&seen[0].head, "sylphx-effect-id").unwrap();
        assert_eq!(
            Some(e1),
            header(&seen[1].head, "sylphx-effect-id"),
            "reused on retry"
        );
        assert_eq!(uuid_version(e1), '7');
        assert!(header(&seen[0].head, "idempotency-key").is_none());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(),
            json!({"body": "aGVsbG8=", "content_type": "application/pdf"})
        );
    }

    #[tokio::test]
    async fn every_data_plane_read_carries_an_effect_id_and_errors_map_to_registry_codes() {
        let (url, seen) = serve(vec![(
            404,
            "",
            json!({"error": {"code": "not_found", "message": "value not found", "occurrenceId": "occ_1"}}),
        )])
        .await;
        let mut req = sylphx::data::GetValueRequest::default();
        req.namespace_id = "ratelimit".into();
        req.key = "ip:1".into();
        match data_client(&url, 0).data().kv().get(req).await.unwrap_err() {
            Error::Api {
                code,
                status,
                detail,
                ..
            } => {
                assert_eq!(code, sylphx::common::ErrorCode::ResourceNotFound);
                assert_eq!(status, 404);
                assert_eq!(detail, "not_found: value not found");
            }
            other => panic!("{other}"),
        }
        let seen = seen.lock().unwrap();
        assert!(seen[0].head.starts_with("GET /v1/kv/ratelimit/ip%3A1 "));
        assert!(header(&seen[0].head, "sylphx-effect-id").is_some());
    }

    #[tokio::test]
    async fn an_explicit_zero_increment_is_sent() {
        let (url, seen) = serve(vec![(
            200,
            "",
            json!({"value": {"namespaceId": "n", "key": "k", "value": "Mw=="}}),
        )])
        .await;
        let mut req = sylphx::data::IncrementValueRequest::default();
        req.namespace_id = "n".into();
        req.key = "k".into();
        req.delta = Some(0);
        data_client(&url, 0)
            .data()
            .kv()
            .increment(req)
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert!(seen[0].head.starts_with("POST /v1/kv-increments "));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen[0].body).unwrap(),
            json!({"namespace_id": "n", "key": "k", "delta": "0"})
        );
    }

    #[test]
    fn a_retired_per_project_host_is_never_called() {
        assert!(sylphx::runtime::retired_base_url(
            "https://gold-time-8ea5.api.sylphx.com/v1"
        ));
        assert!(sylphx::runtime::retired_base_url(
            "sylphx://pk_x:sk_y@gold-time-8ea5.api.sylphx.com"
        ));
        assert!(!sylphx::runtime::retired_base_url("https://api.sylphx.com"));
        let t = HttpTransport::builder()
            .base_url("https://gold-time-8ea5.api.sylphx.com")
            .build()
            .unwrap();
        assert_eq!(t.base_url(), "https://api.sylphx.com");
    }

    #[test]
    fn a_key_with_an_empty_segment_is_refused() {
        assert!(sylphx::runtime::protocol_param("a//b", true, "key").is_err());
        assert!(sylphx::runtime::protocol_param("a/b", false, "bucket_id").is_err());
        assert_eq!(
            sylphx::runtime::protocol_param("a b/c", true, "key").unwrap(),
            "a%20b/c"
        );
    }

    fn uuid_version(s: &str) -> char {
        s.chars().nth(14).unwrap()
    }
}
