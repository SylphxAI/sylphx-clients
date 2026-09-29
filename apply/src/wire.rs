//! The one place apply talks to the API. It builds [`HttpRequest`]s and
//! decodes answers with the SDK runtime (`sylphx::Client::call`, so errors
//! are the SDK's own problem-details errors); the only thing added is the
//! per-request headers the standard methods need (`If-Match`, and an
//! `Idempotency-Key` apply derives so a retried Job replays its answers).

use std::future::Future;

use serde_json::Value;
use sylphx::{Client, Error, HttpRequest, HttpResponse, Transport};

/// Sends one request with extra headers.
pub trait Wire: Send + Sync {
    fn send(
        &self,
        request: HttpRequest,
        headers: Vec<(String, String)>,
    ) -> impl Future<Output = Result<HttpResponse, Error>> + Send;
}

impl Wire for sylphx::HttpTransport {
    async fn send(
        &self,
        request: HttpRequest,
        headers: Vec<(String, String)>,
    ) -> Result<HttpResponse, Error> {
        self.send_with_headers(request, &headers).await
    }
}

/// A [`Transport`] that adds fixed headers to whatever it sends.
pub(crate) struct Headed<'a, W: Wire> {
    wire: &'a W,
    headers: Vec<(String, String)>,
}

impl<W: Wire> Transport for Headed<'_, W> {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, Error> {
        self.wire.send(request, self.headers.clone()).await
    }
}

/// A client that sends `headers` with every request.
pub(crate) fn client<W: Wire>(wire: &W, headers: Vec<(String, String)>) -> Client<Headed<'_, W>> {
    Client::new(Headed { wire, headers })
}

/// `GET /v1/{path}` with a query.
pub(crate) fn get(path: &str, query: Vec<(String, String)>) -> HttpRequest {
    HttpRequest {
        method: "GET",
        path: format!("/v1/{path}"),
        query,
        body: None,
        mutation: false,
        origin: None,
        effect_ids: false,
    }
}

/// `PATCH /v1/{name}` with a query and the Resource as the body.
pub(crate) fn patch(name: &str, query: Vec<(String, String)>, body: Value) -> HttpRequest {
    HttpRequest {
        method: "PATCH",
        path: format!("/v1/{name}"),
        query,
        body: Some(body),
        mutation: true,
        origin: None,
        effect_ids: false,
    }
}
