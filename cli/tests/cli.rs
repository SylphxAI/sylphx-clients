//! The `sylphx` binary against a local fake API: generated commands build the
//! wire request (ids, parents from the linked project, flags, update masks,
//! etags), wait for Operations, refuse unconfirmed destructive calls, and
//! `sylphx mcp` speaks MCP over stdio, and `sylphx build run` drives a lease
//! and its guest end to end.

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
    serve_bytes(
        replies
            .into_iter()
            .map(|(s, v)| (s, "application/json", v.to_string().into_bytes()))
            .collect(),
    )
    .await
}

/// Like [`serve`], with each reply's content type and raw body.
async fn serve_bytes(replies: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Log) {
    serve_bytes_on(TcpListener::bind("127.0.0.1:0").await.unwrap(), replies).await
}

async fn serve_bytes_on(
    listener: TcpListener,
    replies: Vec<(u16, &'static str, Vec<u8>)>,
) -> (String, Log) {
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        for (status, content_type, body) in replies {
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
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body).await.unwrap();
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
            .env("SYLPHX_BUILD_CACHE_URL", "http://127.0.0.1:9")
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

#[tokio::test(flavor = "multi_thread")]
async fn login_keeps_polling_through_a_transient_server_error() {
    let (url, log) = serve(vec![
        (
            200,
            json!({"device_code": "dc", "user_code": "BCDF-GHJK", "verification_uri": "https://sylphx.com/device",
                   "interval": 1, "expires_in": 600}),
        ),
        (503, json!({"code": "UNAVAILABLE"})),
        (
            200,
            json!({"api_key": "sylphx_sk_test_issued", "org": {"slug": "acme"}, "key": {"name": "orgs/org_a/api_keys/key_cli"}, "active_after_ms": 300}),
        ),
    ])
    .await;
    let sb = Sandbox::new("login-503");
    let mut c = sb.cmd(&url, &["login", "--org", "acme", "--no-verify"]);
    c.env_remove("SYLPHX_API_KEY");
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("Approved in acme"), "{err}");
    let seen = log.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen[1].line.starts_with("POST /v1/access/device/token "));
    assert!(seen[2].line.starts_with("POST /v1/access/device/token "));
}

fn device_replies() -> Vec<(u16, Value)> {
    vec![
        (
            200,
            json!({"device_code": "dc", "user_code": "BCDF-GHJK", "verification_uri": "https://sylphx.com/device",
                   "interval": 1, "expires_in": 600}),
        ),
        (
            200,
            json!({"api_key": "sylphx_sk_test_issued", "org": {"slug": "acme"}, "key": {"name": "orgs/org_a/api_keys/key_cli"}, "active_after_ms": 300}),
        ),
    ]
}

fn not_yet() -> (u16, Value) {
    (
        401,
        json!({"ok": false, "error": "key_not_yet_propagated", "code": "key_not_yet_propagated", "status": 401}),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn login_waits_for_a_fresh_key_to_propagate() {
    let mut replies = device_replies();
    // Not yet propagated, then (window passed, replica not refreshed)
    // unknown_key, then verified.
    replies.extend([
        not_yet(),
        (
            401,
            json!({"ok": false, "error": "unknown_key", "code": "unknown_key", "status": 401}),
        ),
        (200, json!({"principal": "principal_kyle"})),
    ]);
    let (url, log) = serve(replies).await;
    let sb = Sandbox::new("login-propagate");
    let mut c = sb.cmd(&url, &["login", "--org", "acme"]);
    c.env_remove("SYLPHX_API_KEY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS");
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("Signed in as principal_kyle"), "{stdout}");
    assert_eq!(log.lock().unwrap().len(), 5);
    let creds = std::fs::read_to_string(sb.dir.join("config/credentials.json")).unwrap_or_default();
    assert!(creds.contains("sylphx_sk_test_issued"), "{creds}");
}

#[tokio::test(flavor = "multi_thread")]
async fn login_keeps_an_issued_key_it_cannot_confirm_yet() {
    let mut replies = device_replies();
    replies.extend(std::iter::repeat_with(not_yet).take(6));
    let (url, _log) = serve(replies).await;
    let sb = Sandbox::new("login-unconfirmed");
    let mut c = sb.cmd(&url, &["login", "--org", "acme"]);
    c.env_remove("SYLPHX_API_KEY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS");
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("no need to sign in again"), "{err}");
    let creds = std::fs::read_to_string(sb.dir.join("config/credentials.json")).unwrap_or_default();
    assert!(creds.contains("sylphx_sk_test_issued"), "{creds}");
}

