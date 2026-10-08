//! Offline flags acceptance, through the SDK's actual wire client.
#![allow(clippy::field_reassign_with_default)]
use serde_json::json;
use std::future::Future;
use std::sync::Mutex;
use sylphx::config::EvaluationContext;
use sylphx::{Client, Error, HttpRequest, HttpResponse, Transport};

struct Api(Mutex<bool>);
impl Transport for Api {
    fn send(&self, _: HttpRequest) -> impl Future<Output = Result<HttpResponse, Error>> + Send {
        let mut online = self.0.lock().unwrap();
        let response = if *online {
            *online = false;
            Ok(HttpResponse {
                status: 200,
                request_id: None,
                body: serde_json::to_vec(&json!({"evaluations": [
                    {"config_flag": "enabled", "value": false, "reason": "default"},
                    {"config_flag": "layout", "value": "new", "reason": "default"},
                    {"config_flag": "limit", "value": 0, "reason": "default"},
                    {"config_flag": "settings", "value": {"color": "blue"}, "reason": "default"}
                ]}))
                .unwrap(),
            })
        } else {
            Err(Error::Transport("API unreachable".into()))
        };
        async move { response }
    }
}
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("test transport is always ready"),
    }
}
#[test]
fn offline_returns_cached_values_or_call_site_defaults() {
    let client = Client::new(Api(Mutex::new(true)));
    let parent = "orgs/org_a/projects/prj_a/envs/env_a";
    let mut alice = EvaluationContext::default();
    alice.user_id = "alice".into();
    let mut flags = client.flags(parent, alice.clone());
    assert!(!block_on(flags.bool("enabled", true)));
    assert!(!block_on(flags.bool("enabled", true)));
    assert_eq!(block_on(flags.string("layout", "old")), "new");
    assert_eq!(block_on(flags.number("limit", 10.0)), 0.0);
    assert_eq!(
        block_on(flags.json("settings", json!({}))),
        json!({"color": "blue"})
    );
    assert_eq!(block_on(flags.string("missing", "default")), "default");
    assert!(block_on(flags.bool("layout", true)));
    let mut bob = EvaluationContext::default();
    bob.user_id = "bob".into();
    for mut fresh in [
        client.flags(parent, bob),
        client.flags("orgs/org_a/projects/prj_a/envs/env_b", alice),
    ] {
        assert!(block_on(fresh.bool("enabled", true)));
        assert_eq!(block_on(fresh.string("layout", "old")), "old");
        assert_eq!(block_on(fresh.number("limit", 10.0)), 10.0);
        assert_eq!(
            block_on(fresh.json("settings", json!({"offline": true}))),
            json!({"offline": true})
        );
    }
}
