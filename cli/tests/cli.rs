//! The `sylphx` binary against a local fake API: generated commands build the
//! wire request (ids, parents from the linked project, flags, update masks,
//! etags), wait for Operations, refuse unconfirmed destructive calls, and
//! `sylphx mcp` speaks MCP over stdio.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ENV: &str = "orgs/org_a/projects/prj_a/envs/env_a";

#[derive(Debug, Clone)]
struct Seen {
    line: String,
    head: String,
    body: String,
}

type Log = Arc<Mutex<Vec<Seen>>>;

async fn serve(replies: Vec<(u16, Value)>) -> (String, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        for (status, body) in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body_in) = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let head = text[..i].to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    while buf.len() < i + 4 + len {
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    break (
                        head,
                        String::from_utf8_lossy(&buf[i + 4..i + 4 + len]).to_string(),
                    );
                }
            };
            let line = head.lines().next().unwrap_or_default().to_string();
            seen.lock().unwrap().push(Seen {
                line,
                head,
                body: body_in,
            });
            let payload = body.to_string();
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        }
    });
    (url, log)
}

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sylphx-cli-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".sylphx")).unwrap();
        std::fs::write(
            dir.join(".sylphx/project.json"),
            json!({"org": "orgs/org_a", "project": "orgs/org_a/projects/prj_a", "env": ENV})
                .to_string(),
        )
        .unwrap();
        Self { dir }
    }

    fn cmd(&self, url: &str, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_sylphx"));
        c.args(args)
            .current_dir(&self.dir)
            .env("SYLPHX_BASE_URL", url)
            .env("SYLPHX_API_KEY", "sylphx_sk_test")
            .env("SYLPHX_CONFIG_DIR", self.dir.join("config"))
            .stdin(Stdio::null());
        c
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn run(sb: &Sandbox, url: &str, args: &[&str]) -> std::process::Output {
    let mut c = sb.cmd(url, args);
    tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn create_takes_the_id_and_the_linked_env_and_waits() {
    let db = json!({"name": format!("{ENV}/databases/main"), "spec": {"compute_units": 2}});
    let (url, log) = serve(vec![
        (200, json!({"name": format!("{ENV}/operations/op_1"), "done": false})),
        (200, json!({"name": format!("{ENV}/operations/op_1"), "done": true,
                     "response": {"@type": "type.googleapis.com/sylphx.data.v1.Database", "name": format!("{ENV}/databases/main"), "spec": {"compute_units": 2}}})),
    ])
    .await;
    let sb = Sandbox::new("create");
    let out = run(
        &sb,
        &url,
        &[
            "data",
            "databases",
            "create",
            "main",
            "--spec.compute-units",
            "2",
            "-o",
            "json",
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed, db);
    let log = log.lock().unwrap().clone();
    assert!(
        log[0]
            .line
            .starts_with(&format!("POST /v1/{ENV}/databases?database_id=main ")),
        "{}",
        log[0].line
    );
    assert_eq!(
        serde_json::from_str::<Value>(&log[0].body).unwrap(),
        json!({"spec": {"compute_units": 2}})
    );
    assert!(log[0]
        .head
        .to_ascii_lowercase()
        .contains("idempotency-key:"));
    assert!(log[1]
        .line
        .starts_with(&format!("POST /v1/{ENV}/operations/op_1:wait?timeout=60s ")));
}

#[tokio::test(flavor = "multi_thread")]
async fn update_reads_the_etag_and_sends_a_mask_of_the_set_flags() {
    let key = format!("{ENV}/api_keys/key_a");
    let (url, log) = serve(vec![
        (200, json!({"name": key, "meta": {"etag": "\"k3\""}})),
        (200, json!({"name": key, "spec": {"label": "ci"}})),
    ])
    .await;
    let sb = Sandbox::new("update");
    let out = run(
        &sb,
        &url,
        &[
            "access",
            "api-keys",
            "update",
            "key_a",
            "--spec.label",
            "ci",
            "-o",
            "name",
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), key);
    let log = log.lock().unwrap().clone();
    assert!(log[0].line.starts_with(&format!("GET /v1/{key} ")));
    assert!(
        log[1]
            .line
            .starts_with(&format!("PATCH /v1/{key}?update_mask=spec.label ")),
        "{}",
        log[1].line
    );
    assert_eq!(
        serde_json::from_str::<Value>(&log[1].body).unwrap(),
        json!({"name": key, "meta": {"etag": "\"k3\""}, "spec": {"label": "ci"}})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_destructive_call_needs_yes_without_a_terminal() {
    let (url, log) = serve(vec![(
        200,
        json!({"name": format!("{ENV}/api_keys/key_a"), "meta": {"etag": "\"e\""}}),
    )])
    .await;
    let sb = Sandbox::new("destructive");
    let out = run(&sb, &url, &["access", "api-keys", "revoke", "key_a"]).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--yes"));
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|s| !s.line.starts_with("POST")),
        "nothing was revoked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_api_error_prints_the_problem_and_exits_1() {
    let (url, _log) = serve(vec![(
        403,
        json!({"code": "PERMISSION_DENIED", "status": 403, "detail": "missing scope data:read", "retryable": false, "instance": "req_9"}),
    )])
    .await;
    let sb = Sandbox::new("error");
    let out = run(&sb, &url, &["data", "databases", "list"]).await;
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("PERMISSION_DENIED (403): missing scope data:read"),
        "{err}"
    );
    assert!(err.contains("req_9"));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_defaults_to_the_linked_env_and_prints_a_table() {
    let (url, log) = serve(vec![(
        200,
        json!({"databases": [{"name": format!("{ENV}/databases/main"), "status": {"conditions": [{"type": "Ready", "status": "TRUE"}]}}]}),
    )])
    .await;
    let sb = Sandbox::new("list");
    let out = run(
        &sb,
        &url,
        &["data", "databases", "list", "--page-size", "10"],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("NAME"), "{text}");
    assert!(text.contains("databases/main  true"));
    assert!(log.lock().unwrap()[0]
        .line
        .starts_with(&format!("GET /v1/{ENV}/databases?page_size=10 ")));
}

#[tokio::test(flavor = "multi_thread")]
async fn login_with_an_empty_stdin_starts_the_device_flow() {
    let (url, log) = serve(vec![
        (
            200,
            json!({"device_code": "dc", "user_code": "BCDF-GHJK", "verification_uri": "https://sylphx.com/device",
                   "verification_uri_complete": "https://sylphx.com/device?user_code=BCDF-GHJK",
                   "interval": 1, "expires_in": 600}),
        ),
        (400, json!({"error": "access_denied"})),
    ])
    .await;
    let sb = Sandbox::new("login-stdin");
    let mut c = sb.cmd(&url, &["login", "--org", "acme"]);
    c.env_remove("SYLPHX_API_KEY");
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        err.contains("https://sylphx.com/device?user_code=BCDF-GHJK"),
        "{err}"
    );
    assert!(err.contains("denied"), "{err}");
    let seen = log.lock().unwrap();
    assert!(seen[0]
        .line
        .starts_with("POST /v1/access/device/authorize "));
    assert!(
        seen[0].body.contains("\"org\":\"acme\""),
        "{}",
        seen[0].body
    );
    assert!(seen[1].line.starts_with("POST /v1/access/device/token "));
}

#[test]
fn mcp_answers_over_stdio() {
    let sb = Sandbox::new("mcp");
    let mut child = sb
        .cmd("http://127.0.0.1:9", &["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}})).unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        )
        .unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "sylphx");
    assert!(lines[1]["result"]["tools"].as_array().unwrap().len() >= 4);
}
