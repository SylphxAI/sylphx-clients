//! The lease's guest daemon over the E2B envd protocol, as Sandboxes serves it
//! at `status.endpoints.guest_uri`: Connect-RPC with the JSON codec
//! (`process.Process/Start` is a server stream of enveloped JSON messages)
//! and HTTP `/files`. The lease is named by the `E2b-Sandbox-Id` header and
//! the caller by a lease token in `X-Access-Token`; the guest user travels as
//! HTTP Basic, as the E2B SDKs send it.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};

/// How the guest failed a call.
#[derive(Debug)]
pub enum Fault {
    /// The machine is gone or not running: look at the lease to learn why.
    Gone(String),
    /// A process stream broke or closed before its end event. The machine may
    /// be fine, so the run is worth one more try (it is `retryable`).
    Stream(String),
    /// Anything else; the text is the reason.
    Other(String),
}

impl Fault {
    pub fn text(&self) -> &str {
        match self {
            Fault::Gone(s) | Fault::Stream(s) | Fault::Other(s) => s,
        }
    }
}

#[derive(Clone)]
pub struct Guest {
    http: reqwest::Client,
    base: String,
    sandbox: String,
    token: String,
}

/// One message of a running process.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Start(u64),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The process ended: its exit code, with a signal death as 128 + the
    /// signal's number, as a local shell reports it.
    End(i32),
    /// The stream ended with an error (the end-of-stream message).
    Error(String),
}

/// Reassembles Connect envelopes (`flags u8, length u32 BE, message`) from a
/// byte stream that splits them anywhere.
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 5 {
                break;
            }
            let flags = self.buf[0];
            let len =
                u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
            if self.buf.len() < 5 + len {
                break;
            }
            let msg: Vec<u8> = self.buf.drain(..5 + len).skip(5).collect();
            let v: Value = serde_json::from_slice(&msg).unwrap_or(Value::Null);
            if flags & 0x02 != 0 {
                // End of stream: `{}` or `{"error": {"code", "message"}}`.
                if let Some(err) = v.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| err.get("code").and_then(Value::as_str))
                        .unwrap_or("the process stream failed");
                    out.push(Event::Error(m.to_string()));
                }
                continue;
            }
            out.extend(events(&v));
        }
        out
    }
}

fn events(v: &Value) -> Vec<Event> {
    let ev = &v["event"];
    let mut out = Vec::new();
    if let Some(p) = ev.pointer("/start/pid").and_then(Value::as_u64) {
        out.push(Event::Start(p));
    }
    if let Some(d) = ev.get("data") {
        if let Some(b) = d.get("stdout").and_then(Value::as_str) {
            out.push(Event::Stdout(B64.decode(b).unwrap_or_default()));
        }
        if let Some(b) = d.get("stderr").and_then(Value::as_str) {
            out.push(Event::Stderr(B64.decode(b).unwrap_or_default()));
        }
    }
    if let Some(end) = ev.get("end") {
        out.push(Event::End(exit_code(end)));
    }
    out
}

/// envd's end event: `exitCode` (absent when 0), `exited`, and `status`
/// (`exit status 3`, `signal: killed`).
fn exit_code(end: &Value) -> i32 {
    let status = end.get("status").and_then(Value::as_str).unwrap_or("");
    if let Some(sig) = status.strip_prefix("signal: ") {
        return 128 + signal_number(sig);
    }
    match end
        .get("exitCode")
        .or_else(|| end.get("exit_code"))
        .and_then(Value::as_i64)
    {
        Some(c) if c >= 0 => c as i32,
        Some(_) => 128 + signal_number(""),
        None if end.get("exited").and_then(Value::as_bool) == Some(false) => 128,
        None => 0,
    }
}

fn signal_number(name: &str) -> i32 {
    match name {
        "hangup" => 1,
        "interrupt" => 2,
        "quit" => 3,
        "illegal instruction" => 4,
        "aborted" => 6,
        "bus error" => 7,
        "floating point exception" => 8,
        "killed" => 9,
        "segmentation fault" => 11,
        "broken pipe" => 13,
        "terminated" => 15,
        _ => 0,
    }
}

fn basic(user: &str) -> String {
    format!("Basic {}", B64.encode(format!("{user}:")))
}

fn q(s: &str) -> String {
    url_escape(s)
}

