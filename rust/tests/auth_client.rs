#![cfg(feature = "http")]
//! Recorded client-API responses over HTTP: no identity service or live account.
use serde_json::{json, Value};
use sylphx::auth::client::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const KEY: &str = "sylphx_pk_test_recorded";
const UA: &str = "Original Browser/1";

async fn recorded(
    responses: Vec<(u16, Value, Option<&str>)>,
) -> (AuthClient, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = AuthClient::with_base_url(
        KEY,
        UA,
        &format!("http://{}", listener.local_addr().unwrap()),
    )
    .unwrap();
    let responses: Vec<_> = responses
        .into_iter()
        .map(|(s, b, h)| (s, b, h.map(str::to_owned)))
        .collect();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body, retry) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buf = [0; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            let body = body.to_string();
            let retry = retry
                .map(|h| format!("Retry-After: {h}\r\n"))
                .unwrap_or_default();
            socket.write_all(format!("HTTP/1.1 {status} Recorded\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{retry}\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (client, task)
}

fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

fn assert_request(request: &str, path: &str) {
    assert!(request.starts_with(&format!("POST {path}?publishable_key={KEY} HTTP/1.1\r\n")));
    let headers = request
        .split_once("\r\n\r\n")
        .unwrap()
        .0
        .to_ascii_lowercase();
    assert!(headers.contains("user-agent: original browser/1\r\n"));
    assert!(!headers.contains("authorization:"));
}

#[tokio::test]
async fn server_default_body_user_agent_and_ticket() {
    let (client, task) = recorded(vec![
        (
            200,
            json!({"step":"email_otp","flow":"f","expires_in":600}),
            None,
        ),
        (
            200,
            json!({"ticket":"one-time-ticket","expires_in":120}),
            None,
        ),
    ])
    .await;
    let response = client
        .r#continue(ContinueRequest {
            email: "a@b.c".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        response,
        ContinueResponse::EmailOtp {
            flow: "f".into(),
            expires_in: 600
        }
    );
    assert_eq!(
        client
            .continue_verify(ContinueVerifyRequest {
                flow: "f".into(),
                code: "012345".into(),
                ..Default::default()
            })
            .await
            .unwrap(),
        ContinueVerifyResponse::Ticket {
            ticket: "one-time-ticket".into(),
            expires_in: 120
        }
    );
    let requests = task.await.unwrap();
    assert_request(&requests[0], "/v1/client/continue");
    assert_request(&requests[1], "/v1/client/continue/verify");
    assert_eq!(
        body(&requests[0]),
        json!({"email":"a@b.c","session_mode":"server"})
    );
    assert_eq!(body(&requests[1]), json!({"flow":"f","code":"012345"}));
}

#[tokio::test]
async fn password_fallback_browser_mode_and_mfa_proofs() {
    let (client, task) = recorded(vec![
        (200, json!({"step":"password"}), None),
        (200, json!({"step":"email_otp","flow":"f","expires_in":600}), None),
        (401, json!({"code":"mfa_required","error":"MFA required"}), None),
        (200, json!({"user":{"id":"u","email":"a@b.c","name":"A","email_verified":true},"session":{"id":"s","token":"bearer","expires_at":1800000000}}), None),
    ]).await;
    assert_eq!(
        client
            .r#continue(ContinueRequest {
                email: "a@b.c".into(),
                ..Default::default()
            })
            .await
            .unwrap(),
        ContinueResponse::Password
    );
    client
        .r#continue(ContinueRequest {
            email: "a@b.c".into(),
            method: Some(ContinueMethod::EmailOtp),
            captcha_token: Some("captcha".into()),
            session_mode: SessionMode::Browser,
        })
        .await
        .unwrap();
    let mut verify = ContinueVerifyRequest {
        flow: "f".into(),
        code: "012345".into(),
        ..Default::default()
    };
    match client.continue_verify(verify.clone()).await.unwrap_err() {
        AuthClientError::MfaRequired(error) => {
            assert_eq!(error.status, 401);
            assert_eq!(error.code, "mfa_required");
        }
        error => panic!("unexpected error: {error}"),
    }
    verify.factor_proofs.push(FactorProof {
        factor_type: "totp".into(),
        factor_id: "factor".into(),
        response: "654321".into(),
    });
    let response = client.continue_verify(verify).await.unwrap();
    assert_eq!(
        response,
        ContinueVerifyResponse::Session {
            user: AuthUser {
                id: "u".into(),
                email: "a@b.c".into(),
                name: "A".into(),
                email_verified: true
            },
            session: AuthSession {
                id: "s".into(),
                token: "bearer".into(),
                expires_at: 1800000000
            },
        }
    );
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 4); // No automatic replay of the MFA failure.
    assert_eq!(
        body(&requests[1]),
        json!({"email":"a@b.c","method":"email_otp","captcha_token":"captcha","session_mode":"browser"})
    );
    assert_eq!(
        body(&requests[3]),
        json!({"flow":"f","code":"012345","factor_proofs":[{"factor_type":"totp","factor_id":"factor","response":"654321"}]})
    );
}

#[tokio::test]
async fn rate_limit_body_and_header_mapping_on_both_routes() {
    for verify in [false, true] {
        for header in [None, Some("17"), Some("invalid")] {
            let (client, task) = recorded(vec![(
                429,
                json!({"code":"rate_limited","error":"slow down","retry_after":9}),
                header,
            )])
            .await;
            let error = if verify {
                client
                    .continue_verify(ContinueVerifyRequest::default())
                    .await
                    .unwrap_err()
            } else {
                client
                    .r#continue(ContinueRequest::default())
                    .await
                    .unwrap_err()
            };
            match error {
                AuthClientError::RateLimited(e) => {
                    assert_eq!(e.status, 429);
                    assert_eq!(e.code, "rate_limited");
                    assert_eq!(
                        e.retry_after_seconds,
                        Some(if header == Some("17") { 17 } else { 9 })
                    );
                }
                error => panic!("unexpected error: {error}"),
            }
            assert_eq!(task.await.unwrap().len(), 1);
        }
    }
}

#[test]
fn refuses_secret_keys_and_missing_or_invalid_user_agent() {
    assert!(matches!(
        AuthClient::new("sylphx_sk_secret", UA),
        Err(AuthClientError::InvalidArgument(_))
    ));
    assert!(matches!(
        AuthClient::new(KEY, ""),
        Err(AuthClientError::InvalidArgument(_))
    ));
    assert!(matches!(
        AuthClient::new(KEY, "injected\r\nheader"),
        Err(AuthClientError::InvalidArgument(_))
    ));
}
