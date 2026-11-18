//! Per-call replay keys supplied by CLI callers, using the SDK's retry path.

pub(crate) struct IdempotencyTransport<'a> {
    inner: &'a sylphx::HttpTransport,
    headers: [(String, String); 1],
}

impl<'a> IdempotencyTransport<'a> {
    pub(crate) fn new(inner: &'a sylphx::HttpTransport, key: impl Into<String>) -> Self {
        Self {
            inner,
            headers: [("Idempotency-Key".into(), key.into())],
        }
    }
}

impl sylphx::runtime::Transport for IdempotencyTransport<'_> {
    async fn send(
        &self,
        request: sylphx::runtime::HttpRequest,
    ) -> Result<sylphx::runtime::HttpResponse, sylphx::Error> {
        self.inner.send_with_headers(request, &self.headers).await
    }
}
