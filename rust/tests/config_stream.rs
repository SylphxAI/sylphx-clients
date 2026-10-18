#![cfg(feature = "http")]

use futures_util::StreamExt;
use sylphx::config::WatchConfigFlagsRequest;
use sylphx::runtime::{Client, HttpTransport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn generated_watch_reads_sse_and_sends_the_reconnect_cursor() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut bytes = [0; 1024];
            let n = socket.read(&mut bytes).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&bytes[..n]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with(
            "GET /v1/orgs/o/projects/p/envs/e/config_flags:watch?if_none_match=held "
        ));
        assert!(request.to_lowercase().contains("accept: text/event-stream"));
        let body = ": keepalive\n\nid: newer\ndata: {\"etag\":\"newer\"}\n\n";
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let transport = HttpTransport::builder()
        .base_url(format!("http://{addr}"))
        .build()
        .unwrap();
    let client = Client::new(transport);
    let request: WatchConfigFlagsRequest = serde_json::from_value(serde_json::json!({
        "parent": "orgs/o/projects/p/envs/e", "if_none_match": "held"
    }))
    .unwrap();
    let mut stream = client.config().config_flags().watch(request).await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().etag, "newer");
    assert!(stream.next().await.is_none());
    server.await.unwrap();
}
