use super::*;
use serde_json::{json, Value};
use std::{collections::VecDeque, sync::Mutex};
use sylphx::{Error, HttpRequest, HttpResponse, Transport};

const ATTEMPT: &str =
    "orgs/org_a/projects/prj_a/envs/env_a/build_attempts/bda_00000000000000000000000001";

struct Script(Mutex<VecDeque<(&'static str, u16, Value)>>);

impl Transport for Script {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, Error> {
        let (cursor, status, body) = self.0.lock().unwrap().pop_front().unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, format!("/v1/{ATTEMPT}:readLogs"));
        if cursor.is_empty() {
            assert!(req.query.is_empty());
        } else {
            assert_eq!(req.query, vec![("page_token".into(), cursor.into())]);
        }
        Ok(HttpResponse {
            status,
            request_id: None,
            body: serde_json::to_vec(&body).unwrap(),
        })
    }
}

#[tokio::test]
async fn resume_and_follow_drains_ordered_output_before_failing() {
    let client = Client::new(Script(Mutex::new(VecDeque::from([
        (
            "saved",
            200,
            json!({"lines":["second", "third"],"next_page_token":"c3","has_more":true,"state":"building"}),
        ),
        (
            "c3",
            200,
            json!({"next_page_token":"c3","state":"building"}),
        ),
        (
            "c3",
            200,
            json!({"lines":["last"],"next_page_token":"c4","done":true,"state":"failed"}),
        ),
    ]))));
    let mut seen = Vec::new();
    let result = follow(
        &client,
        ATTEMPT,
        "saved".into(),
        true,
        Duration::ZERO,
        |page| seen.extend(page.lines.clone()),
    )
    .await;
    assert_eq!(seen, ["second", "third", "last"]);
    assert!(matches!(result, Err(Failure::Refused(s)) if s == "Hosting build attempt failed"));
    assert!(client.transport().0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn follow_returns_success_or_refusal_without_falling_back() {
    for (status, body) in [
        (200, json!({"done":true,"state":"succeeded"})),
        (
            403,
            json!({"status":403,"code":"PERMISSION_DENIED","detail":"scope does not cover attempt"}),
        ),
    ] {
        let client = Client::new(Script(Mutex::new(VecDeque::from([("", status, body)]))));
        let result = follow(
            &client,
            ATTEMPT,
            String::new(),
            true,
            Duration::ZERO,
            |page| assert!(page.lines.is_empty()),
        )
        .await;
        if status == 200 {
            assert!(result.is_ok());
        } else {
            assert!(matches!(
                result,
                Err(Failure::Api(Error::Api { status: 403, .. }))
            ));
        }
        assert!(client.transport().0.lock().unwrap().is_empty());
    }
}

#[test]
fn only_explicit_native_names_are_dispatched_to_release() {
    assert!(is_attempt(ATTEMPT));
    assert!(is_attempt("bda_00000000000000000000000001"));
    assert!(!is_attempt(
        "orgs/org_a/projects/prj_a/builds/bda_00000000000000000000000001"
    ));
    assert!(!is_attempt("b01j8x"));
}
