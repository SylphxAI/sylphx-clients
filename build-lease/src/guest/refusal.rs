//! Bounded HTTP refusal correlation, without request credentials or file queries.

use reqwest::{header::HeaderMap, Response, StatusCode, Url};
use serde_json::{json, Map, Value};

use super::Fault;

pub(super) struct Details {
    status: StatusCode,
    metadata: Value,
}

impl Details {
    pub(super) fn capture(method: &'static str, response: &Response) -> Self {
        Self::from_parts(
            method,
            response.status(),
            response.url(),
            response.headers(),
        )
    }

    fn from_parts(
        method: &'static str,
        status: StatusCode,
        url: &Url,
        headers: &HeaderMap,
    ) -> Self {
        // The response URL can follow a redirect. Name it separately from the
        // initiating method; do not claim to know a redirected request method.
        // origin() excludes URL userinfo; path excludes query and fragment.
        let endpoint: String = format!("{}{}", url.origin().ascii_serialization(), url.path())
            .chars()
            .take(1536)
            .collect();
        let mut selected = Map::new();
        for name in [
            "content-type",
            "server",
            "cf-ray",
            "x-request-id",
            "traceparent",
        ] {
            if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
                selected.insert(
                    name.into(),
                    json!(value.chars().take(160).collect::<String>()),
                );
            }
        }
        Self {
            status,
            metadata: json!({
                "request_method": method,
                "response_endpoint": endpoint,
                "response_headers": selected,
            }),
        }
    }

    pub(super) fn fault(self, body: &str) -> Fault {
        let text: String = body.chars().take(300).collect();
        let message = format!(
            "guest answered {}: {text}; diagnostic={}",
            self.status, self.metadata
        );
        // Keep the existing lease-loss classification. Metadata supplies no
        // authority to repeat a request or create a replacement operation.
        match self.status.as_u16() {
            404 | 409 | 410 | 502 | 503 => Fault::Gone(message),
            _ => Fault::Other(message),
        }
    }
}

pub(super) async fn read(method: &'static str, response: Response) -> Fault {
    let details = Details::capture(method, &response);
    details.fault(&response.text().await.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest::Guest;
    use std::collections::BTreeMap;

    #[test]
    fn correlation_omits_url_credentials_queries_fragments_and_sensitive_headers() {
        let url = Url::parse("https://url-user:url-password@guest.example:8443/files?path=file-canary#fragment-canary").unwrap();
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("content-type", "text/html"),
            ("server", "edge"),
            ("cf-ray", "ray-123"),
            ("x-request-id", "request-456"),
            ("traceparent", "trace-789"),
            ("set-cookie", "cookie-canary"),
            ("authorization", "Bearer auth-canary"),
            ("x-access-token", "token-canary"),
            ("location", "https://other.example/?secret=location-canary"),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        let fault =
            Details::from_parts("POST", StatusCode::FORBIDDEN, &url, &headers).fault("denied");
        assert!(matches!(fault, Fault::Other(_)));
        let text = fault.text();
        for expected in [
            "403 Forbidden",
            "POST",
            "https://guest.example:8443/files",
            "text/html",
            "ray-123",
            "request-456",
            "trace-789",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
        for forbidden in [
            "url-user",
            "url-password",
            "file-canary",
            "fragment-canary",
            "cookie-canary",
            "auth-canary",
            "token-canary",
            "location-canary",
        ] {
            assert!(!text.contains(forbidden), "{forbidden}: {text}");
        }
    }

    #[test]
    fn refusal_metadata_is_bounded_and_keeps_failure_classification() {
        let url = Url::parse(&format!("https://guest.example/{}", "p".repeat(3000))).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "r".repeat(4000).parse().unwrap());
        let details = Details::from_parts("GET", StatusCode::SERVICE_UNAVAILABLE, &url, &headers);
        assert_eq!(
            details.metadata["response_endpoint"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            1536
        );
        assert_eq!(
            details.metadata["response_headers"]["x-request-id"]
                .as_str()
                .unwrap()
                .len(),
            160
        );
        let fault = details.fault(&"é".repeat(400));
        assert!(matches!(fault, Fault::Gone(_)));
        assert_eq!(fault.text().matches('é').count(), 300);
        for status in [404, 409, 410, 502, 503] {
            assert!(matches!(
                Details::from_parts(
                    "POST",
                    StatusCode::from_u16(status).unwrap(),
                    &url,
                    &headers
                )
                .fault("no"),
                Fault::Gone(_)
            ));
        }
        for status in [400, 401, 403, 429, 500] {
            assert!(matches!(
                Details::from_parts(
                    "POST",
                    StatusCode::from_u16(status).unwrap(),
                    &url,
                    &headers
                )
                .fault("no"),
                Fault::Other(_)
            ));
        }
    }

    /// One loopback exchange exercises the response capture in each real
    /// caller, including download's separate sandbox/file 404 distinction.
    #[tokio::test]
    async fn guest_http_callers_retain_correlation_and_file_404_semantics() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for operation in [
            "upload",
            "download",
            "start",
            "signal",
            "touch",
            "sandbox404",
            "file404",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let (status, body) = match operation {
                    "touch" => ("503 Service Unavailable", "unavailable"),
                    "sandbox404" => ("404 Not Found", "sandbox missing"),
                    "file404" => ("404 Not Found", "file missing"),
                    _ => ("403 Forbidden", "denied"),
                };
                socket.write_all(format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\ncontent-type: text/html\r\ncf-ray: loopback-proof\r\nset-cookie: cookie-canary\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned()
            });
            let guest = Guest::new(
                &format!("http://{address}"),
                "sandbox",
                "request-token-canary",
            )
            .unwrap();
            let result = match operation {
                "upload" => guest.upload("/file-query-canary", "user", b"data").await,
                "download" | "sandbox404" | "file404" => guest
                    .download("/file-query-canary", "user")
                    .await
                    .map(|_| ()),
                "start" => guest
                    .start("user", &["true".into()], "", &BTreeMap::new())
                    .await
                    .map(|_| ()),
                "signal" => guest.signal(7, "TERM").await,
                "touch" => guest.touch("/file-query-canary").await,
                _ => unreachable!(),
            };
            let request = server.await.unwrap();
            if operation == "file404" {
                assert!(result.is_ok(), "{result:?}");
                continue;
            }
            let fault = result.unwrap_err();
            let metadata: Value =
                serde_json::from_str(fault.text().split_once("; diagnostic=").unwrap().1).unwrap();
            let method = if matches!(operation, "download" | "sandbox404") {
                "GET"
            } else {
                "POST"
            };
            assert_eq!(metadata["request_method"], method);
            assert!(request.starts_with(method));
            assert_eq!(metadata["response_headers"]["cf-ray"], "loopback-proof");
            for secret in ["file-query-canary", "request-token-canary", "cookie-canary"] {
                assert!(!fault.text().contains(secret), "{fault:?}");
            }
            assert_eq!(
                matches!(fault, Fault::Gone(_)),
                matches!(operation, "touch" | "sandbox404")
            );
        }
    }
}