/// No keychain and no directory of its own: the shared default is refused,
/// before any key is read or any device grant is started.
#[tokio::test(flavor = "multi_thread")]
async fn login_refuses_the_shared_default_store_without_a_keychain() {
    let (url, log) = serve(vec![]).await;
    let sb = Sandbox::new("login-shared-default");
    let home = sb.dir.join("home");
    let shared = home.join(".config/sylphx/credentials.json");
    for args in [
        vec!["login", "--api-key", "sylphx_sk_test_given", "--no-verify"],
        vec!["login", "--org", "acme"],
    ] {
        let mut c = sb.cmd(&url, &args);
        c.env_remove("SYLPHX_API_KEY")
            .env_remove("SYLPHX_CONFIG_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env("HOME", &home)
            .env("SYLPHX_NO_KEYCHAIN", "1");
        let out = tokio::task::spawn_blocking(move || c.output().unwrap())
            .await
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
        assert!(
            err.contains("--store-file") && err.contains("SYLPHX_CONFIG_DIR"),
            "{err}"
        );
    }
    assert!(!shared.exists(), "nothing written to the shared default");
    assert!(log.lock().unwrap().is_empty(), "no device grant started");
}

/// `--store-file` keeps the key in the default directory; `SYLPHX_CONFIG_DIR`
/// keeps it in the chosen one.
#[tokio::test(flavor = "multi_thread")]
async fn login_stores_a_file_only_where_asked() {
    let (url, _log) = serve(vec![]).await;
    let sb = Sandbox::new("login-store-file");
    let home = sb.dir.join("home");
    let mut c = sb.cmd(
        &url,
        &[
            "login",
            "--api-key",
            "sylphx_sk_test_given",
            "--no-verify",
            "--store-file",
        ],
    );
    c.env_remove("SYLPHX_API_KEY")
        .env_remove("SYLPHX_CONFIG_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", &home)
        .env("SYLPHX_NO_KEYCHAIN", "1");
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let shared = std::fs::read_to_string(home.join(".config/sylphx/credentials.json")).unwrap();
    assert!(shared.contains("sylphx_sk_test_given"), "{shared}");

    let own = sb.dir.join("own-config");
    let mut c = sb.cmd(
        &url,
        &["login", "--api-key", "sylphx_sk_test_own", "--no-verify"],
    );
    c.env_remove("SYLPHX_API_KEY")
        .env("SYLPHX_CONFIG_DIR", &own)
        .env("SYLPHX_NO_KEYCHAIN", "1");
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stored = std::fs::read_to_string(own.join("credentials.json")).unwrap();
    assert!(stored.contains("sylphx_sk_test_own"), "{stored}");
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

fn token_reply(secret: &str) -> Value {
    json!({"name": format!("{ENV}/api_keys/key_child"), "spec": {"scopes": ["hosting:deploy"]},
           "status": {"secret": secret}, "active_after_ms": 3000})
}

fn whoami_reply() -> Value {
    json!({"principal": "principal_x", "api_key": format!("{ENV}/api_keys/key_self"),
           "org": "orgs/org_a", "project": "orgs/org_a/projects/prj_a", "env": ENV,
           "scopes": ["*:write"]})
}

fn civil(secs: u64) -> String {
    // Enough of RFC 3339 for the tests: the date part via the same algorithm.
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let r = secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        r / 3600,
        r % 3600 / 60,
        r % 60
    )
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test(flavor = "multi_thread")]
async fn token_prints_only_a_minted_key_scoped_to_one_scope() {
    let (url, log) = serve(vec![
        (200, whoami_reply()),
        (201, token_reply("sylphx_sk_child_1")),
    ])
    .await;
    let sb = Sandbox::new("token-mint");
    let out = run(&sb, &url, &["token", "--scope", "packages:read"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "sylphx_sk_child_1\n");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("sylphx_sk_child_1"));
    let log = log.lock().unwrap().clone();
    assert!(log[1]
        .line
        .starts_with(&format!("POST /v1/{ENV}/api_keys ")));
    let body: Value = serde_json::from_str(&log[1].body).unwrap();
    assert_eq!(body["spec"]["scopes"], json!(["packages:read"]));
    assert_eq!(body["spec"]["label"], "token:packages:read");
    let exp = body["spec"]["expire_time"].as_str().unwrap();
    assert!(
        exp.ends_with('Z') && exp > civil(now_secs()).as_str(),
        "{exp}"
    );
    // The login key is used to mint, never printed.
    assert!(log[1].head.contains("sylphx_sk_test"));
}

#[tokio::test(flavor = "multi_thread")]
async fn token_is_cached_until_shortly_before_it_expires() {
    let (url, log) = serve(vec![
        (200, whoami_reply()),
        (201, token_reply("sylphx_sk_child_1")),
    ])
    .await;
    let sb = Sandbox::new("token-cache");
    let first = run(&sb, &url, &["token", "--scope", "hosting:deploy"]).await;
    let second = run(&sb, &url, &["token", "--scope", "hosting:deploy"]).await;
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(
        log.lock().unwrap().len(),
        2,
        "the second call made no request"
    );
    let dir = sb.dir.join("config/token-cache");
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
    assert_eq!(files.len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = files[0].metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    // A different scope is a different token.
    let (url2, log2) = serve(vec![
        (200, whoami_reply()),
        (201, token_reply("sylphx_sk_child_ai")),
    ])
    .await;
    let other = run(&sb, &url2, &["token", "--scope", "ai:inference"]).await;
    assert_eq!(
        String::from_utf8_lossy(&other.stdout),
        "sylphx_sk_child_ai\n"
    );
    assert_eq!(log2.lock().unwrap().len(), 2);

    // Within 2 minutes of expiry the cache is not used.
    let path = dir
        .read_dir()
        .unwrap()
        .flatten()
        .find(|f| {
            std::fs::read_to_string(f.path())
                .unwrap()
                .contains("child_1")
        })
        .unwrap()
        .path();
    let soon = json!({"token": "sylphx_sk_child_1", "expires_at": now_secs() + 60});
    std::fs::write(&path, soon.to_string()).unwrap();
    let (url3, log3) = serve(vec![
        (200, whoami_reply()),
        (201, token_reply("sylphx_sk_child_2")),
    ])
    .await;
    let again = run(&sb, &url3, &["token", "--scope", "hosting:deploy"]).await;
    assert_eq!(
        String::from_utf8_lossy(&again.stdout),
        "sylphx_sk_child_2\n"
    );
    assert_eq!(log3.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn token_refuses_an_unregistered_scope() {
    let (url, _log) = serve(vec![
        (200, whoami_reply()),
        (400, json!({"code": "INVALID_FIELD", "status": 400, "retryable": false, "detail": "scopes: unregistered scope nope:nope"})),
    ])
    .await;
    let sb = Sandbox::new("token-unregistered");
    let out = run(&sb, &url, &["token", "--scope", "nope:nope"]).await;
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("nope:nope") && err.contains("not a registered scope"),
        "{err}"
    );
    // A malformed scope never reaches the network.
    let bad = run(
        &sb,
        "http://127.0.0.1:9",
        &["token", "--scope", "Not A Scope"],
    )
    .await;
    assert!(!bad.status.success() && bad.stdout.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn token_says_when_signed_out() {
    let sb = Sandbox::new("token-signed-out");
    let mut c = sb.cmd(
        "http://127.0.0.1:9",
        &["token", "--scope", "hosting:deploy"],
    );
    c.env_remove("SYLPHX_API_KEY")
        .env("SYLPHX_NO_KEYCHAIN", "1");
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not signed in"));
}

#[tokio::test(flavor = "multi_thread")]
async fn token_names_the_scope_a_login_may_not_grant() {
    let (url, _log) = serve(vec![
        (200, whoami_reply()),
        (403, json!({"code": "PERMISSION_DENIED", "status": 403, "retryable": false, "detail": "the caller cannot grant hosting:deploy"})),
    ])
    .await;
    let sb = Sandbox::new("token-denied");
    let out = run(&sb, &url, &["token", "--scope", "hosting:deploy"]).await;
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("hosting:deploy"), "{err}");
    assert_eq!(err.trim().lines().count(), 1, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn token_from_an_api_key_that_cannot_mint_prints_it_only_if_short_lived() {
    let key = format!("{ENV}/api_keys/key_self");
    let me = json!({"api_key": key, "org": "orgs/org_a", "project": "orgs/org_a/projects/prj_a",
                    "env": ENV, "scopes": ["hosting:deploy"]});
    let denied = || json!({"code": "PERMISSION_DENIED", "status": 403, "retryable": false, "detail": "missing access:keys:write"});
    // Expires in 30 minutes: an exchange key, handed out as it is.
    let (url, _l) = serve(vec![
        (200, me.clone()),
        (403, denied()),
        (
            200,
            json!({"name": key, "spec": {"expire_time": civil(now_secs() + 1800)}}),
        ),
    ])
    .await;
    let sb = Sandbox::new("token-ci-short");
    let out = run(&sb, &url, &["token", "--scope", "hosting:deploy"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "sylphx_sk_test\n");
    // Expires in a day: refused.
    let (url, _l) = serve(vec![
        (200, me.clone()),
        (403, denied()),
        (
            200,
            json!({"name": key, "spec": {"expire_time": civil(now_secs() + 86400)}}),
        ),
    ])
    .await;
    let sb = Sandbox::new("token-ci-long");
    let out = run(&sb, &url, &["token", "--scope", "hosting:deploy"]).await;
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    // Not carrying the scope: refused without reading the key.
    let (url, log) = serve(vec![(200, me), (403, denied())]).await;
    let sb = Sandbox::new("token-ci-other");
    let out = run(&sb, &url, &["token", "--scope", "ai:inference"]).await;
    assert!(!out.status.success() && out.stdout.is_empty());
    assert_eq!(log.lock().unwrap().len(), 2);
}

/// Enable Auth binds Sylphx Auth into the linked environment's identity slot
/// with the composition route's own request (`PUT .../composition/bindings`),
/// and prints the instance the answer names. The route's server side is
/// covered by the composition bind tests in `sylphx-apps-api`.
#[tokio::test(flavor = "multi_thread")]
async fn auth_enable_binds_the_linked_environment_and_prints_the_instance() {
    let (url, log) = serve(vec![(
        200,
        json!({"status": "active", "providerId": "sylphx-auth",
               "config": {"organizationId": "org-1"}}),
    )])
    .await;
    let sb = Sandbox::new("auth-enable");
    let out = run(&sb, &url, &["auth", "enable"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("org-1"), "{text}");
    let log = log.lock().unwrap().clone();
    assert!(
        log[0]
            .line
            .starts_with("PUT /v1/projects/prj_a/composition/bindings "),
        "{}",
        log[0].line
    );
    assert_eq!(
        serde_json::from_str::<Value>(&log[0].body).unwrap(),
        json!({"capabilityId": "identity", "slot": "identity",
               "providerId": "sylphx-auth", "environmentId": "env_a"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_status_reads_the_identity_bindings_back() {
    let (url, log) = serve(vec![(
        200,
        json!({"bindings": [
            {"slot": "identity", "environmentId": "e1", "providerId": "sylphx-auth",
             "status": "active", "config": {"organizationId": "org-1"}},
            {"slot": "email", "environmentId": "e1", "providerId": "x"}]}),
    )])
    .await;
    let sb = Sandbox::new("auth-status");
    let out = run(&sb, &url, &["auth", "status", "-o", "json"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed["enabled"], true, "{printed}");
    assert_eq!(printed["bindings"][0]["organizationId"], "org-1");
    assert!(log.lock().unwrap()[0]
        .line
        .starts_with("GET /v1/projects/prj_a/composition/bindings "));
}

#[tokio::test(flavor = "multi_thread")]
async fn workflows_schedules_read_the_served_compute_route() {
    let sched = json!({"id": "sched_1", "name": "nightly", "status": "active"});
    let (url, log) = serve(vec![
        (200, json!({"schedules": [sched.clone()], "limit": 50})),
        (200, sched),
    ])
    .await;
    let sb = Sandbox::new("schedules");
    let out = run(&sb, &url, &["workflows", "schedules", "list", "-o", "json"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("sched_1"));
    let out = run(
        &sb,
        &url,
        &["workflows", "schedules", "get", "sched_1", "-o", "json"],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = log.lock().unwrap();
    assert!(
        seen[0].line.starts_with("GET /v1/schedules "),
        "{}",
        seen[0].line
    );
    assert!(
        seen[1].line.starts_with("GET /v1/schedules/sched_1 "),
        "{}",
        seen[1].line
    );
}

/// One Connect envelope of the envd process stream.
fn envd_frame(flags: u8, v: Value) -> Vec<u8> {
    let body = serde_json::to_vec(&v).unwrap();
    let mut out = vec![flags];
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// A finished process: optional stdout, then its exit code.
fn envd_process(stdout: &str, code: i64) -> (u16, &'static str, Vec<u8>) {
    use base64::Engine;
    let mut b = envd_frame(0, json!({"event": {"start": {"pid": 7}}}));
    if !stdout.is_empty() {
        let data = base64::engine::general_purpose::STANDARD.encode(stdout);
        b.extend(envd_frame(0, json!({"event": {"data": {"stdout": data}}})));
    }
    b.extend(envd_frame(
        0,
        json!({"event": {"end": {"exitCode": code, "exited": true, "status": format!("exit status {code}")}}}),
    ));
    b.extend(envd_frame(2, json!({})));
    (200, "application/connect+json", b)
}

/// A process that wrote `stderr` and ended with `code`.
fn envd_process_err(stderr: &str, code: i64) -> (u16, &'static str, Vec<u8>) {
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(stderr);
    let mut b = envd_frame(0, json!({"event": {"start": {"pid": 7}}}));
    b.extend(envd_frame(0, json!({"event": {"data": {"stderr": data}}})));
    b.extend(envd_frame(
        0,
        json!({"event": {"end": {"exitCode": code, "exited": true, "status": format!("exit status {code}")}}}),
    ));
    b.extend(envd_frame(2, json!({})));
    (200, "application/connect+json", b)
}

/// A process stream that stops before its end event, as a dropped gateway
/// connection looks to the client.
fn envd_broken() -> (u16, &'static str, Vec<u8>) {
    let b = envd_frame(0, json!({"event": {"start": {"pid": 7}}}));
    (200, "application/connect+json", b)
}

fn git_init(dir: &std::path::Path) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success();
    assert!(ok);
}

/// The whole run against a stub API and guest: a new workspace from an empty
/// pool, a `build-large` lease with it at /workspace on the package-host
/// allow-list, a full sync (cold), the command's output on stdout and its own
/// exit code, and the lease released at the end.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_syncs_runs_and_returns_the_commands_exit_code() {
    let sb = Sandbox::new("build-run");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("main.rs"), "fn main() {}\n").unwrap();
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let lease = format!("{ENV}/leases/l1");
    // The guest is the same stub: its address is the lease's guest_uri.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let replies = vec![
        json_reply(json!({"volumes": []})),
        json_reply(
            json!({"name": format!("{ENV}/volumes/vol-1"), "status": {"state": "available"}}),
        ),
        json_reply(
            json!({"name": lease, "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}}),
        ),
        json_reply(json!({"token": "h.p.s", "generation": 1})),
        envd_process("", 0),
        (404, "text/plain", b"file not found".to_vec()),
        json_reply(json!({})),
        json_reply(json!({})),
        envd_process("", 0),
        envd_process("", 0),
        envd_process("hello from the build machine\n", 3),
        json_reply(json!({"name": lease, "status": {"state": "ending"}})),
    ];
    let (url2, log) = serve_bytes_at(&url, replies).await;
    assert_eq!(url, url2);
    let out = run(
        &sb,
        &url,
        &["build", "run", "--no-cache", "--", "cargo", "test"],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from the build machine\n"
    );
    assert!(
        err.contains("sylphx: synced 2 files") || err.contains("sylphx: synced 1 files"),
        "{err}"
    );
    assert!(err.contains("cold workspace"), "{err}");
    assert!(err.contains("sylphx: exit 3 in"), "{err}");

    let seen = log.lock().unwrap();
    let lines: Vec<&str> = seen.iter().map(|s| s.line.as_str()).collect();
    assert!(
        lines[0].starts_with(&format!("GET /v1/{ENV}/volumes")),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with(&format!("POST /v1/{ENV}/volumes")),
        "{lines:?}"
    );
    let vol: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(vol["meta"]["labels"]["purpose"], "build-workspace");
    assert_eq!(vol["spec"]["storageClass"], "local");
    let created: Value = serde_json::from_str(&seen[2].body).unwrap();
    assert_eq!(created["spec"]["shape"], "build-large");
    assert_eq!(created["spec"]["image"], "template:build");
    assert_eq!(
        created["spec"]["volumes"][0]["mount_path"]
            .as_str()
            .or(created["spec"]["volumes"][0]["mountPath"].as_str()),
        Some("/workspace")
    );
    let net = &created["spec"]["network"];
    assert!(
        net["egress"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case("allowlist"),
        "{net}"
    );
    assert!(net.to_string().contains("static.crates.io"), "{net}");
    // The guest calls carry the sandbox id and the lease token, never the key.
    let guest: Vec<&Seen> = seen.iter().skip(4).take(7).collect();
    for g in &guest {
        let head = g.head.to_ascii_lowercase();
        assert!(head.contains("e2b-sandbox-id: abcd1234"), "{}", g.head);
        assert!(head.contains("x-access-token: h.p.s"), "{}", g.head);
        assert!(!head.contains("sylphx_sk_test"), "{}", g.head);
    }
    assert!(
        lines[4].starts_with("POST /process.Process/Start"),
        "{lines:?}"
    );
    assert!(
        lines[5].starts_with("GET /files?path=/workspace/.sylphx/manifest.gz"),
        "{lines:?}"
    );
    assert!(
        lines[6].starts_with("POST /files?path=/workspace/.sylphx/in-0000.tar.gz"),
        "{lines:?}"
    );
    assert!(
        lines[7].starts_with("POST /files?path=/workspace/.sylphx/add"),
        "{lines:?}"
    );
    assert!(
        seen[10].body.contains("cargo"),
        "the command is started: {}",
        seen[10].body
    );
    // On a Volume: the run keeps its local sccache (flag 0), as before.
    assert!(
        seen[10].body.contains(r#""/workspace",".","0","cargo""#),
        "{}",
        seen[10].body
    );
    assert!(
        lines[11].contains(":release"),
        "the lease is always released: {lines:?}"
    );
}

/// `-o json` writes NDJSON events, ending with the `result` event.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_json_reports_a_platform_failure_as_125() {
    let sb = Sandbox::new("build-run-json");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let (url, _log) = serve(vec![(
        403,
        json!({"code": "PERMISSION_DENIED", "status": 403, "detail": "missing scope sandboxes:read", "retryable": false}),
    )])
    .await;
    let out = run(&sb, &url, &["build", "run", "-o", "json", "--", "true"]).await;
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["exit_code"], 125);
    assert_eq!(last["outcome"], "platform_error");
    assert_eq!(
        last["retryable"], false,
        "a refused key is not worth a retry"
    );
    assert!(
        last["error"].as_str().unwrap().contains("sandboxes:read"),
        "{last}"
    );
    assert_eq!(last["workspace"], "cold");
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_usage_errors_exit_2_and_dry_run_creates_nothing() {
    let sb = Sandbox::new("build-run-usage");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "abc\n").unwrap();
    // No server: nothing may be called.
    let url = "http://127.0.0.1:9";
    let out = run(&sb, url, &["build", "run", "--size", "huge", "--", "true"]).await;
    assert_eq!(out.status.code(), Some(2));
    let out = run(
        &sb,
        url,
        &["build", "run", "--dry-run", "-o", "json", "--", "true"],
    )
    .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ev: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(ev["type"], "sync");
    assert!(ev["files"].as_u64().unwrap() >= 1, "{ev}");
}

/// `--region gra`: a new workspace is created in the region and labelled
/// with it, and the lease names the region and mounts that workspace (the
/// pool filter itself is unit-tested in `build_run`).
#[tokio::test(flavor = "multi_thread")]
async fn build_run_region_routes_the_lease_and_its_workspace() {
    let sb = Sandbox::new("build-run-region");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let mut replies = vec![
        // An available workspace of another pool.
        (
            200,
            json!({"volumes": [{"name": format!("{ENV}/volumes/home-1"),
                "meta": {"labels": {"purpose": "build-workspace", "build-repo": "any"}},
                "status": {"state": "available"}}]}),
        ),
        (
            200,
            json!({"name": format!("{ENV}/volumes/vol-gra"), "status": {"state": "available"}}),
        ),
    ];
    // NO_CAPACITY is retryable: the SDK retries it itself before the run
    // gives up, so the stub answers it every time.
    let no_capacity = json!({"code": "NO_CAPACITY", "status": 503, "detail": "No machine can be granted in region `gra` now; retry.", "retryable": true});
    replies.extend(std::iter::repeat_n((503, no_capacity), 8));
    let (url, log) = serve(replies).await;
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "-o",
            "json",
            "--region",
            "gra",
            "--queue-timeout",
            "120s",
            "--",
            "true",
        ],
    )
    .await;
    let seen = log.lock().unwrap();
    let lines: Vec<&str> = seen.iter().map(|s| s.line.as_str()).collect();
    assert!(
        lines[1].starts_with(&format!("POST /v1/{ENV}/volumes")),
        "{lines:?}"
    );
    let vol: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(vol["spec"]["region"], "gra");
    assert_eq!(vol["meta"]["labels"]["build-region"], "gra");
    let lease: Value = serde_json::from_str(&seen[2].body).unwrap();
    assert_eq!(lease["spec"]["region"], "gra");
    let mounted = lease["spec"]["volumes"][0]["volume"].as_str().unwrap();
    assert!(mounted.ends_with("/volumes/vol-gra"), "{lease}");
    // No capacity in the region: the platform exit code, retryable, which the
    // caller can turn into a run in the home region.
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["exit_code"], 125);
    assert_eq!(last["retryable"], true, "{last}");
    assert!(last["error"].as_str().unwrap().contains("gra"), "{last}");
}

/// No --region: the request names no region and the pool is the home pool,
/// exactly as before regions existed.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_without_region_stays_in_the_home_region() {
    let sb = Sandbox::new("build-run-home");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let (url, log) = serve(vec![
        // An available gra workspace of another pool.
        (
            200,
            json!({"volumes": [{"name": format!("{ENV}/volumes/gra-1"),
                "meta": {"labels": {"purpose": "build-workspace", "build-repo": "any", "build-region": "gra"}},
                "status": {"state": "available"}}]}),
        ),
        (
            200,
            json!({"name": format!("{ENV}/volumes/vol-home"), "status": {"state": "available"}}),
        ),
        (
            400,
            json!({"code": "SHAPE_NOT_OFFERED", "status": 400, "detail": "shape `build-large` is not offered", "retryable": false}),
        ),
    ])
    .await;
    let out = run(&sb, &url, &["build", "run", "-o", "json", "--", "true"]).await;
    assert_eq!(out.status.code(), Some(125));
    let seen = log.lock().unwrap();
    let vol: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert!(vol["spec"].get("region").is_none(), "{vol}");
    assert!(vol["meta"]["labels"].get("build-region").is_none(), "{vol}");
    let lease: Value = serde_json::from_str(&seen[2].body).unwrap();
    assert!(
        lease["spec"]
            .get("region")
            .is_none_or(|r| r.as_str() == Some("")),
        "{lease}"
    );
    assert!(
        lease["spec"]["volumes"][0]["volume"]
            .as_str()
            .unwrap()
            .ends_with("/volumes/vol-home"),
        "{lease}"
    );
}

/// A region with no Cell (or not offered yet) refuses the workspace and then
/// the lease at once: still 125, not retryable, so the wrapper falls back at
/// once, and nothing was created.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_region_not_offered_exits_125() {
    let sb = Sandbox::new("build-run-no-region");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let no_cell = json!({"code": "SHAPE_NOT_OFFERED", "status": 422, "detail": "`spec.region`: region `gra` has no Sandboxes Cell", "retryable": false});
    let (url, log) = serve(vec![
        (200, json!({"volumes": []})),
        (422, no_cell.clone()),
        (
            422,
            json!({"code": "SHAPE_NOT_OFFERED", "status": 422, "detail": "region `gra` has no Sandboxes Cell", "retryable": false}),
        ),
    ])
    .await;
    let out = run(
        &sb,
        &url,
        &[
            "build", "run", "-o", "json", "--region", "gra", "--", "true",
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["retryable"], false, "{last}");
    assert!(
        last["error"]
            .as_str()
            .unwrap()
            .contains("no Sandboxes Cell"),
        "{last}"
    );
    let seen = log.lock().unwrap();
    assert_eq!(seen.len(), 3, "no retry, no wait");
    assert!(seen[2].line.starts_with(&format!("POST /v1/{ENV}/leases")));
}

/// A region whose Cell offers no Volumes (gra while its admission binds no
/// claim): the workspace is refused at once with SHAPE_NOT_OFFERED, so the
/// run leases the machine in the region with no Volume, syncs the whole tree
/// to the machine's own disk, runs, and releases. No Volume is created, no
/// queue-timeout is waited out, and the command's exit code comes back.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_in_a_region_without_volumes_builds_on_the_machines_disk() {
    let sb = Sandbox::new("build-run-no-volumes");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("main.rs"), "fn main() {}\n").unwrap();
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let lease = format!("{ENV}/leases/l1");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let no_volumes = json!({"code": "SHAPE_NOT_OFFERED", "status": 422, "detail": "`spec.region`: region `gra` offers no Volumes", "retryable": false});
    let replies = vec![
        json_reply(json!({"volumes": []})),
        (422, "application/json", no_volumes.to_string().into_bytes()),
        json_reply(
            json!({"name": lease, "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}}),
        ),
        json_reply(json!({"token": "h.p.s", "generation": 1})),
        envd_process("", 0),
        (404, "text/plain", b"file not found".to_vec()),
        json_reply(json!({})),
        json_reply(json!({})),
        envd_process("", 0),
        envd_process("", 0),
        envd_process("built in gra\n", 0),
        json_reply(json!({"name": lease, "status": {"state": "ending"}})),
    ];
    let (url2, log) = serve_bytes_at(&url, replies).await;
    assert_eq!(url, url2);
    let t = std::time::Instant::now();
    let out = run(
        &sb,
        &url,
        &[
            "build", "run", "-o", "json", "--region", "gra", "--", "cargo", "check",
        ],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(
        t.elapsed() < std::time::Duration::from_secs(30),
        "no queue-timeout is waited out: {:?}",
        t.elapsed()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let running = events.iter().find(|e| e["type"] == "running").unwrap();
    assert_eq!(running["volume"], false, "{running}");
    assert_eq!(running["region"], "gra", "{running}");
    assert_eq!(running["workspace"], "cold", "{running}");
    let sync = events.iter().find(|e| e["type"] == "sync").unwrap();
    assert_eq!(sync["new_volume"], false, "{sync}");
    assert_eq!(sync["full"], true, "{sync}");
    let last = events.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["exit_code"], 0, "{last}");

    let seen = log.lock().unwrap();
    let lines: Vec<&str> = seen.iter().map(|s| s.line.as_str()).collect();
    // Exactly one workspace request, refused; no second one, no poll of it.
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with(&format!("POST /v1/{ENV}/volumes")))
            .count(),
        1,
        "{lines:?}"
    );
    let vol: Value = serde_json::from_str(&seen[1].body).unwrap();
    assert_eq!(vol["spec"]["region"], "gra");
    assert!(
        lines[2].starts_with(&format!("POST /v1/{ENV}/leases")),
        "{lines:?}"
    );
    let created: Value = serde_json::from_str(&seen[2].body).unwrap();
    assert_eq!(created["spec"]["region"], "gra", "{created}");
    assert_eq!(created["spec"]["shape"], "build-large", "{created}");
    assert!(
        created["spec"]
            .get("volumes")
            .is_none_or(|v| v.as_array().is_some_and(|a| a.is_empty())),
        "no Volume is mounted: {created}"
    );
    // The command runs with the no-volume flag (no local sccache).
    let args = &seen[10].body;
    assert!(args.contains("cargo"), "{args}");
    assert!(args.contains(r#""/workspace",".","1","cargo""#), "{args}");
    assert!(
        lines[11].contains(":release"),
        "the lease is always released: {lines:?}"
    );
}

/// The toolchain cannot be installed: nothing the user asked for has started,
/// so the run is a platform failure (125), never the status of a command that
/// did not run, and the machine is released.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_exits_125_when_the_toolchain_cannot_be_installed() {
    let sb = Sandbox::new("provision-fails");
    project(&sb.dir);
    let (url, log) = build_stub_with(
        envd_process_err(
            "the Rust toolchain could not be installed:\nerror: could not download channel-rust-1.99.0.toml.sha256 (Connection timed out)\n",
            4,
        ),
        vec![ended()],
    )
    .await;
    let out = run(
        &sb,
        &url,
        &["build", "run", "--no-cache", "--", "cargo", "build"],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{err}");
    assert!(err.contains("Connection timed out"), "{err}");
    assert!(err.contains("[retryable]"), "{err}");
    let seen = log.lock().unwrap();
    let lines: Vec<&str> = seen.iter().map(|s| s.line.as_str()).collect();
    assert_eq!(lines.len(), 11, "the command is never started: {lines:?}");
    assert!(lines[10].contains(":release"), "{lines:?}");
}

/// A broken output stream is retried once, on a new machine after a backoff;
/// when the retry runs, its own exit code is the run's.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_retries_a_broken_stream_once_and_keeps_the_commands_exit_code() {
    let sb = Sandbox::new("stream-retry");
    project(&sb.dir);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let lease_state = |state: &str| {
        (
            200u16,
            "application/json",
            json!({"name": format!("{ENV}/leases/l1"), "status": {"state": state}})
                .to_string()
                .into_bytes(),
        )
    };
    let mut replies = attempt_replies(&url, envd_process("", 0));
    replies.push(envd_broken());
    replies.push(lease_state("ready")); // the lease is looked at: still running
    replies.push(ended()); // released before the retry
    replies.extend(attempt_replies(&url, envd_process("", 0)));
    replies.push(envd_process("second try\n", 101));
    replies.push(ended());
    let (_, log) = serve_bytes_on(listener, replies).await;
    let t = std::time::Instant::now();
    let out = run(
        &sb,
        &url,
        &["build", "run", "--no-cache", "--", "cargo", "test"],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(101), "{err}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "second try\n");
    assert!(err.contains("retrying once"), "{err}");
    assert!(
        t.elapsed() >= std::time::Duration::from_secs(4),
        "a backoff: {:?}",
        t.elapsed()
    );
    let seen = log.lock().unwrap();
    let releases = seen.iter().filter(|s| s.line.contains(":release")).count();
    assert_eq!(
        releases, 2,
        "the first machine is released before the retry"
    );
}

/// A stream that breaks twice ends as 125, retryable.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_exits_125_when_the_stream_breaks_twice() {
    let sb = Sandbox::new("stream-twice");
    project(&sb.dir);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let running = || {
        (
            200u16,
            "application/json",
            json!({"name": format!("{ENV}/leases/l1"), "status": {"state": "ready"}})
                .to_string()
                .into_bytes(),
        )
    };
    let mut replies = Vec::new();
    for _ in 0..2 {
        replies.extend(attempt_replies(&url, envd_process("", 0)));
        replies.push(envd_broken());
        replies.push(running());
        replies.push(ended());
    }
    let (_, log) = serve_bytes_on(listener, replies).await;
    let out = run(
        &sb,
        &url,
        &["build", "run", "--no-cache", "--", "cargo", "test"],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{err}");
    assert!(err.contains("twice"), "{err}");
    assert!(err.contains("[retryable]"), "{err}");
    let seen = log.lock().unwrap();
    let starts = seen.iter().filter(|s| s.body.contains("\"cargo\"")).count();
    assert_eq!(
        starts, 2,
        "the command is started once per attempt, no more"
    );
}

/// A machine that stays queued past --queue-timeout: the lease is released
/// and the run answers 125, retryable, in about that long, not 30 minutes.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_queue_timeout_releases_and_exits_125() {
    let sb = Sandbox::new("build-run-queue-timeout");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let lease = format!("{ENV}/leases/l1");
    let queued = json!({"name": lease, "status": {"state": "granted"}});
    let (url, log) = serve(vec![
        (200, json!({"volumes": []})),
        (
            200,
            json!({"name": format!("{ENV}/volumes/vol-1"), "status": {"state": "available"}}),
        ),
        (200, queued.clone()),
        (200, queued.clone()),
        (200, queued.clone()),
        (200, queued.clone()),
        (200, queued),
    ])
    .await;
    let t = std::time::Instant::now();
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "-o",
            "json",
            "--region",
            "gra",
            "--queue-timeout",
            "3s",
            "--",
            "true",
        ],
    )
    .await;
    assert!(
        t.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["retryable"], true, "{last}");
    assert!(
        last["error"].as_str().unwrap().contains("--queue-timeout"),
        "{last}"
    );
    let seen = log.lock().unwrap();
    assert!(
        seen.iter().any(|s| s.line.contains(":release")),
        "the queued lease is released"
    );
}

/// [`serve_bytes`] on a known address (the guest's URI must be known before
/// the replies are built).
async fn serve_bytes_at(url: &str, replies: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Log) {
    let addr = url.trim_start_matches("http://").to_string();
    serve_bytes_on(TcpListener::bind(addr).await.unwrap(), replies).await
}

fn seat_fixture() -> Value {
    let w = |key: &str, secs: i64, util: f64| {
        json!({
            "window_key": key, "utilization": util, "used_percent": util * 100.0,
            "limit_window_seconds": secs, "reset_at": "2099-01-01T00:00:00.000Z",
            "observed_at": "2099-01-01T00:00:00.000Z", "limit_reached": util >= 1.0,
            "state": "AVAILABLE",
        })
    };
    json!({"object": "list", "data": [
        {"id": "seat-ok", "provider": "claude", "state": "active", "last_success_at": null,
         "quota_pressure": 0.4, "windows": [w("claude_5h", 18000, 0.1), w("claude_7d", 604800, 0.4)]},
        {"id": "seat-spent", "provider": "claude", "state": "active", "last_success_at": null,
         "quota_pressure": 1.0, "windows": [w("claude_5h", 18000, 1.0), w("claude_7d", 604800, 0.5)]},
        {"id": "seat-login", "provider": "claude", "state": "reauth_required", "last_success_at": null,
         "quota_pressure": null, "windows": []},
    ]})
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_top_once_reads_the_operator_seats_and_shows_red_lines() {
    let (url, log) = serve(vec![(200, seat_fixture())]).await;
    let sb = Sandbox::new("ai-top");
    let out = run(&sb, &url, &["ai", "top", "--once"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("1 usable / 1 spent / 1 out"), "{text}");
    assert!(
        text.contains("RED  seat seat-login (claude): login needed"),
        "{text}"
    );
    assert!(
        text.contains("SESSIONS AND SUBAGENTS") && text.contains("n/a"),
        "{text}"
    );
    assert!(!text.contains('\x1b'), "no colour off a terminal");
    let seen = log.lock().unwrap();
    assert!(
        seen[0].line.starts_with("GET /v1/operator/seats "),
        "{}",
        seen[0].line
    );
    // the reading is kept for the pace
    let kept = std::fs::read_to_string(sb.dir.join("config/ai-top-history.json")).unwrap();
    assert!(kept.contains("seat-ok"), "{kept}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_top_json_is_one_document_with_the_missing_fields_named() {
    let (url, _) = serve(vec![(200, seat_fixture())]).await;
    let sb = Sandbox::new("ai-top-json");
    let out = run(&sb, &url, &["ai", "top", "--json"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["seats"]["usable"], 1);
    assert_eq!(v["seats"]["spent"], 1);
    assert!(v["sessions"].is_null());
    assert!(v["red"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("seat-login")));
    assert!(!v["missing_api_fields"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_top_says_so_when_the_key_may_not_read_seats() {
    let (url, _) = serve(vec![(
        403,
        json!({"code": "missing_scope", "error": {"code": "missing_scope", "message": "needs ai:operator:seats:read"}}),
    )])
    .await;
    let sb = Sandbox::new("ai-top-403");
    let out = run(&sb, &url, &["ai", "top", "--once"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing_scope"));
}

// ---- the shared build cache ------------------------------------------------

const CACHE_TOKEN: &str = "cache-token-0123456789-do-not-print";

fn cache_reply() -> Value {
    json!({
        "token": CACHE_TOKEN,
        "expires_at": "2026-10-04T12:00:00Z",
        "org": "org_a",
        "cache": "prj_a",
        "read_scopes": ["protected", "dev"],
        "write_scope": "dev",
        "env": {
            "SCCACHE_WEBDAV_ENDPOINT": "https://cache.test/sccache",
            "SCCACHE_WEBDAV_TOKEN": CACHE_TOKEN,
            "TURBO_API": "https://cache.test",
            "TURBO_TOKEN": CACHE_TOKEN,
            "TURBO_TEAM": "team_prj_a",
            "SCCACHE_IGNORE_SERVER_IO_ERROR": "1"
        }
    })
}

async fn run_with(
    sb: &Sandbox,
    url: &str,
    args: &[&str],
    tweak: impl FnOnce(&mut Command),
) -> std::process::Output {
    let mut c = sb.cmd(url, args);
    tweak(&mut c);
    tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap()
}

/// A server that takes connections and never answers.
async fn hanging_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    url
}

/// The sandbox and guest stub of a run up to the start of the command: an
/// empty pool, a new workspace, a ready lease, its token, and the sync.
/// `tail` is what happens from the command on.
async fn build_stub(tail: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Log) {
    build_stub_with(envd_process("", 0), tail).await
}

/// The replies of one attempt up to the start of the command: an empty pool,
/// a new workspace, a ready lease, its token, the sync, then `provision`.
fn attempt_replies(
    url: &str,
    provision: (u16, &'static str, Vec<u8>),
) -> Vec<(u16, &'static str, Vec<u8>)> {
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let lease = format!("{ENV}/leases/l1");
    vec![
        json_reply(json!({"volumes": []})),
        json_reply(
            json!({"name": format!("{ENV}/volumes/vol-1"), "status": {"state": "available"}}),
        ),
        json_reply(
            json!({"name": lease, "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}}),
        ),
        json_reply(json!({"token": "h.p.s", "generation": 1})),
        envd_process("", 0),
        (404, "text/plain", b"file not found".to_vec()),
        json_reply(json!({})),
        json_reply(json!({})),
        envd_process("", 0),
        provision,
    ]
}

/// [`build_stub`] with the preparation step (the toolchain install) answering
/// `provision`.
async fn build_stub_with(
    provision: (u16, &'static str, Vec<u8>),
    tail: Vec<(u16, &'static str, Vec<u8>)>,
) -> (String, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut replies = attempt_replies(&url, provision);
    replies.extend(tail);
    serve_bytes_on(listener, replies).await
}

fn ended() -> (u16, &'static str, Vec<u8>) {
    (
        200,
        "application/json",
        json!({"name": format!("{ENV}/leases/l1"), "status": {"state": "ending"}})
            .to_string()
            .into_bytes(),
    )
}

/// The environment the command was started with (the 11th call).
fn started_env(log: &Log) -> Value {
    let seen = log.lock().unwrap();
    let start = &seen[10];
    assert!(start.line.starts_with("POST /process.Process/Start"));
    let text = &start.body;
    let from = text.find("{\"process\"").expect("a JSON message");
    let end = text.rfind('}').unwrap();
    let v: Value = serde_json::from_str(&text[from..=end]).unwrap();
    v["process"]["envs"].clone()
}

fn sylphx_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("sylphx:"))
        .collect()
}

fn project(dir: &std::path::Path) {
    git_init(dir);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_mints_a_write_token_and_gives_the_command_the_cache_env() {
    let sb = Sandbox::new("cache-run");
    project(&sb.dir);
    let (cache, clog) = serve(vec![(200, cache_reply())]).await;
    let (url, log) = build_stub(vec![envd_process("built\n", 0), ended()]).await;
    let out = run_with(
        &sb,
        &url,
        &[
            "build",
            "run",
            "-o",
            "json",
            "--timeout",
            "10m",
            "--env",
            "TURBO_TEAM=mine",
            "--",
            "make",
        ],
        |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{err}");
    // The mint: the key as a bearer, and the run's access, network and life.
    let seen = clog.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].line.starts_with("POST /v1/tokens"),
        "{}",
        seen[0].line
    );
    assert!(
        seen[0]
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer sylphx_sk_test"),
        "{}",
        seen[0].head
    );
    let body: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(
        body,
        json!({"project": "prj_a", "access": "write", "ttl_seconds": 1500, "network": "cluster"})
    );
    // The command's environment: the cache's, the user's --env winning.
    let env = started_env(&log);
    assert_eq!(env["SCCACHE_WEBDAV_ENDPOINT"], "https://cache.test/sccache");
    assert_eq!(env["SCCACHE_WEBDAV_TOKEN"], CACHE_TOKEN);
    assert_eq!(env["TURBO_TEAM"], "mine");
    // The token is in no event, no log line and no error.
    assert!(!stdout.contains(CACHE_TOKEN), "{stdout}");
    assert!(!err.contains(CACHE_TOKEN), "{err}");
    let events: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let running = events.iter().find(|e| e["type"] == "running").unwrap();
    assert_eq!(running["cache"], true);
    assert!(!err.contains("build cache unavailable"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_caps_the_token_life_at_the_longest_run() {
    let sb = Sandbox::new("cache-ttl");
    project(&sb.dir);
    let (cache, clog) = serve(vec![(200, cache_reply())]).await;
    let (url, _log) = build_stub(vec![envd_process("", 0), ended()]).await;
    let out = run_with(
        &sb,
        &url,
        &["build", "run", "--timeout", "6h", "--", "make"],
        |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    assert_eq!(out.status.code(), Some(0));
    let body: Value = serde_json::from_str(&clog.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["ttl_seconds"], 22500);
}

/// Whatever goes wrong minting, the run builds without the cache, says so on
/// exactly one line, and stays within three `sylphx:` lines.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_builds_without_the_cache_when_it_cannot_be_minted() {
    let hang = hanging_server().await;
    let (e503, _) = serve(vec![(
        503,
        json!({"error": {"code": "UNAVAILABLE", "message": "store down"}}),
    )])
    .await;
    let (e403, _) = serve(vec![(
        403,
        json!({"error": {"code": "PERMISSION_DENIED", "message": "no"}}),
    )])
    .await;
    let (e401, _) = serve(vec![(
        401,
        json!({"error": {"code": "UNAUTHENTICATED", "message": "no"}}),
    )])
    .await;
    let (e400, _) = serve(vec![(
        400,
        json!({"error": {"code": "INVALID_ARGUMENT", "message": "ttl"}}),
    )])
    .await;
    let (junk, _) = serve_bytes(vec![(200, "application/json", b"{not json".to_vec())]).await;
    let (noenv, _) = serve(vec![(200, json!({"token": CACHE_TOKEN}))]).await;
    let cases: Vec<(&str, String, &str)> = vec![
        ("503", e503, "HTTP 503 UNAVAILABLE"),
        ("403", e403, "HTTP 403 PERMISSION_DENIED"),
        ("401", e401, "HTTP 401 UNAUTHENTICATED"),
        ("400", e400, "HTTP 400 INVALID_ARGUMENT"),
        ("json", junk, "unreadable reply"),
        ("noenv", noenv, "unreadable reply"),
        ("down", "http://127.0.0.1:9".into(), "unreachable"),
        ("timeout", hang, "timed out"),
    ];
    for (name, cache, reason) in cases {
        let sb = Sandbox::new(&format!("cache-fail-{name}"));
        project(&sb.dir);
        let (url, log) = build_stub(vec![envd_process("ok\n", 0), ended()]).await;
        let out = run_with(&sb, &url, &["build", "run", "--", "make"], |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        })
        .await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{name}: {err}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n", "{name}");
        let warn: Vec<&str> = err.lines().filter(|l| l.contains("build cache")).collect();
        assert_eq!(
            warn,
            [format!(
                "sylphx: warning: build cache unavailable ({reason}); building without it"
            )],
            "{name}: {err}"
        );
        assert!(sylphx_lines(&err).len() <= 3, "{name}: {err}");
        assert_eq!(started_env(&log), json!({}), "{name}");
        assert!(!err.contains(CACHE_TOKEN), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_no_cache_mints_nothing_and_warns_of_nothing() {
    let sb = Sandbox::new("cache-off");
    project(&sb.dir);
    let (cache, clog) = serve(vec![(200, cache_reply())]).await;
    let (url, log) = build_stub(vec![envd_process("", 0), ended()]).await;
    let out = run_with(
        &sb,
        &url,
        &["build", "run", "--no-cache", "--", "make"],
        |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(clog.lock().unwrap().is_empty(), "no token was asked for");
    assert!(!err.contains("build cache"), "{err}");
    assert_eq!(sylphx_lines(&err).len(), 3, "{err}");
    assert_eq!(started_env(&log), json!({}));
}

/// An error from the machine that happens to repeat the token does not
/// carry it out.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_errors_never_carry_the_token() {
    let sb = Sandbox::new("cache-scrub");
    project(&sb.dir);
    let (cache, _) = serve(vec![(200, cache_reply())]).await;
    let echo = format!("boom {CACHE_TOKEN}").into_bytes();
    let (url, _log) = build_stub(vec![
        (500, "text/plain", echo),
        (
            200,
            "application/json",
            json!({"name": format!("{ENV}/leases/l1"), "status": {"state": "ready"}})
                .to_string()
                .into_bytes(),
        ),
        ended(),
    ])
    .await;
    let out = run_with(&sb, &url, &["build", "run", "--", "make"], |c| {
        c.env("SYLPHX_BUILD_CACHE_URL", &cache);
    })
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{err}");
    assert!(err.contains("sylphx: error:"), "{err}");
    assert!(!err.contains(CACHE_TOKEN), "{err}");
}

fn fake_sccache_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sylphx-sccache-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("sccache");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn build_cache_env_prints_shell_exports_from_a_read_token() {
    let sb = Sandbox::new("cache-env");
    let mut reply = cache_reply();
    reply["env"]["ODD"] = json!("it's $HOME `x` \"y\"");
    let (cache, clog) = serve(vec![(200, reply)]).await;
    let with = fake_sccache_dir("with");
    let out = run_with(&sb, "http://127.0.0.1:9", &["build", "cache", "env"], |c| {
        c.env("SYLPHX_BUILD_CACHE_URL", &cache)
            .env("PATH", &with)
            .env_remove("RUSTC_WRAPPER");
    })
    .await;
    let _ = std::fs::remove_dir_all(&with);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains(&format!("export SCCACHE_WEBDAV_TOKEN='{CACHE_TOKEN}'\n")),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("export RUSTC_WRAPPER=sccache\n"),
        "{stdout}"
    );
    assert!(!err.contains(CACHE_TOKEN), "{err}");
    // A shell reads the quoting back to the same values.
    let script = format!("{stdout}\nprintf %s \"$ODD\"");
    let echoed = Command::new("sh").arg("-c").arg(script).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&echoed.stdout),
        "it's $HOME `x` \"y\""
    );
    let seen = clog.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let body: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(
        body,
        json!({"project": "prj_a", "access": "read", "ttl_seconds": 43200, "network": "public"})
    );
    assert!(
        seen[0]
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer sylphx_sk_test"),
        "{}",
        seen[0].head
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn build_cache_env_adds_the_wrapper_only_when_it_is_missing_and_usable() {
    let with = fake_sccache_dir("wrapper");
    let without = std::env::temp_dir().join(format!("sylphx-no-sccache-{}", std::process::id()));
    std::fs::create_dir_all(&without).unwrap();
    for (name, path, wrapper, want) in [
        ("none", &without, None, false),
        ("set", &with, Some("other"), false),
        ("present", &with, None, true),
    ] {
        let sb = Sandbox::new(&format!("cache-wrapper-{name}"));
        let (cache, _) = serve(vec![(200, cache_reply())]).await;
        let out = run_with(&sb, "http://127.0.0.1:9", &["build", "cache", "env"], |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache).env("PATH", path);
            match wrapper {
                Some(w) => c.env("RUSTC_WRAPPER", w),
                None => c.env_remove("RUSTC_WRAPPER"),
            };
        })
        .await;
        assert_eq!(out.status.code(), Some(0), "{name}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout.contains("RUSTC_WRAPPER"), want, "{name}: {stdout}");
    }
    let _ = std::fs::remove_dir_all(&with);
    let _ = std::fs::remove_dir_all(&without);
}

#[tokio::test(flavor = "multi_thread")]
async fn build_cache_env_json_is_the_env_object_and_takes_a_project() {
    let sb = Sandbox::new("cache-env-json");
    let (cache, clog) = serve(vec![(200, cache_reply())]).await;
    let out = run_with(
        &sb,
        "http://127.0.0.1:9",
        &[
            "build",
            "cache",
            "env",
            "--project",
            "prj_other",
            "-o",
            "json",
        ],
        |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, cache_reply()["env"]);
    let body: Value = serde_json::from_str(&clog.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["project"], "prj_other");
}

#[tokio::test(flavor = "multi_thread")]
async fn build_cache_env_failures_exit_125_or_2_and_say_why() {
    let (e503, _) = serve(vec![(
        503,
        json!({"error": {"code": "UNAVAILABLE", "message": "store down"}}),
    )])
    .await;
    let (e403, _) = serve(vec![(
        403,
        json!({"error": {"code": "PERMISSION_DENIED", "message": "key lacks build:cache.read"}}),
    )])
    .await;
    let (e400, _) = serve(vec![(
        400,
        json!({"error": {"code": "INVALID_ARGUMENT", "message": "unknown project"}}),
    )])
    .await;
    let (junk, _) = serve_bytes(vec![(200, "application/json", b"<html>".to_vec())]).await;
    let hang = hanging_server().await;
    for (name, cache, code, said) in [
        ("503", e503, 125, "store down"),
        ("403", e403, 125, "key lacks build:cache.read"),
        ("400", e400, 2, "unknown project"),
        ("junk", junk, 125, "unreadable reply"),
        ("down", "http://127.0.0.1:9".to_string(), 125, "unreachable"),
        ("hang", hang, 125, "timed out"),
        ("http", "http://cache.example.com".to_string(), 2, "https"),
    ] {
        let sb = Sandbox::new(&format!("cache-env-fail-{name}"));
        let out = run_with(&sb, "http://127.0.0.1:9", &["build", "cache", "env"], |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        })
        .await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "{name}: {err}");
        assert!(
            err.contains("error:") && err.contains(said),
            "{name}: {err}"
        );
        assert!(out.stdout.is_empty(), "{name}: nothing on stdout");
        assert!(!err.contains("sylphx_sk_test"), "{name}: {err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn build_cache_env_needs_a_project_and_a_key() {
    // No linked project and a key that names none: usage, and no mint.
    let sb = Sandbox::new("cache-env-noproject");
    std::fs::remove_file(sb.dir.join(".sylphx/project.json")).unwrap();
    let (api, _) = serve(vec![(200, json!({"org": "orgs/org_a"}))]).await;
    let (cache, clog) = serve(vec![(200, cache_reply())]).await;
    let out = run_with(&sb, &api, &["build", "cache", "env"], |c| {
        c.env("SYLPHX_BUILD_CACHE_URL", &cache);
    })
    .await;
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no project"));
    assert!(clog.lock().unwrap().is_empty());
    // Signed out: a platform failure, as for `build run`.
    let out = run_with(&sb, "http://127.0.0.1:9", &["build", "cache", "env"], |c| {
        c.env_remove("SYLPHX_API_KEY");
    })
    .await;
    assert_eq!(out.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not signed in"));
}