/// Percent-encodes a query value.
fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A multipart/form-data body with one part named `file`, as the E2B SDKs
/// upload.
pub fn multipart(boundary: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"file\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

pub fn envelope(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap_or_default();
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(0);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn fault(status: reqwest::StatusCode, body: &str) -> Fault {
    let text: String = body.chars().take(300).collect();
    match status.as_u16() {
        404 | 409 | 410 | 502 | 503 => Fault::Gone(format!("guest answered {status}: {text}")),
        _ => Fault::Other(format!("guest answered {status}: {text}")),
    }
}

fn net(e: reqwest::Error) -> Fault {
    Fault::Gone(format!("guest unreachable: {e}"))
}

/// A started process: read its events with [`Proc::next`].
pub struct Proc {
    resp: reqwest::Response,
    frames: Frames,
    pending: std::collections::VecDeque<Event>,
    /// Bytes received, for `bytes_down`.
    pub bytes: u64,
}

impl Proc {
    /// The next event; `None` when the stream closed without an end event.
    pub async fn next(&mut self) -> Result<Option<Event>, Fault> {
        loop {
            if let Some(e) = self.pending.pop_front() {
                return Ok(Some(e));
            }
            match self.resp.chunk().await {
                Ok(Some(c)) => {
                    self.bytes += c.len() as u64;
                    self.pending.extend(self.frames.push(&c));
                }
                Ok(None) => return Ok(None),
                Err(e) => return Err(Fault::Stream(format!("the process stream broke: {e}"))),
            }
        }
    }
}

impl Guest {
    pub fn new(base: &str, sandbox: &str, token: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            sandbox: sandbox.to_string(),
            token: token.to_string(),
        })
    }

    pub fn set_token(&mut self, token: &str) {
        self.token = token.to_string();
    }

    fn req(&self, method: reqwest::Method, path: &str, user: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}/{path}", self.base))
            .header("e2b-sandbox-id", &self.sandbox)
            .header("x-access-token", &self.token)
            .header("authorization", basic(user))
    }

    /// Writes `bytes` to `path` (absolute), as `user`.
    pub async fn upload(&self, path: &str, user: &str, bytes: &[u8]) -> Result<(), Fault> {
        let boundary = format!("sylphx{:016x}", rand_u64());
        let resp = self
            .req(
                reqwest::Method::POST,
                &format!("files?path={}&username={user}", q(path)),
                user,
            )
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(multipart(&boundary, bytes))
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(net)?;
        if !resp.status().is_success() {
            let s = resp.status();
            return Err(fault(s, &resp.text().await.unwrap_or_default()));
        }
        Ok(())
    }

    /// Reads `path`; `None` when it does not exist.
    pub async fn download(&self, path: &str, user: &str) -> Result<Option<Vec<u8>>, Fault> {
        let resp = self
            .req(
                reqwest::Method::GET,
                &format!("files?path={}&username={user}", q(path)),
                user,
            )
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(net)?;
        let s = resp.status();
        if s.as_u16() == 404 {
            let body = resp.text().await.unwrap_or_default();
            // A missing file is a 404 from envd itself; a missing sandbox is
            // the proxy's 404 naming the sandbox.
            if body.contains("sandbox") {
                return Err(Fault::Gone(body));
            }
            return Ok(None);
        }
        if !s.is_success() {
            return Err(fault(s, &resp.text().await.unwrap_or_default()));
        }
        resp.bytes()
            .await
            .map(|b| Some(b.to_vec()))
            .map_err(|e| Fault::Gone(format!("the download broke: {e}")))
    }

    /// Starts `argv` in `cwd` as `user`, streaming its output.
    pub async fn start(
        &self,
        user: &str,
        argv: &[String],
        cwd: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<Proc, Fault> {
        let mut process = json!({"cmd": argv[0], "args": &argv[1..], "envs": env});
        if !cwd.is_empty() {
            process["cwd"] = json!(cwd);
        }
        let resp = self
            .req(reqwest::Method::POST, "process.Process/Start", user)
            .header("content-type", "application/connect+json")
            .header("connect-protocol-version", "1")
            .body(envelope(&json!({"process": process})))
            .send()
            .await
            .map_err(net)?;
        if !resp.status().is_success() {
            let s = resp.status();
            return Err(fault(s, &resp.text().await.unwrap_or_default()));
        }
        Ok(Proc {
            resp,
            frames: Frames::default(),
            pending: Default::default(),
            bytes: 0,
        })
    }

    /// Runs `argv` to its end and answers its exit code and output.
    pub async fn run(
        &self,
        user: &str,
        argv: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>), Fault> {
        let mut p = self.start(user, argv, "", env).await?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            match p.next().await? {
                Some(Event::Stdout(b)) => out.extend(b),
                Some(Event::Stderr(b)) => err.extend(b),
                Some(Event::End(c)) => return Ok((c, out, err)),
                Some(Event::Error(m)) => return Err(Fault::Other(m)),
                Some(Event::Start(_)) => {}
                None => return Err(Fault::Stream("the guest closed the stream".into())),
            }
        }
    }

    /// Sends a signal (`SIGTERM`, `SIGKILL`) to a started process.
    pub async fn signal(&self, pid: u64, signal: &str) -> Result<(), Fault> {
        let resp = self
            .req(reqwest::Method::POST, "process.Process/SendSignal", "root")
            .header("content-type", "application/json")
            .header("connect-protocol-version", "1")
            .json(&json!({"process": {"pid": pid}, "signal": format!("SIGNAL_{signal}")}))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(net)?;
        if !resp.status().is_success() {
            let s = resp.status();
            return Err(fault(s, &resp.text().await.unwrap_or_default()));
        }
        Ok(())
    }

    /// A cheap call that counts as activity, so a long silent command does
    /// not end the lease IDLE.
    pub async fn touch(&self, path: &str) -> Result<(), Fault> {
        let resp = self
            .req(reqwest::Method::POST, "filesystem.Filesystem/Stat", "user")
            .header("content-type", "application/json")
            .header("connect-protocol-version", "1")
            .json(&json!({"path": path}))
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(net)?;
        if resp.status().is_server_error() || resp.status().as_u16() == 410 {
            let s = resp.status();
            return Err(fault(s, &resp.text().await.unwrap_or_default()));
        }
        Ok(())
    }
}

