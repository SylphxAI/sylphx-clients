//! Money's app-facing contract: discover the key's environment, read prices,
//! create hosted checkout, and check an end-user entitlement.
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

use serde_json::{json, Value};
use sylphx::{money, Client, Error, HttpRequest, HttpResponse, Transport};

const ENV: &str = "orgs/o/projects/p/envs/e";

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<Value>>,
}

impl Recorder {
    fn replies(values: impl IntoIterator<Item = Value>) -> Self {
        Self {
            replies: Mutex::new(values.into_iter().collect()),
            ..Self::default()
        }
    }
}

impl Transport for Recorder {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, Error>> + Send {
        self.requests.lock().unwrap().push(request);
        let body = self.replies.lock().unwrap().pop_front().unwrap();
        async move {
            Ok(HttpResponse {
                status: 200,
                request_id: None,
                body: serde_json::to_vec(&body).unwrap(),
            })
        }
    }
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("recording transport must be ready"),
    }
}

#[test]
fn catalogue_uses_the_environment_discovered_by_whoami() {
    let client = Client::new(Recorder::replies([
        json!({"env": ENV}),
        json!({"spec": {"products": [{"key": "plan", "prices": [{"key": "monthly", "unit_amounts": {"USD": "1100"}}]}]}}),
    ]));
    let me = ready(client.access().whoami(Default::default())).unwrap();
    let request: money::GetCatalogRequest = serde_json::from_value(json!({
        "name": format!("{}/price_catalogs/default", me.env)
    }))
    .unwrap();
    let catalog = ready(client.money().price_catalogs().get(request)).unwrap();
    let products = catalog.spec.unwrap().products;
    assert_eq!(products[0].prices[0].unit_amounts["USD"], "1100");
    let requests = client.transport().requests.lock().unwrap();
    assert_eq!(requests[0].path, "/v1/whoami");
    assert_eq!(
        requests[1].path,
        format!("/v1/{ENV}/price_catalogs/default")
    );
    assert_eq!(requests[1].method, "GET");
    assert!(!requests[1].mutation);
}

#[test]
fn checkout_typed_and_dynamic_calls_send_only_the_session_body() {
    let client = Client::new(Recorder::replies([
        json!({"url": "https://checkout.example/session"}),
        json!({"url": "https://checkout.example/session"}),
    ]));
    let session = json!({
        "subject": {"end_user": "learner"},
        "line_items": [{"price": "monthly", "quantity": 1}],
        "success_url": "https://app.example/billing/return",
        "cancel_url": "https://app.example/pricing",
        "currency_code": "USD",
        "metadata": {"consent_id": "consent-1"}
    });
    let args = json!({"parent": ENV, "checkout_session": session});
    let request: money::CreateCheckoutSessionRequest =
        serde_json::from_value(args.clone()).unwrap();
    let result = ready(client.money().checkout_sessions().create(request)).unwrap();
    assert_eq!(result.url, "https://checkout.example/session");
    ready(client.invoke("money.checkout_sessions.create", args)).unwrap();
    let requests = client.transport().requests.lock().unwrap();
    assert_eq!(requests[0], requests[1]);
    assert_eq!(requests[0].path, format!("/v1/{ENV}/checkout_sessions"));
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].body, Some(session));
    assert!(
        requests[0].mutation,
        "SDK transport must attach mutation idempotency"
    );
}

#[test]
fn entitlement_check_preserves_subject_feature_limit_and_expiry() {
    let client = Client::new(Recorder::replies([
        json!({"entitled": true, "limit": "50", "expire_time": "2030-01-01T00:00:00Z"}),
        json!({}),
    ]));
    let args = json!({"parent": ENV, "subject": {"end_user": "learner"}, "feature": "pro"});
    let request: money::CheckEntitlementRequest = serde_json::from_value(args.clone()).unwrap();
    let grant = ready(client.money().entitlement_grants().check(request)).unwrap();
    assert!(grant.entitled);
    assert_eq!(grant.limit, "50");
    assert_eq!(grant.expire_time, "2030-01-01T00:00:00Z");
    let request: money::CheckEntitlementRequest = serde_json::from_value(args).unwrap();
    assert!(
        !ready(client.money().entitlement_grants().check(request))
            .unwrap()
            .entitled
    );
    let requests = client.transport().requests.lock().unwrap();
    assert_eq!(
        requests[0].path,
        format!("/v1/{ENV}/entitlement_grants:check")
    );
    assert_eq!(
        requests[0].body,
        Some(json!({"subject": {"end_user": "learner"}, "feature": "pro"}))
    );
    // The HTTP runtime marks POSTs for idempotency even when the contract
    // effect is read; the request still needs only billing:read.
    assert!(requests[0].mutation);
}

#[test]
fn a_key_without_an_environment_cannot_call_money() {
    let client = Client::new(Recorder::replies([json!({"org": "orgs/o"})]));
    let me = ready(client.access().whoami(Default::default())).unwrap();
    let result = ready(client.invoke(
        "money.price_catalogs.get",
        json!({
            "name": format!("{}/price_catalogs/default", me.env)
        }),
    ));
    assert!(matches!(result, Err(Error::InvalidArgument(_))));
    assert_eq!(client.transport().requests.lock().unwrap().len(), 1);
}
