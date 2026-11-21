use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn read_headers(socket: &mut tokio::net::TcpStream) -> String {
    let mut req = Vec::new();
    while !req.ends_with(b"\r\n\r\n") {
        assert!(req.len() < 4096);
        req.push(socket.read_u8().await.unwrap());
    }
    String::from_utf8(req).unwrap()
}

async fn with_reply(
    reply: &str,
    expected: u64,
    max: u64,
) -> (Result<(), OutputTransferError>, Vec<u8>) {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let reply = reply.to_owned();
    let task = tokio::spawn(async move {
        let (mut socket, _) = server.accept().await.unwrap();
        let req = read_headers(&mut socket).await;
        let req = req.to_lowercase();
        assert!(req.starts_with("get /files?path=/out/build%3fresult&username=user http/1.1\r\n"));
        assert!(req.contains("\r\ne2b-sandbox-id: lease-id\r\n"));
        assert!(req.contains("\r\nx-access-token: test-token\r\n"));
        assert!(req.contains("\r\nauthorization: basic dxnlcjo=\r\n"));
        socket.write_all(reply.as_bytes()).await.unwrap();
    });
    let guest = Guest::new(&format!("http://{addr}"), "lease-id", "test-token").unwrap();
    let mut staging = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        guest.download_exact_into("/out/build?result", "user", expected, max, &mut staging),
    )
    .await
    .expect("the loopback transfer must terminate");
    task.await.unwrap();
    (result, staging)
}

#[tokio::test]
async fn copies_exact_bytes_from_authenticated_lease() {
    let (result, staging) = with_reply(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n",
        4,
        4,
    )
    .await;
    assert_eq!(result, Ok(()));
    assert_eq!(staging, b"abcd");
}

#[tokio::test]
async fn refuses_oversize_before_writing_and_rejects_stream_overruns() {
    let mut staging = Vec::new();
    let guest = Guest::new("invalid-url", "lease-id", "test-token").unwrap();
    assert_eq!(
        guest
            .download_exact_into("/out/build", "user", 5, 4, &mut staging)
            .await,
        Err(OutputTransferError::LimitExceeded)
    );
    let (result, staging) = with_reply("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n", 4, 4).await;
    assert_eq!(result, Err(OutputTransferError::LimitExceeded));
    assert!(staging.is_empty());
    let (result, staging) = with_reply("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n2\r\ncd\r\n1\r\ne\r\n0\r\n\r\n", 4, 4).await;
    assert_eq!(result, Err(OutputTransferError::LimitExceeded));
    assert_eq!(staging, b"abcd");
}

#[tokio::test]
async fn refuses_short_or_broken_transfers_and_non_file_statuses() {
    let (result, _) = with_reply(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n0\r\n\r\n",
        4,
        4,
    )
    .await;
    assert_eq!(result, Err(OutputTransferError::LengthMismatch));
    let (result, _) = with_reply("HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nab", 4, 4).await;
    assert_eq!(result, Err(OutputTransferError::Transport));
    for (status, text) in [
        (206, "Partial Content"),
        (401, "Unauthorized"),
        (404, "Not Found"),
    ] {
        let reply = format!("HTTP/1.1 {status} {text}\r\nContent-Length: 4\r\n\r\nabcd");
        let (result, staging) = with_reply(&reply, 4, 4).await;
        assert_eq!(
            result,
            Err(OutputTransferError::Refused(
                StatusCode::from_u16(status).unwrap()
            ))
        );
        assert!(staging.is_empty());
    }
}

#[tokio::test]
async fn never_follows_a_redirect_with_the_lease_token() {
    let sink = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{}/file\r\nContent-Length: 0\r\n\r\n",
        sink.local_addr().unwrap()
    );
    let (result, staging) = with_reply(&redirect, 4, 4).await;
    assert_eq!(result, Err(OutputTransferError::Refused(StatusCode::FOUND)));
    assert!(staging.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.accept())
            .await
            .is_err()
    );
}