/// A random-enough number for multipart boundaries and run ids.
pub fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(flags: u8, v: Value) -> Vec<u8> {
        let mut f = envelope(&v);
        f[0] = flags;
        f
    }

    #[test]
    fn frames_split_anywhere_reassemble() {
        let mut stream = Vec::new();
        stream.extend(frame(0, json!({"event": {"start": {"pid": 42}}})));
        stream.extend(frame(
            0,
            json!({"event": {"data": {"stdout": B64.encode("hello\n")}}}),
        ));
        stream.extend(frame(
            0,
            json!({"event": {"data": {"stderr": B64.encode("warn\n")}}}),
        ));
        stream.extend(frame(0, json!({"event": {"end": {"exitCode": 101, "exited": true, "status": "exit status 101"}}})));
        stream.extend(frame(2, json!({})));
        let want = vec![
            Event::Start(42),
            Event::Stdout(b"hello\n".to_vec()),
            Event::Stderr(b"warn\n".to_vec()),
            Event::End(101),
        ];
        for cut in [1usize, 3, 5, 7, 64, stream.len()] {
            let mut f = Frames::default();
            let mut got = Vec::new();
            for piece in stream.chunks(cut) {
                got.extend(f.push(piece));
            }
            assert_eq!(got, want, "cut every {cut} bytes");
        }
    }

    #[test]
    fn end_events_map_to_shell_exit_codes() {
        let cases = [
            (json!({"exited": true, "status": "exit status 0"}), 0),
            (
                json!({"exitCode": 3, "exited": true, "status": "exit status 3"}),
                3,
            ),
            (
                json!({"exitCode": -1, "exited": false, "status": "signal: killed"}),
                137,
            ),
            (json!({"exitCode": -1, "status": "signal: terminated"}), 143),
            (
                json!({"exitCode": -1, "status": "signal: segmentation fault"}),
                139,
            ),
            (json!({"exited": false}), 128),
        ];
        for (end, want) in cases {
            assert_eq!(exit_code(&end), want, "{end}");
        }
    }

    #[test]
    fn a_stream_error_is_reported() {
        let mut f = Frames::default();
        let got = f.push(&frame(
            2,
            json!({"error": {"code": "not_found", "message": "no such file: cargo"}}),
        ));
        assert_eq!(got, vec![Event::Error("no such file: cargo".into())]);
    }

    #[test]
    fn multipart_has_one_file_part() {
        let body = multipart("B", b"abc");
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("--B\r\nContent-Disposition: form-data; name=\"file\""));
        assert!(text.contains("\r\n\r\nabc\r\n--B--\r\n"));
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(
            q("/workspace/.sylphx/in 1&x"),
            "/workspace/.sylphx/in%201%26x"
        );
    }
    /// A server that sends part of a chunked body and hangs up, as a gateway
    /// that drops a long stream does (`error decoding response body`).
    #[tokio::test]
    async fn a_dropped_process_stream_is_a_stream_fault() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut c, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf).await;
            let _ = c
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/connect+json\r\ntransfer-encoding: chunked\r\n\r\n10\r\nonly-part")
                .await;
            // Closes in the middle of a chunk.
        });
        let g = Guest::new(&format!("http://{addr}"), "sbx", "tok").unwrap();
        let mut p = g
            .start("user", &["true".to_string()], "", &BTreeMap::new())
            .await
            .unwrap();
        let got = p.next().await;
        assert!(
            matches!(got, Err(Fault::Stream(ref m)) if m.contains("stream broke")),
            "{got:?}"
        );
    }
}
