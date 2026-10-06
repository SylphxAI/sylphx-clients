//! `sylphx::auth::resource` against a fake introspection endpoint on a local
//! TCP listener: metadata, challenges, the introspection wire, scopes, cache.
#![cfg(feature = "resource")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use sylphx::auth::resource::{grant_from_introspection, Grant, ProtectedResource, Rejection};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RES: &str = "https://mcp.example.com/mcp";
const ISS: &str = "https://auth.example.com";

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn active(exp: i64) -> Value {
    json!({"active": true, "scope": "read", "client_id": "c1", "token_type": "Bearer",
           "exp": exp, "iat": now(), "sub": "user_1", "iss": ISS, "aud": RES})
}

struct Fake {
    url: String,
    hits: Arc<AtomicUsize>,
    last: Arc<Mutex<String>>,
    status: Arc<Mutex<u16>>,
    body: Arc<Mutex<String>>,
}

impl Fake {
    async fn start(body: Value) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/introspect", listener.local_addr().unwrap());
        let f = Fake {
            url,
            hits: Arc::default(),
            last: Arc::default(),
            status: Arc::new(Mutex::new(200)),
            body: Arc::new(Mutex::new(body.to_string())),
        };
        let (hits, last, status, body) = (
            f.hits.clone(),
            f.last.clone(),
            f.status.clone(),
            f.body.clone(),
        );
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let len = text[..i]
                            .to_ascii_lowercase()
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= i + 4 + len {
                            break;
                        }
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                *last.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
                let b = body.lock().unwrap().clone();
                let reply = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    *status.lock().unwrap(),
                    b.len(),
                    b
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        f
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn rs(endpoint: &str, ttl: Duration) -> ProtectedResource {
    ProtectedResource::builder(RES, ISS)
        .scopes_supported(["read", "write"])
        .resource_name("Kalkas MCP")
        .client_credentials("rs_client", "s3cret-value")
        .introspection_endpoint(endpoint)
        .cache_ttl(ttl)
        .build()
        .unwrap()
}

#[test]
fn metadata_for_path_and_root_resources() {
    let r = rs("http://127.0.0.1:1/x", Duration::ZERO);
    assert_eq!(
        r.metadata(),
        json!({"resource": RES, "authorization_servers": [ISS], "scopes_supported": ["read","write"],
               "bearer_methods_supported": ["header"], "resource_name": "Kalkas MCP"})
    );
    assert_eq!(
        r.metadata_path(),
        "/.well-known/oauth-protected-resource/mcp"
    );
    assert_eq!(
        r.metadata_url(),
        "https://mcp.example.com/.well-known/oauth-protected-resource/mcp"
    );
    let m = r.metadata_response();
    assert_eq!(m.status, 200);
    assert!(m
        .headers
        .contains(&("cache-control".into(), "public, max-age=300".into())));

    let root = ProtectedResource::builder("https://mcp.example.com", ISS)
        .client_credentials("a", "b")
        .build()
        .unwrap();
    assert_eq!(
        root.metadata_path(),
        "/.well-known/oauth-protected-resource"
    );
    assert!(root.metadata().get("resource_name").is_none());
}

#[test]
fn builder_validates() {
    let b = |res: &str, iss: &str| {
        ProtectedResource::builder(res, iss)
            .client_credentials("a", "b")
            .build()
    };
    assert!(b("http://api.example.com/mcp", ISS).is_err());
    assert!(b("https://api.example.com/mcp#f", ISS).is_err());
    assert!(b(RES, "http://auth.example.com").is_err());
    assert!(b("http://localhost:3000/mcp", ISS).is_ok());
    assert!(ProtectedResource::builder(RES, ISS).build().is_err());
    assert!(ProtectedResource::builder(RES, ISS)
        .client_credentials("a", "")
        .build()
        .is_err());
}

#[tokio::test]
async fn missing_and_malformed_headers() {
    let r = rs("http://127.0.0.1:1/x", Duration::ZERO);
    let c = r.authenticate(None).await.unwrap_err();
    assert_eq!(c.status(), 401);
    assert_eq!(
        c.www_authenticate(),
        "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource/mcp\", scope=\"read write\""
    );
    for bad in ["Bearer", "Bearer a b", "Bearer "] {
        let c = r.authenticate(Some(bad)).await.unwrap_err();
        assert_eq!(c.status(), 400, "{bad}");
        assert!(c.www_authenticate().contains("error=\"invalid_request\""));
        assert!(c.www_authenticate().contains("resource_metadata="));
    }
    // Another scheme is no bearer credential at all: a plain 401 that starts
    // the client's OAuth discovery (RFC 6750 §3.1).
    let c = r.authenticate(Some("Basic abc")).await.unwrap_err();
    assert_eq!(c.status(), 401);
    assert!(!c.www_authenticate().contains("error="));
    assert!(c.www_authenticate().contains("resource_metadata="));
    // `authorize` names the scopes the operation needs in the 401.
    let c = r.authorize(None, &["write"]).await.unwrap_err();
    assert_eq!(c.status(), 401);
    assert!(
        c.www_authenticate().ends_with("scope=\"write\""),
        "{}",
        c.www_authenticate()
    );
}

#[tokio::test]
async fn a_trailing_slash_on_the_issuer_still_matches() {
    let r = ProtectedResource::builder(RES, "https://acme.auth.sylphx.net/")
        .client_credentials("rs_client", "s3cret-value")
        .build()
        .unwrap();
    assert_eq!(
        r.metadata()["authorization_servers"],
        serde_json::json!(["https://acme.auth.sylphx.net"])
    );
}

#[tokio::test]
async fn accepts_and_sends_the_exact_wire() {
    let fake = Fake::start(active(now() + 600)).await;
    let r = rs(&fake.url, Duration::ZERO);
    let g = r
        .authenticate(Some("bEaReR identity_oauth_at_a+b/c"))
        .await
        .unwrap();
    assert_eq!(g.subject, "user_1");
    assert_eq!(g.client_id, "c1");
    assert_eq!(g.scopes, vec!["read"]);
    let req = fake.last.lock().unwrap().clone();
    let lower = req.to_ascii_lowercase();
    assert!(req.starts_with("POST /introspect HTTP/1.1"));
    // base64("rs_client:s3cret-value")
    assert!(req.contains("authorization: Basic cnNfY2xpZW50OnMzY3JldC12YWx1ZQ=="));
    assert!(lower.contains("content-type: application/x-www-form-urlencoded"));
    assert!(req.ends_with("token=identity_oauth_at_a%2Bb%2Fc"));
}

#[tokio::test]
async fn refuses_bad_tokens_with_401() {
    let cases: Vec<(&str, Value)> = vec![
        ("aud", {
            let mut v = active(now() + 600);
            v["aud"] = json!("https://other");
            v
        }),
        ("iss", {
            let mut v = active(now() + 600);
            v["iss"] = json!("https://evil");
            v
        }),
        ("exp", active(now() - 5)),
        ("dpop", {
            let mut v = active(now() + 600);
            v["token_type"] = json!("DPoP");
            v
        }),
        (
            "inactive",
            json!({"active": false, "scope": "", "client_id": "", "token_type": "", "exp": 0, "iat": 0, "sub": "", "iss": "", "aud": ""}),
        ),
    ];
    for (name, body) in cases {
        let fake = Fake::start(body).await;
        let r = rs(&fake.url, Duration::from_secs(30));
        let c = r
            .authenticate(Some("Bearer tok_secret_xyz"))
            .await
            .unwrap_err();
        assert_eq!(c.status(), 401, "{name}");
        let w = c.www_authenticate();
        assert!(w.contains("error=\"invalid_token\""), "{name}");
        assert!(!w.contains("tok_secret_xyz") && !c.reply().body.contains("tok_secret_xyz"));
        // invalid answers are never cached
        let _ = r.authenticate(Some("Bearer tok_secret_xyz")).await;
        assert_eq!(fake.hits(), 2, "{name}");
    }
}

#[tokio::test]
async fn aud_array_is_accepted() {
    let mut v = active(now() + 600);
    v["aud"] = json!(["https://x", RES]);
    let fake = Fake::start(v).await;
    assert!(rs(&fake.url, Duration::ZERO)
        .authenticate(Some("Bearer t"))
        .await
        .is_ok());
}

#[tokio::test]
async fn server_failure_is_503_and_not_cached() {
    let fake = Fake::start(json!({})).await;
    *fake.status.lock().unwrap() = 500;
    let r = rs(&fake.url, Duration::from_secs(30));
    let c = r.authenticate(Some("Bearer t")).await.unwrap_err();
    assert_eq!(c.status(), 503);
    let reply = c.reply();
    assert!(reply.headers.contains(&("retry-after".into(), "5".into())));
    assert!(!reply.headers.iter().any(|(k, _)| k == "www-authenticate"));
    assert_eq!(reply.body, r#"{"error":"temporarily_unavailable"}"#);
    *fake.status.lock().unwrap() = 200;
    *fake.body.lock().unwrap() = active(now() + 600).to_string();
    assert!(r.authenticate(Some("Bearer t")).await.is_ok());
    assert_eq!(fake.hits(), 2);
}

#[tokio::test]
async fn unreachable_is_503() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/i", l.local_addr().unwrap());
    drop(l);
    let c = rs(&url, Duration::ZERO)
        .authenticate(Some("Bearer t"))
        .await
        .unwrap_err();
    assert_eq!(c.status(), 503);
}

#[tokio::test]
async fn insufficient_scope_lists_all_needed() {
    let fake = Fake::start(active(now() + 600)).await;
    let r = rs(&fake.url, Duration::ZERO);
    let g = Grant {
        subject: "u".into(),
        client_id: "c".into(),
        scopes: vec!["read".into()],
        expires_at: now() + 60,
        issued_at: 0,
    };
    assert!(r.require(&g, &["read"]).is_ok());
    let c = r.require(&g, &["read", "write"]).unwrap_err();
    assert_eq!(c.status(), 403);
    let w = c.www_authenticate();
    assert!(w.contains("error=\"insufficient_scope\"") && w.contains("scope=\"read write\""));
    let c = r
        .authorize(Some("Bearer t"), &["read", "write"])
        .await
        .unwrap_err();
    assert_eq!(c.status(), 403);
}

#[tokio::test]
async fn cache_hits_ttl_and_exp() {
    let fake = Fake::start(active(now() + 600)).await;
    let r = rs(&fake.url, Duration::from_millis(300));
    r.authenticate(Some("Bearer t")).await.unwrap();
    r.authenticate(Some("Bearer t")).await.unwrap();
    assert_eq!(fake.hits(), 1);
    tokio::time::sleep(Duration::from_millis(400)).await;
    r.authenticate(Some("Bearer t")).await.unwrap();
    assert_eq!(fake.hits(), 2);

    // a token expiring in 1 s is cached for at most 1 s, and never past exp
    *fake.body.lock().unwrap() = active(now() + 1).to_string();
    let r = rs(&fake.url, Duration::from_secs(30));
    let before = fake.hits();
    r.authenticate(Some("Bearer short")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(r.authenticate(Some("Bearer short")).await.is_err());
    assert_eq!(fake.hits(), before + 2);
}

#[test]
fn debug_redacts_secret() {
    let r = rs("http://127.0.0.1:1/x", Duration::ZERO);
    let d = format!("{r:?}");
    assert!(!d.contains("s3cret-value"));
    assert!(d.contains("redacted"));
}

#[test]
fn pure_core_names_the_failed_check() {
    let n = now();
    assert!(grant_from_introspection(&active(n + 10), RES, ISS, n).is_ok());
    assert_eq!(
        grant_from_introspection(&json!([]), RES, ISS, n),
        Err(Rejection::Malformed)
    );
    let mut v = active(n + 10);
    v["sub"] = json!("");
    assert_eq!(
        grant_from_introspection(&v, RES, ISS, n),
        Err(Rejection::Subject)
    );
}
