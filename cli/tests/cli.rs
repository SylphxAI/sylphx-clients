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
