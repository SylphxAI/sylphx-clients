//! The `sylphx` binary against a local fake API: generated commands build the
//! wire request (ids, parents from the linked project, flags, update masks,
//! etags), wait for Operations, refuse unconfirmed destructive calls, and
//! `sylphx mcp` speaks MCP over stdio, and `sylphx build run` drives a lease
//! and its guest end to end.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[path = "support/devices_capture.rs"]
mod devices_capture;

#[tokio::test]
async fn apply_offline_refuses_an_undeclarable_kind_without_a_backend() {
    let sb = Sandbox::new("apply-offline-refused");
    std::fs::write(
        sb.dir.join("sylphx.toml"),
        "[[resource]]\nkind = 'hosting.sylphx.com/Service'\nname = 'main'\n",
    )
    .unwrap();
    let out = run(
        &sb,
        "http://127.0.0.1:1",
        &["apply", "--offline", "--detailed-exitcode"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("hosting.sylphx.com/Service"), "{err}");
    assert!(err.contains("is written by `[[services]]`"), "{err}");
}

#[tokio::test]
async fn apply_json_empty_manifest_has_no_changes_and_errors_exit_one() {
    let sb = Sandbox::new("apply-empty");
    std::fs::write(sb.dir.join("declarations.json"), "{}").unwrap();
    let out = run(
        &sb,
        "http://127.0.0.1:1",
        &[
            "apply",
            "--manifest",
            "declarations.json",
            "--detailed-exitcode",
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed["resources"], json!([]));
    let out = run(
        &sb,
        "http://127.0.0.1:1",
        &["apply", "--manifest", "missing.json", "--detailed-exitcode"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
}

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
    serve_bytes_headers_on(
        listener,
        replies.into_iter().map(|(s, t, b)| (s, t, b, "")).collect(),
    )
    .await
}

async fn serve_bytes_headers_on(
    listener: TcpListener,
    replies: Vec<(u16, &'static str, Vec<u8>, &'static str)>,
) -> (String, Log) {
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        let mut replies = replies.into_iter().peekable();
        while replies.peek().is_some() {
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
            // A recovered expired deadline may signal the guest before release.
            // That guest call must not consume the next platform API response.
            let (status, content_type, body, headers) =
                if line.starts_with("POST /process.Process/SendSignal ") {
                    (200, "application/json", b"{}".to_vec(), "")
                } else {
                    replies.next().unwrap()
                };
            seen.lock().push(Seen {
                line,
                head,
                body: body_in,
            });
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body).await.unwrap();
            sock.shutdown().await.ok();
        }
    });
    (url, log)
}

#[tokio::test]
async fn guest_signal_does_not_consume_the_lease_release_reply() {
    let (url, log) = serve_bytes(vec![ended()]).await;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let signal: Value = http
        .post(format!("{url}/process.Process/SendSignal"))
        .json(&json!({"process":{"pid":7},"signal":"SIGNAL_SIGKILL"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        signal,
        json!({}),
        "a signal must not consume the queued release response"
    );
    let released: Value = http
        .post(format!("{url}/v1/{ENV}/leases/l1:release"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(released["status"]["state"], "ending");
    assert_eq!(
        log.lock()
            .iter()
            .filter(|seen| seen.line.contains(":release"))
            .count(),
        1
    );
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
            .env("SYLPHX_NO_KEYCHAIN", "1")
            .env("SYLPHX_NO_BROWSER", "1")
            .env_remove("SYLPHX_ENVIRONMENT")
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

/// Admission's done Operation does not mean the Release is serving. Both
/// porcelain writes must read the Release's current live pointer before exit 0.
#[tokio::test(flavor = "multi_thread")]
async fn release_writes_follow_their_recorded_release_until_it_serves() {
    for verb in ["deploy", "rollback"] {
        let sb = Sandbox::new(&format!("release-{verb}"));
        let name = format!("{ENV}/releases/rel_new");
        let release = json!({
            "name": name,
            "status": {"state": "live", "current_release": name,
                "served_hostnames": ["app.example.com"]}
        });
        let (url, log) = serve(vec![
            (200, json!({"done": true, "response": {"name": name}})),
            (200, release.clone()),
        ])
        .await;
        let args = if verb == "deploy" {
            vec!["deploy", "--redeploy", "-o", "json"]
        } else {
            vec!["rollback", "--to", "rel_old", "--yes", "-o", "json"]
        };
        let out = run(&sb, &url, &args).await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(printed, release);
        let log = log.lock();
        assert_eq!(log.len(), 2);
        assert!(log[0]
            .line
            .starts_with(&format!("POST /v1/{ENV}/releases:{verb} ")));
        assert!(log[0].head.to_lowercase().contains("idempotency-key:"));
        let body: Value = serde_json::from_str(&log[0].body).unwrap();
        if verb == "rollback" {
            assert_eq!(body["release"], format!("{ENV}/releases/rel_old"));
        } else {
            assert_eq!(body["commit"], "");
        }
        assert!(log[1].line.starts_with(&format!("GET /v1/{name} ")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn release_status_fetches_the_live_pointer_not_the_newest_release() {
    let sb = Sandbox::new("release-status");
    let live = format!("{ENV}/releases/rel_serving");
    let newest = json!({"name": format!("{ENV}/releases/rel_new"),
        "status": {"state": "rolling_out", "current_release": live}});
    let serving = json!({"name": live,
        "status": {"state": "live", "current_release": live}});
    let (url, log) = serve(vec![
        (200, json!({"releases": [newest.clone()]})),
        (200, serving.clone()),
    ])
    .await;
    let out = run(&sb, &url, &["status", "-o", "json"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed, json!({"live": serving, "newest": newest}));
    let log = log.lock();
    assert_eq!(log.len(), 2);
    assert!(log[0].line.starts_with(&format!("GET /v1/{ENV}/releases?")));
    assert!(log[0].line.contains("page_size=1"));
    assert!(log[1].line.starts_with(&format!("GET /v1/{live} ")));
}

#[tokio::test(flavor = "multi_thread")]
async fn release_follow_reports_terminal_failure_instead_of_admission_success() {
    let sb = Sandbox::new("release-failed");
    let name = format!("{ENV}/releases/rel_failed");
    let (url, log) = serve(vec![
        (200, json!({"done": true, "response": {"name": name}})),
        (
            200,
            json!({"name": name, "status": {"state": "failed",
            "reason": "auto_halt:unavailable:web"}}),
        ),
    ])
    .await;
    let out = run(&sb, &url, &["deploy", "--redeploy", "-o", "json"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("auto_halt:unavailable:web"));
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed["status"]["state"], "failed");
    assert_eq!(log.lock().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn release_follow_times_out_on_stale_live_state_without_the_serving_pointer() {
    let sb = Sandbox::new("release-stale");
    let name = format!("{ENV}/releases/rel_stale");
    let stale = json!({"name": name, "status": {"state": "live",
        "current_release": format!("{ENV}/releases/rel_other")}});
    let (url, log) = serve(vec![
        (200, json!({"done": true, "response": {"name": name}})),
        (200, stale.clone()),
        (200, stale),
    ])
    .await;
    let out = run(&sb, &url, &["deploy", "--redeploy", "--timeout", "1"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("follow it with `sylphx status`"));
    assert!(
        out.stdout.is_empty(),
        "a stale state must not print success"
    );
    assert_eq!(log.lock().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn release_invalid_timeout_is_refused_without_contacting_the_api() {
    let sb = Sandbox::new("release-invalid-timeout");
    let (url, log) = serve(vec![(200, json!({}))]).await;
    let out = run(&sb, &url, &["deploy", "--redeploy", "--timeout", "0"]).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(log.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn file_flag_conflicts_are_refused_before_a_request() {
    for (collection, resource, field, flag, file_value, flag_value) in [
        (
            "volumes",
            "volume",
            "storage_class",
            "--spec.storage-class",
            "local",
            "standard",
        ),
        ("leases", "lease", "region", "--spec.region", "gra", "bhs"),
    ] {
        let sb = Sandbox::new(&format!("file-conflict-{collection}"));
        let path = sb.dir.join("request.json");
        let mut spec = serde_json::Map::new();
        spec.insert(field.into(), json!(file_value));
        let mut body = serde_json::Map::new();
        body.insert(resource.into(), json!({"spec": spec}));
        std::fs::write(&path, Value::Object(body).to_string()).unwrap();
        let (url, log) = serve(vec![(200, json!({}))]).await;
        let out = run(
            &sb,
            &url,
            &[
                "sandboxes",
                collection,
                "create",
                "-f",
                path.to_str().unwrap(),
                flag,
                flag_value,
            ],
        )
        .await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{err}");
        assert!(
            err.contains("conflict")
                && err.contains(flag)
                && err.contains(file_value)
                && err.contains(flag_value),
            "{err}"
        );
        assert!(log.lock().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn volume_create_accepts_matching_and_missing_file_fields() {
    for file_class in [Some("local"), None] {
        let sb = Sandbox::new(&format!("file-match-{file_class:?}"));
        let path = sb.dir.join("request.yaml");
        let mut spec = json!({"size_gib": 10});
        if let Some(class) = file_class {
            spec["storage_class"] = json!(class);
        }
        std::fs::write(
            &path,
            serde_yaml_ng::to_string(&json!({"volume_id": "test", "volume": {"spec": spec}}))
                .unwrap(),
        )
        .unwrap();
        let (url, log) = serve(vec![(200, json!({"name": format!("{ENV}/volumes/test")}))]).await;
        let out = run(
            &sb,
            &url,
            &[
                "sandboxes",
                "volumes",
                "create",
                "test",
                "-f",
                path.to_str().unwrap(),
                "--spec.storage-class",
                "local",
                "--spec.region",
                "gra",
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
        let log = log.lock();
        assert_eq!(log.len(), 1);
        let body: Value = serde_json::from_str(&log[0].body).unwrap();
        assert_eq!(body["spec"]["storage_class"], "local");
        assert_eq!(body["spec"]["region"], "gra");
        assert_eq!(body["spec"]["size_gib"], 10);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_create_without_a_webhook_reaches_the_api_including_dry_run() {
    for dry_run in [false, true] {
        let sb = Sandbox::new(&format!("lease-no-webhook-{dry_run}"));
        let (url, log) = serve(vec![(200, json!({"name": format!("{ENV}/leases/test")}))]).await;
        let mut args = vec![
            "sandboxes",
            "leases",
            "create",
            "test",
            "--spec.shape",
            "standard",
            "--spec.ttl",
            "60s",
            "--spec.image",
            "alpine",
            "--no-wait",
            "-o",
            "json",
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let out = run(&sb, &url, &args).await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let log = log.lock();
        assert_eq!(log.len(), 1);
        let body: Value = serde_json::from_str(&log[0].body).unwrap();
        assert!(body["spec"].get("webhook").is_none(), "{body}");
        assert_eq!(log[0].line.contains("validate_only=true"), dry_run);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_create_accepts_matching_and_missing_file_fields() {
    for file_region in [Some("gra"), None] {
        let sb = Sandbox::new(&format!("lease-file-match-{file_region:?}"));
        let path = sb.dir.join("request.json");
        let mut spec = json!({"shape": "standard", "ttl": "60s", "image": "alpine"});
        if let Some(region) = file_region {
            spec["region"] = json!(region);
        }
        std::fs::write(
            &path,
            json!({"lease_id": "test", "lease": {"spec": spec}}).to_string(),
        )
        .unwrap();
        let (url, log) = serve(vec![(200, json!({}))]).await;
        let out = run(
            &sb,
            &url,
            &[
                "sandboxes",
                "leases",
                "create",
                "test",
                "-f",
                path.to_str().unwrap(),
                "--spec.region",
                "gra",
                "--no-wait",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let log = log.lock();
        assert_eq!(log.len(), 1);
        let sent: Value = serde_json::from_str(&log[0].body).unwrap();
        assert_eq!(sent["spec"]["region"], "gra");
        assert_eq!(sent["spec"]["image"], "alpine");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn file_positional_conflicts_are_refused_before_a_request() {
    for (collection, verb, body) in [
        ("volumes", "create", json!({"volume_id": "file-id"})),
        (
            "volumes",
            "get",
            json!({"name": format!("{ENV}/volumes/file-id")}),
        ),
        ("leases", "create", json!({"lease_id": "file-id"})),
    ] {
        let sb = Sandbox::new(&format!("positional-conflict-{collection}-{verb}"));
        let path = sb.dir.join("request.json");
        std::fs::write(&path, body.to_string()).unwrap();
        let (url, log) = serve(vec![(200, json!({}))]).await;
        let out = run(
            &sb,
            &url,
            &[
                "sandboxes",
                collection,
                verb,
                "flag-id",
                "-f",
                path.to_str().unwrap(),
            ],
        )
        .await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{err}");
        assert!(err.contains("conflict"), "{err}");
        assert!(log.lock().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn password_file_conflicts_never_print_either_secret() {
    let sb = Sandbox::new("password-conflict");
    let path = sb.dir.join("request.json");
    let file_secret = "synthetic-file-password-16118";
    let flag_secret = "synthetic-flag-password-16118";
    std::fs::write(
        &path,
        json!({"end_user": {"password": file_secret}}).to_string(),
    )
    .unwrap();
    let (url, log) = serve(vec![(200, json!({}))]).await;
    let out = run(
        &sb,
        &url,
        &[
            "auth",
            "end-users",
            "create",
            "-f",
            path.to_str().unwrap(),
            "--password",
            flag_secret,
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("conflict"));
    for output in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(output);
        assert!(
            !text.contains(file_secret) && !text.contains(flag_secret),
            "secret reached output"
        );
    }
    assert!(log.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn equivalent_numeric_file_and_flag_values_are_accepted() {
    for (service, collection, resource, field, flag, file_value) in [
        (
            "data",
            "databases",
            "database",
            "compute_units",
            "--spec.compute-units",
            json!(2.0),
        ),
        (
            "sandboxes",
            "leases",
            "lease",
            "budget",
            "--spec.budget.max-cpu-seconds",
            json!({"max_cpu_seconds": "2"}),
        ),
    ] {
        let sb = Sandbox::new(&format!("numeric-match-{collection}"));
        let path = sb.dir.join("request.json");
        let mut body = json!({resource: {"spec": {field: file_value}}});
        if collection == "leases" {
            body[resource]["spec"]["shape"] = json!("standard");
            body[resource]["spec"]["ttl"] = json!("60s");
        }
        std::fs::write(&path, body.to_string()).unwrap();
        let (url, log) = serve(vec![(200, json!({}))]).await;
        let out = run(
            &sb,
            &url,
            &[
                service,
                collection,
                "create",
                "test",
                "-f",
                path.to_str().unwrap(),
                flag,
                "2",
                "--no-wait",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let log = log.lock();
        assert_eq!(log.len(), 1);
        let sent: Value = serde_json::from_str(&log[0].body).unwrap();
        if collection == "leases" {
            assert_eq!(sent["spec"][field]["max_cpu_seconds"], 2);
        } else {
            assert_eq!(sent["spec"][field], 2);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn matching_update_flags_preserve_file_edits_and_explicit_masks() {
    for explicit_mask in [false, true] {
        let sb = Sandbox::new(&format!("file-update-mask-{explicit_mask}"));
        let path = sb.dir.join("request.json");
        let key = format!("{ENV}/api_keys/key_a");
        let mut body = json!({"api_key": {"name": key, "meta": {"etag": "k3", "display_name": "file edit"}, "spec": {"label": "ci"}}});
        if explicit_mask {
            body["update_mask"] = json!("spec.label,meta.display_name");
        }
        std::fs::write(&path, body.to_string()).unwrap();
        let (url, log) = serve(vec![(200, json!({"name": key}))]).await;
        let out = run(
            &sb,
            &url,
            &[
                "access",
                "api-keys",
                "update",
                "key_a",
                "-f",
                path.to_str().unwrap(),
                "--spec.label",
                "ci",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let log = log.lock();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0].line.contains("update_mask="),
            explicit_mask,
            "{}",
            log[0].line
        );
        if explicit_mask {
            assert!(
                log[0]
                    .line
                    .contains("update_mask=spec.label%2Cmeta.display_name"),
                "{}",
                log[0].line
            );
        }
        let sent: Value = serde_json::from_str(&log[0].body).unwrap();
        assert_eq!(sent["meta"]["display_name"], "file edit");
        assert_eq!(sent["spec"]["label"], "ci");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_file_in_another_shape_never_shadows_the_key_scope() {
    // The older `{orgId, projectId}` file names no org, project or env: it is
    // skipped with a warning, and the key's own env (whoami) is the default.
    let (url, log) = serve(vec![
        (200, whoami_reply()),
        (
            200,
            json!({"name": format!("{ENV}/databases"), "databases": []}),
        ),
    ])
    .await;
    let sb = Sandbox::new("legacy-link");
    std::fs::write(
        sb.dir.join(".sylphx/project.json"),
        json!({"projectId": "proj_x", "orgId": "org_x"}).to_string(),
    )
    .unwrap();
    let out = run(&sb, &url, &["data", "databases", "list", "-o", "json"]).await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("warning: ignoring") && err.contains(".sylphx/project.json"),
        "{err}"
    );
    let log = log.lock().clone();
    assert!(
        log[0].line.starts_with("GET /v1/whoami "),
        "{}",
        log[0].line
    );
    assert!(
        log[1].line.starts_with(&format!("GET /v1/{ENV}/databases")),
        "{}",
        log[1].line
    );
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
    let log = log.lock().clone();
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
    let log = log.lock().clone();
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
        log.lock().iter().all(|s| !s.line.starts_with("POST")),
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
    assert!(log.lock()[0]
        .line
        .starts_with(&format!("GET /v1/{ENV}/databases?page_size=10 ")));
}

/// `sylphx money revenue summary` reads the linked org's revenue summary
/// (the org-level singleton `orgs/{org}/revenue`): `--by` is the split.
#[tokio::test(flavor = "multi_thread")]
async fn revenue_summary_reads_the_linked_orgs_revenue() {
    let (url, log) = serve(vec![(
        200,
        json!({"name": "orgs/org_a/revenue", "currency_code": "USD", "total": "1500", "split": "source"}),
    )])
    .await;
    let sb = Sandbox::new("revenue");
    let out = run(
        &sb,
        &url,
        &[
            "money", "revenue", "summary", "--period", "mtd", "--by", "source", "--output", "json",
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1500"));
    let line = log.lock()[0].line.clone();
    assert!(line.starts_with("GET /v1/orgs/org_a/revenue?"), "{line}");
    assert!(
        line.contains("period=mtd") && line.contains("split=source"),
        "{line}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sylphx_env_overrides_the_link_file() {
    let other = "orgs/org_b/projects/prj_b/envs/env_b";
    let (url, log) = serve(vec![(200, json!({"databases": []}))]).await;
    let sb = Sandbox::new("env-override");
    let mut c = sb.cmd(&url, &["data", "databases", "list"]);
    c.env("SYLPHX_ENVIRONMENT", other);
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = log.lock();
    assert!(
        seen[0]
            .line
            .starts_with(&format!("GET /v1/{other}/databases")),
        "{}",
        seen[0].line
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sylphx_env_of_another_shape_is_a_usage_error_and_calls_nothing() {
    let (url, log) = serve(vec![]).await;
    let sb = Sandbox::new("env-bad");
    for bad in [
        "orgs/org_b/projects/prj_b",
        "env_b",
        "orgs//projects/p/envs/e",
        "orgs/o/projects/p/envs/e/x",
    ] {
        let mut c = sb.cmd(&url, &["data", "databases", "list"]);
        c.env("SYLPHX_ENVIRONMENT", bad);
        let out = tokio::task::spawn_blocking(move || c.output().unwrap())
            .await
            .unwrap();
        assert!(!out.status.success(), "{bad}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("SYLPHX_ENVIRONMENT must be a full environment name"),
            "{bad}: {err}"
        );
    }
    assert!(log.lock().is_empty());
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
    let seen = log.lock();
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
            json!({"keys": [{"api_key": "sylphx_sk_test_issued", "org": {"id": "org_a", "slug": "acme"}, "key": {"name": "orgs/org_a/api_keys/key_cli"}}], "active_after_ms": 300}),
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
    let seen = log.lock();
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
            json!({"keys": [{"api_key": "sylphx_sk_test_issued", "org": {"id": "org_a", "slug": "acme"}, "key": {"name": "orgs/org_a/api_keys/key_cli"}}], "active_after_ms": 300}),
        ),
    ]
}

fn two_org_replies() -> Vec<(u16, Value)> {
    let mut replies = device_replies();
    replies[1].1 = json!({"keys": [
        {"api_key": "sylphx_sk_org_a", "key": {"name": "orgs/org_a/api_keys/cli_a"}, "org": {"id": "org_a", "slug": "acme"}},
        {"api_key": "sylphx_sk_org_b", "key": {"name": "orgs/org_b/api_keys/cli_b"}, "org": {"id": "org_b", "slug": "beta"}}
    ], "active_after_ms": 0});
    replies
}

async fn run_stored(sb: &Sandbox, url: &str, args: &[&str]) -> std::process::Output {
    let mut c = sb.cmd(url, args);
    c.env_remove("SYLPHX_API_KEY");
    tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap()
}

fn assert_success(out: &std::process::Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn device_login_stores_all_keys_and_selects_org_per_command() {
    let mut replies = two_org_replies();
    replies.extend((0..7).map(|_| (200, whoami_reply())));
    replies.push((
        200,
        json!({"name": "orgs/org_b/projects/prj_b/envs/env_b/databases/db_x"}),
    ));
    let (url, log) = serve(replies).await;
    let sb = Sandbox::new("login-two-orgs");
    let login = run_stored(&sb, &url, &["login"]).await;
    assert_success(&login);
    for stream in [&login.stdout, &login.stderr] {
        assert!(
            !String::from_utf8_lossy(stream).contains("sylphx_sk_org_"),
            "a login secret was printed"
        );
    }
    let creds: Value = serde_json::from_str(
        &std::fs::read_to_string(sb.dir.join("config/credentials.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(creds["orgs"].as_array().unwrap().len(), 2);
    assert_eq!(creds["orgs"][0]["api_key"], "sylphx_sk_org_a");
    assert_eq!(creds["orgs"][1]["api_key"], "sylphx_sk_org_b");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(sb.dir.join("config/credentials.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_success(&run_stored(&sb, &url, &["whoami", "--org", "beta"]).await);
    assert_success(&run_stored(&sb, &url, &["whoami"]).await);
    let mut c = sb.cmd(&url, &["whoami"]);
    c.env_remove("SYLPHX_API_KEY")
        .env("SYLPHX_ENVIRONMENT", "orgs/org_b/projects/prj_b/envs/env_b");
    assert_success(
        &tokio::task::spawn_blocking(move || c.output().unwrap())
            .await
            .unwrap(),
    );
    std::fs::remove_file(sb.dir.join(".sylphx/project.json")).unwrap();
    assert_success(&run_stored(&sb, &url, &["org", "use", "beta"]).await);
    assert_success(&run_stored(&sb, &url, &["whoami"]).await);
    assert_success(&run_stored(&sb, &url, &["whoami", "--org", "orgs/org_a"]).await);
    assert_success(
        &run_stored(
            &sb,
            &url,
            &[
                "data",
                "databases",
                "get",
                "orgs/org_b/projects/prj_b/envs/env_b/databases/db_x",
            ],
        )
        .await,
    );
    let seen = log.lock();
    let body: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert!(
        body.get("org").is_none(),
        "login without --org must not lock selection"
    );
    for (request, key) in seen[2..].iter().zip([
        "sylphx_sk_org_a",
        "sylphx_sk_org_b",
        "sylphx_sk_org_b",
        "sylphx_sk_org_a",
        "sylphx_sk_org_b",
        "sylphx_sk_org_b",
        "sylphx_sk_org_a",
        "sylphx_sk_org_b",
    ]) {
        assert!(
            request
                .head
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {key}")),
            "{}",
            request.head
        );
    }
    assert_eq!(seen.len(), 10);
}

#[tokio::test(flavor = "multi_thread")]
async fn fixed_org_relogin_replaces_only_that_org_credential() {
    let mut replies = two_org_replies();
    replies.extend(device_replies());
    let (url, log) = serve(replies).await;
    let sb = Sandbox::new("relogin-one-org");
    assert_success(&run_stored(&sb, &url, &["login", "--no-verify"]).await);
    assert_success(&run_stored(&sb, &url, &["login", "--org", "acme", "--no-verify"]).await);
    let creds: Value = serde_json::from_str(
        &std::fs::read_to_string(sb.dir.join("config/credentials.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(creds["orgs"].as_array().unwrap().len(), 2);
    assert_eq!(creds["orgs"][0]["api_key"], "sylphx_sk_test_issued");
    assert_eq!(creds["orgs"][1]["api_key"], "sylphx_sk_org_b");
    let seen = log.lock();
    let requested: Value = serde_json::from_str(&seen[2].body).unwrap();
    assert_eq!(requested["org"], "acme");
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_selected_org_preserves_other_keys_and_all_revokes_the_rest() {
    let mut replies = two_org_replies();
    replies.extend([
        (200, json!({"ok": true})),
        (200, whoami_reply()),
        (200, json!({"ok": true})),
    ]);
    let (url, log) = serve(replies).await;
    let sb = Sandbox::new("logout-two-orgs");
    assert_success(&run_stored(&sb, &url, &["login", "--no-verify"]).await);
    assert_success(&run_stored(&sb, &url, &["logout", "--org", "acme"]).await);
    let path = sb.dir.join("config/credentials.json");
    let creds: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(creds["orgs"].as_array().unwrap().len(), 1);
    assert_eq!(creds["orgs"][0]["api_key"], "sylphx_sk_org_b");
    assert_eq!(creds["active_org"], "org_b");
    assert_success(&run_stored(&sb, &url, &["whoami", "--org", "beta"]).await);
    assert_success(&run_stored(&sb, &url, &["logout", "--all"]).await);
    assert!(!path.exists());
    let seen = log.lock();
    assert!(seen[2]
        .line
        .starts_with("POST /v1/access/keys/self:revoke "));
    assert!(seen[2].head.contains("Bearer sylphx_sk_org_a"));
    assert!(seen[3].head.contains("Bearer sylphx_sk_org_b"));
    assert!(seen[4]
        .line
        .starts_with("POST /v1/access/keys/self:revoke "));
    assert!(seen[4].head.contains("Bearer sylphx_sk_org_b"));
}

#[tokio::test(flavor = "multi_thread")]
async fn supplied_login_and_logout_preserve_scoped_org_credentials() {
    let mut replies = two_org_replies();
    replies.extend([
        (200, whoami_reply()),
        (200, whoami_reply()),
        (200, json!({"ok": true})),
        (200, json!({"ok": true})),
        (200, json!({"ok": true})),
    ]);
    let (url, log) = serve(replies).await;
    let sb = Sandbox::new("supplied-login-orgs");
    assert_success(&run_stored(&sb, &url, &["login", "--no-verify"]).await);
    assert_success(
        &run_stored(
            &sb,
            &url,
            &["login", "--key", "sylphx_sk_supplied", "--no-verify"],
        )
        .await,
    );
    assert_success(&run_stored(&sb, &url, &["whoami"]).await);
    assert_success(&run_stored(&sb, &url, &["whoami", "--org", "beta"]).await);
    assert_success(&run_stored(&sb, &url, &["logout"]).await);
    let path = sb.dir.join("config/credentials.json");
    let creds: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(creds["orgs"].as_array().unwrap().len(), 2);
    assert!(creds.get("api_key").is_none());
    assert_success(&run_stored(&sb, &url, &["logout", "--all"]).await);
    assert!(!path.exists());
    let seen = log.lock();
    for (request, key) in seen[2..].iter().zip([
        "sylphx_sk_supplied",
        "sylphx_sk_org_b",
        "sylphx_sk_supplied",
        "sylphx_sk_org_a",
        "sylphx_sk_org_b",
    ]) {
        assert!(request.head.contains(&format!("Bearer {key}")));
    }
    assert_eq!(seen.len(), 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_credentials_override_org_and_legacy_file_credentials_still_work() {
    let (url, log) = serve(vec![(200, whoami_reply()); 3]).await;
    let sb = Sandbox::new("legacy-org-credentials");
    std::fs::create_dir_all(sb.dir.join("config")).unwrap();
    let path = sb.dir.join("config/credentials.json");
    std::fs::write(&path, json!({"api_key": "sylphx_sk_legacy", "store": "file", "base_url": url, "key_name": "orgs/org_a/api_keys/old"}).to_string()).unwrap();
    assert_success(&run_stored(&sb, &url, &["whoami"]).await);
    let mut creds: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    creds["orgs"] = json!([{"id": "org_b", "slug": "beta", "store": "file", "key_name": "orgs/org_b/api_keys/cli", "api_key": "sylphx_sk_org_b"}]);
    creds["active_org"] = json!("org_b");
    std::fs::write(&path, creds.to_string()).unwrap();
    assert_success(&run(&sb, &url, &["whoami", "--org", "beta"]).await);
    assert_success(
        &run(
            &sb,
            &url,
            &["whoami", "--org", "beta", "--key", "sylphx_sk_explicit"],
        )
        .await,
    );
    let seen = log.lock();
    assert!(seen[0].head.contains("Bearer sylphx_sk_legacy"));
    assert!(seen[1].head.contains("Bearer sylphx_sk_test"));
    assert!(seen[2].head.contains("Bearer sylphx_sk_explicit"));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_org_and_rejected_supplied_login_never_discard_other_credentials() {
    let mut replies = two_org_replies();
    replies.push((401, json!({"code": "UNAUTHENTICATED", "status": 401, "detail": "invalid credential", "retryable": false})));
    let (url, log) = serve(replies).await;
    let sb = Sandbox::new("org-credentials-retained");
    assert_success(&run_stored(&sb, &url, &["login", "--no-verify"]).await);
    let path = sb.dir.join("config/credentials.json");
    let before = std::fs::read(&path).unwrap();
    assert!(!run_stored(&sb, &url, &["whoami", "--org", "absent"])
        .await
        .status
        .success());
    assert!(!run_stored(&sb, &url, &["org", "use", "absent"])
        .await
        .status
        .success());
    assert!(!run_stored(&sb, &url, &["logout", "--org", "absent"])
        .await
        .status
        .success());
    assert!(
        !run_stored(&sb, &url, &["login", "--api-key", "sylphx_sk_invalid"])
            .await
            .status
            .success()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(log.lock().len(), 3);
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
    assert_eq!(log.lock().len(), 5);
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
    assert!(log.lock().is_empty(), "no device grant started");
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
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}})).unwrap();
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
    let log = log.lock().clone();
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
    assert_eq!(log.lock().len(), 2, "the second call made no request");
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
    assert_eq!(log2.lock().len(), 2);

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
    assert_eq!(log3.lock().len(), 2);
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
    assert_eq!(log.lock().len(), 2);
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
    let log = log.lock().clone();
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
async fn auth_enable_preserves_a_real_cell_bearing_environment_name() {
    let env = "orgs/org_fs01afbdccgn8xdk1ghj5y47w0vtph/projects/prj_fs016wck3akrzep3zkwbv3dt0k9hgr/envs/env_fs01y448d2kqbb8ntphzf61k85wthw";
    let (url, log) = serve(vec![(
        200,
        json!({"status": "active", "providerId": "sylphx-auth",
               "config": {"organizationId": "org-1"}}),
    )])
    .await;
    let sb = Sandbox::new("auth-enable-canonical-env");
    std::fs::write(
        sb.dir.join(".sylphx/project.json"),
        json!({"org": "orgs/org_fs01afbdccgn8xdk1ghj5y47w0vtph",
               "project": "orgs/org_fs01afbdccgn8xdk1ghj5y47w0vtph/projects/prj_fs016wck3akrzep3zkwbv3dt0k9hgr",
               "env": env}).to_string(),
    ).unwrap();
    let out = run(&sb, &url, &["auth", "enable", "--env", env]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("org-1"));
    let requests = log.lock();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .line
        .starts_with("PUT /v1/projects/prj_fs016wck3akrzep3zkwbv3dt0k9hgr/composition/bindings "));
    assert_eq!(
        serde_json::from_str::<Value>(&requests[0].body).unwrap(),
        json!({"capabilityId": "identity", "slot": "identity",
               "providerId": "sylphx-auth", "environmentId": "env_fs01y448d2kqbb8ntphzf61k85wthw"})
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
    assert!(log.lock()[0]
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
    let seen = log.lock();
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
    let marker = base64::engine::general_purpose::STANDARD
        .encode(sylphx_build_lease::scripts::COMMAND_STARTED);
    b.extend(envd_frame(
        0,
        json!({"event": {"data": {"stderr": marker}}}),
    ));
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

/// First use with an org-scoped login discovers the default context, then runs
/// a volume-free Build end to end and preserves the command's output/exit code.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_syncs_runs_and_returns_the_commands_exit_code() {
    let sb = Sandbox::new("build-run");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("main.rs"), "fn main() {}\n").unwrap();
    unlink(&sb);
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let lease = format!("{ENV}/leases/l1");
    // The guest is the same stub: its address is the lease's guest_uri.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let replies = vec![
        json_reply(whoami_org_only()),
        json_reply(projects_page(vec![project_row(
            "orgs/org_a/projects/prj_a",
            "default",
        )])),
        json_reply(envs_page(vec![env_row(ENV, "production")])),
        json_reply(
            json!({"name": lease, "spec": {"region": "hk"}, "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}}),
        ),
        json_reply(json!({"token": "h.p.s", "generation": 1})),
        json_reply(json!({})),
        json_reply(json!({})),
        envd_process("", 0),
        envd_process("hello from the build machine\n", 3),
        json_reply(json!({"name": lease, "status": {"state": "ending"}})),
    ];
    assert!(
        TcpListener::bind(url.strip_prefix("http://").unwrap())
            .await
            .is_err(),
        "the stub address must remain reserved during reply setup"
    );
    let (url2, log) = serve_bytes_on(listener, replies).await;
    assert_eq!(url, url2);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(
        &sb,
        &url,
        &[
            "build",
            "run",
            "--region",
            "hk",
            "--no-cache",
            "--",
            "cargo",
            "test",
        ],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello from the build machine\n"
    );
    assert_volume_free_regional_leases(&log, "hk");

    assert_eq!(read_link(&sb)["env"], ENV);
    let requests = log.lock();
    let seen = &requests[3..];
    let lines: Vec<&str> = seen.iter().map(|s| s.line.as_str()).collect();
    assert!(
        lines[0].starts_with(&format!("POST /v1/{ENV}/leases")),
        "{lines:?}"
    );
    let created: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(created["spec"]["shape"], "build-large");
    assert_eq!(created["spec"]["image"], "template:build");
    let net = &created["spec"]["network"];
    assert!(
        net["egress"]
            .as_str()
            .unwrap()
            .eq_ignore_ascii_case("allowlist"),
        "{net}"
    );
    // No internet package host by default: only the build-cache gateway.
    assert_eq!(
        net["allowed_domains"],
        json!(["build-cache.sylphx.net"]),
        "{net}"
    );
    // The guest calls carry the sandbox id and the lease token, never the key.
    let guest: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.line.starts_with("POST /files") || s.line.starts_with("POST /process."))
        .collect();
    for g in &guest {
        let head = g.head.to_ascii_lowercase();
        assert!(head.contains("e2b-sandbox-id: abcd1234"), "{}", g.head);
        assert!(head.contains("x-access-token: h.p.s"), "{}", g.head);
        assert!(!head.contains("sylphx_sk_org_a"), "{}", g.head);
    }
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
    std::fs::write(sb.dir.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(sb.dir.join("ignored.log"), "not sent").unwrap();
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
    assert_eq!(
        ev["files"], 2,
        "only source and .gitignore; no output or Git initialization: {ev}"
    );
    assert!(!sb.dir.join(".git").exists());
}

/// `--region gra` pins a volume-free lease even when admission is refused.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_region_pins_the_volume_free_lease() {
    let sb = Sandbox::new("build-run-region");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let mut replies = Vec::new();
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
    assert_volume_free_regional_leases(&log, "gra");
    // No capacity in the region: the platform exit code, retryable, which the
    // caller can turn into a run in the home region.
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["exit_code"], 125);
    assert_eq!(last["retryable"], true, "{last}");
    assert!(last["error"].as_str().unwrap().contains("gra"), "{last}");
}

/// No --region: send an unpinned ephemeral lease directly, without listing,
/// creating or mounting any home-region workspace.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_without_region_requests_automatic_ephemeral_placement() {
    let sb = Sandbox::new("build-run-home");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let (url, log) = serve(vec![
        (
            400,
            json!({"code": "SHAPE_NOT_OFFERED", "status": 400, "detail": "shape `build-large` is not offered", "retryable": false}),
        ),
    ])
    .await;
    let out = run(&sb, &url, &["build", "run", "-o", "json", "--", "true"]).await;
    assert_eq!(out.status.code(), Some(125));
    let seen = log.lock();
    assert_eq!(seen.len(), 1, "no workspace API calls");
    assert!(seen[0].line.starts_with(&format!("POST /v1/{ENV}/leases")));
    let lease: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert!(
        lease["spec"]
            .get("region")
            .is_none_or(|r| r.as_str() == Some("")),
        "{lease}"
    );
    assert!(
        lease["spec"]
            .get("volumes")
            .is_none_or(|v| v.as_array().is_some_and(|a| a.is_empty())),
        "automatic placement must not bind a home-region PVC: {lease}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_automatic_placement_executes_in_the_selected_fr_cell() {
    let sb = Sandbox::new("build-run-auto-gra");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("main.rs"), "fn main() {}\n").unwrap();
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let lease = format!("{ENV}/leases/l1");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let replies = vec![
        json_reply(
            json!({"name": lease, "spec": {"region": "gra"}, "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}}),
        ),
        json_reply(json!({"token": "h.p.s", "generation": 1})),
        json_reply(json!({})),
        json_reply(json!({})),
        envd_process("", 0),
        envd_process("automatic FR command\n", 0),
        json_reply(json!({"name": lease, "status": {"state": "ending"}})),
    ];
    let (url, log) = serve_bytes_on(listener, replies).await;
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "-o",
            "json",
            "--no-cache",
            "--",
            "cargo",
            "check",
        ],
    )
    .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let running = events.iter().find(|e| e["type"] == "running").unwrap();
    assert_eq!(running["region"], "gra");
    assert_eq!(running["volume"], false);
    let seen = log.lock();
    assert!(seen[0].line.starts_with(&format!("POST /v1/{ENV}/leases")));
    assert!(!seen.iter().any(|s| s.line.contains("/volumes")));
    let created: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert!(created["spec"]
        .get("region")
        .is_none_or(|r| r.as_str() == Some("")));
    assert!(created["spec"]
        .get("volumes")
        .is_none_or(|v| v.as_array().is_some_and(|a| a.is_empty())));
    for request in seen
        .iter()
        .filter(|s| s.line.starts_with("POST /files") || s.line.starts_with("POST /process."))
    {
        let headers = request.head.to_ascii_lowercase();
        assert!(headers.contains("x-access-token: h.p.s"));
        assert!(!headers.contains("sylphx_sk_test"));
    }
}

/// Acceptance fixes the Cell before READY, not only after a machine boots.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_automatic_retry_keeps_the_cell_of_a_lost_pending_lease() {
    automatic_placement_retry("gra", true).await;
}

/// An input upload fault may retry the machine, never its placement.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_automatic_retry_keeps_the_cell_after_a_precommand_stream_fault() {
    automatic_placement_retry("gra", false).await;
}

/// Empty canonical region is an accepted home Cell, not unselected placement.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_automatic_retry_pins_a_legacy_empty_accepted_region_to_home() {
    automatic_placement_retry("", true).await;
}

async fn automatic_placement_retry(accepted_region: &str, pending: bool) {
    let sb = Sandbox::new(&format!("auto-retry-{accepted_region}-{pending}"));
    project(&sb.dir);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let first = format!("{ENV}/leases/first");
    let second = format!("{ENV}/leases/second");
    let retry_region = if accepted_region.is_empty() {
        "fsn1"
    } else {
        accepted_region
    };
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let mut replies = vec![json_reply(json!({
        "name": first,
        "spec": {"region": accepted_region},
        "status": {"state": if pending { "granted" } else { "ready" },
            "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}
    }))];
    if !pending {
        replies.push(json_reply(json!({"token": "h.p.s", "generation": 1})));
        replies.push((500, "text/plain", b"upload stream broke".to_vec()));
    }
    // No spec in the loss response: affinity must already come from CreateLease.
    replies.push(json_reply(json!({"name": first,
        "status": {"state": "ended", "endReason": "machine_lost"}})));
    replies.extend([
        json_reply(json!({"name": second, "spec": {"region": retry_region},
            "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}})),
        json_reply(json!({"token": "h.p.t", "generation": 1})),
        json_reply(json!({})),
        json_reply(json!({})),
        envd_process("", 0),
        envd_process("built once\n", 0),
        json_reply(json!({"name": second, "status": {"state": "ending"}})),
    ]);
    let (_, log) = serve_bytes_on(listener, replies).await;
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "-o",
            "json",
            "--no-cache",
            "--",
            "cargo",
            "check",
        ],
    )
    .await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let events: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "queued" && e["retry"] == true)
            .count(),
        1
    );
    let running = events.iter().find(|e| e["type"] == "running").unwrap();
    assert_eq!(running["region"], retry_region);
    assert_eq!(running["volume"], false);
    assert_eq!(running["workspace"], "cold");
    assert_eq!(events.last().unwrap()["exit_code"], 0);
    let seen = log.lock();
    assert!(
        !seen.iter().any(|s| s.line.contains("/volumes")),
        "automatic retries must stay ephemeral"
    );
    let creates: Vec<Value> = seen
        .iter()
        .filter(|s| s.line.starts_with(&format!("POST /v1/{ENV}/leases ")))
        .map(|s| serde_json::from_str(&s.body).unwrap())
        .collect();
    assert_eq!(creates.len(), 2, "one initial lease and one retry");
    assert!(creates[0]["spec"]
        .get("region")
        .is_none_or(|r| r.as_str() == Some("")));
    assert_eq!(
        creates[1]["spec"]["region"], retry_region,
        "the retry must pin the accepted Cell"
    );
    for lease in &creates {
        assert!(
            lease["spec"]
                .get("volumes")
                .is_none_or(|v| v.as_array().is_some_and(|a| a.is_empty())),
            "{lease}"
        );
    }
    assert!(seen
        .iter()
        .any(|s| s.line.starts_with(&format!("GET /v1/{first} "))));
    let commands: Vec<_> = seen
        .iter()
        .filter(|s| s.body.contains("\"cargo\""))
        .collect();
    assert_eq!(
        commands.len(),
        1,
        "the lost attempt never started the command"
    );
    assert_eq!(
        seen.iter().filter(|s| s.line.contains(":release")).count(),
        1
    );
    assert!(seen
        .last()
        .unwrap()
        .line
        .contains(&format!("/{second}:release")));
}

/// An unavailable region refuses the volume-free lease immediately: 125,
/// not retryable, and no workspace request or allocation is attempted.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_region_not_offered_exits_125() {
    let sb = Sandbox::new("build-run-no-region");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let no_cell = json!({"code": "SHAPE_NOT_OFFERED", "status": 422, "detail": "`spec.region`: region `gra` has no Sandboxes Cell", "retryable": false});
    let (url, log) = serve(vec![(422, no_cell)]).await;
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
    assert_volume_free_regional_leases(&log, "gra");
    assert_eq!(log.lock().len(), 1, "no retry, no wait, no Volume request");
}

/// Explicit regional placement executes cold without consulting Volume APIs.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_in_an_explicit_region_uses_a_cold_owned_workspace() {
    let sb = Sandbox::new("build-run-no-volumes");
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("main.rs"), "fn main() {}\n").unwrap();
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let lease = format!("{ENV}/leases/l1");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let replies = vec![
        json_reply(
            json!({"name": lease, "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}}),
        ),
        json_reply(json!({"token": "h.p.s", "generation": 1})),
        json_reply(json!({})),
        json_reply(json!({})),
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
    assert_eq!(sync["volume"], false, "{sync}");
    assert_eq!(sync["full"], true, "{sync}");
    let last = events.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["exit_code"], 0, "{last}");

    assert_volume_free_regional_leases(&log, "gra");
    let seen = log.lock();
    let created: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(created["spec"]["shape"], "build-large", "{created}");
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
        &[
            "build",
            "run",
            "--region",
            "hk",
            "--no-cache",
            "--",
            "cargo",
            "build",
        ],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{err}");
    assert!(err.contains("Connection timed out"), "{err}");
    assert!(err.contains("[retryable]"), "{err}");
    assert_volume_free_regional_leases(&log, "hk");
    let seen = log.lock();
    let lines: Vec<&str> = seen.iter().map(|s| s.line.as_str()).collect();
    assert!(lines.last().unwrap().contains(":release"), "{lines:?}");
}

/// A process stream that breaks after the command started (a proxy on the way
/// restarted) is reattached to the same process: no second start, no second
/// machine, and the command's own exit code.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_reattaches_a_broken_stream_to_the_running_command() {
    let sb = Sandbox::new("stream-reattach");
    project(&sb.dir);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let mut replies = attempt_replies(&url);
    replies.push(envd_broken());
    replies.push(json_reply(json!({"token": "h.p.t", "generation": 1}))); // a fresh guest token
    replies.push(envd_process("after the break\n", 101)); // Connect by tag
    replies.push(envd_process_err("", 0)); // durable admission query, no start yet
    replies.push(ended());
    let (_, log) = serve_bytes_on(listener, replies).await;
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "--region",
            "hk",
            "--no-cache",
            "--",
            "cargo",
            "test",
        ],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(101), "{err}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "after the break\n");
    assert!(err.contains("reattached to the running command"), "{err}");
    assert!(!err.contains("retrying"), "{err}");
    assert_volume_free_regional_leases(&log, "hk");
    let seen = log.lock();
    let start = seen
        .iter()
        .find(|s| s.body.contains("\"cargo\""))
        .expect("the command was started");
    assert!(
        start.line.contains("process.Process/Start"),
        "{}",
        start.line
    );
    let tag = start
        .body
        .split("\"tag\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    assert!(tag.starts_with("sylphx-"), "{tag}");
    let connect = seen
        .iter()
        .find(|s| s.line.contains("process.Process/Connect"))
        .expect("the run reattached");
    assert!(
        connect
            .body
            .contains(&format!("{{\"process\":{{\"tag\":\"{tag}\"}}}}")),
        "{}",
        connect.body
    );
    assert_eq!(
        seen.iter().filter(|s| s.body.contains("\"cargo\"")).count(),
        1,
        "started once"
    );
    assert_eq!(
        seen.iter().filter(|s| s.line.contains(":release")).count(),
        1,
        "one machine"
    );
}

/// Detached output is not replayed. Only durable command admission can
/// replace the setup deadline or classify a marker-free exit as user status.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_recovers_lost_start_marker_without_restarting_or_resetting_timeout() {
    for (name, state, code, expected, running) in [
        ("expired", "100.00\n102.00\n", 42, 124, true),
        ("completed", "100.00\n100.20\n", 125, 125, true),
        ("setup-failed", "", 0, 125, false),
    ] {
        let sb = Sandbox::new(&format!("lost-marker-{name}"));
        project(&sb.dir);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let mut replies = attempt_replies(&url);
        replies.push(envd_broken());
        replies.push((
            200,
            "application/json",
            json!({"token": "h.p.t", "generation": 1})
                .to_string()
                .into_bytes(),
        ));
        replies.push(envd_process_err("", code)); // Connect, marker lost
        replies.push(envd_process(state, 0)); // durable monotonic admission state
        replies.push(ended());
        let (_, log) = serve_bytes_on(listener, replies).await;
        let out = run(
            &sb,
            &url,
            &[
                "build",
                "run",
                "-o",
                "json",
                "--timeout",
                "1s",
                "--region",
                "hk",
                "--no-cache",
                "--",
                "sh",
                "-c",
                "exit 42",
            ],
        )
        .await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(expected), "{name}: {err}");
        let events: Vec<Value> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(
            events.iter().filter(|e| e["type"] == "running").count(),
            usize::from(running),
            "{name}: {events:?}"
        );
        let result = events.last().unwrap();
        assert_eq!(result["exit_code"], expected);
        assert_eq!(result.get("error").is_some(), !running, "{name}: {result}");
        let seen = log.lock();
        assert_eq!(
            seen.iter().filter(|s| s.body.contains("exit 42")).count(),
            1,
            "command is never replayed"
        );
        assert_eq!(
            seen.iter().filter(|s| s.line.contains(":release")).count(),
            1
        );
    }
}

/// A machine lost after the command started ends the run with 137: the
/// command is never run again, and 125 stays for failures before it starts.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_exits_137_when_the_machine_is_lost_after_the_command_started() {
    let sb = Sandbox::new("stream-lost");
    project(&sb.dir);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let mut replies = attempt_replies(&url);
    replies.push(envd_broken());
    replies.push(json_reply(json!({"token": "h.p.t", "generation": 1})));
    // Connect: envd no longer knows the process.
    replies.push((
        200,
        "application/connect+json",
        envd_frame(
            2,
            json!({"error": {"code": "not_found", "message": "process not found"}}),
        ),
    ));
    replies.push(json_reply(
        json!({"name": format!("{ENV}/leases/l1"), "status": {"state": "ended", "endReason": "machine_lost"}}),
    ));
    replies.push(ended());
    let (_, log) = serve_bytes_on(listener, replies).await;
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "--region",
            "hk",
            "-o",
            "json",
            "--no-cache",
            "--",
            "cargo",
            "test",
        ],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(137), "{err}");
    assert!(
        err.contains("machine_lost (lease l1) after the command started"),
        "{err}"
    );
    let result: Value = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "result")
        .expect("a result event");
    assert_eq!(result["lease"], format!("{ENV}/leases/l1"));
    assert_eq!(result["outcome"], "lost");
    assert_eq!(result["exit_code"], 137);
    assert_eq!(result["retryable"], true);
    assert_volume_free_regional_leases(&log, "hk");
    let seen = log.lock();
    assert_eq!(
        seen.iter().filter(|s| s.body.contains("\"cargo\"")).count(),
        1,
        "never run again"
    );
}

/// Running out of space before command start retries on a fresh lease-owned
/// workspace, without reusing, creating, or emptying a persistent Volume.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_retries_a_full_workspace_once_on_a_fresh_lease() {
    let sb = Sandbox::new("ws-full-retry");
    project(&sb.dir);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut replies = attempt_replies(&url);
    *replies.last_mut().unwrap() = envd_process_err("gzip: stdout: No space left on device\n", 1);
    replies.push(ended()); // the full machine is released before the retry
    let mut retry = attempt_replies(&url);
    let mut lease: Value = serde_json::from_slice(&retry[0].2).unwrap();
    lease["name"] = json!(format!("{ENV}/leases/l2"));
    retry[0].2 = lease.to_string().into_bytes();
    replies.extend(retry);
    replies.push(envd_process("built\n", 0));
    replies.push(ended());
    let (_, log) = serve_bytes_on(listener, replies).await;
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "--region",
            "hk",
            "--no-cache",
            "--",
            "cargo",
            "test",
        ],
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "built\n");
    assert!(err.contains("ran out of space; retrying once"), "{err}");
    assert_volume_free_regional_leases(&log, "hk");
    let seen = log.lock();
    let leases: Vec<Value> = seen
        .iter()
        .filter(|s| s.line.starts_with(&format!("POST /v1/{ENV}/leases ")))
        .map(|s| serde_json::from_str(&s.body).unwrap())
        .collect();
    assert_eq!(leases.len(), 2);
    assert_eq!(
        seen.iter().filter(|s| s.body.contains("\"cargo\"")).count(),
        1
    );
    assert_eq!(
        seen.iter().filter(|s| s.line.contains(":release")).count(),
        2
    );
}

/// A machine that stays queued past --queue-timeout: the lease is released
/// and the run answers 125, retryable, in about that long, not 30 minutes.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_queue_timeout_releases_and_exits_125() {
    queued_build_timeout("run").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn build_image_queue_timeout_never_reserves_a_workspace_volume() {
    queued_build_timeout("image").await;
}

async fn queued_build_timeout(subcommand: &str) {
    let sb = Sandbox::new(&format!("build-{subcommand}-queue-timeout"));
    git_init(&sb.dir);
    std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
    let lease = format!("{ENV}/leases/l1");
    let queued = json!({"name": lease, "spec": {"region": "hk"}, "status": {"state": "granted"}});
    let (url, log) = serve(vec![
        (200, queued.clone()),
        (200, queued.clone()),
        (200, queued.clone()),
        (200, queued.clone()),
        (200, queued),
    ])
    .await;
    let t = std::time::Instant::now();
    let mut args = vec![
        "build",
        subcommand,
        "-o",
        "json",
        "--region",
        "hk",
        "--queue-timeout",
        "3s",
    ];
    if subcommand == "run" {
        args.extend(["--fresh", "--", "true"]);
    }
    let out = run(&sb, &url, &args).await;
    assert!(
        t.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        t.elapsed()
    );
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["retryable"], true, "{last}");
    let error = last["error"].as_str().unwrap();
    assert!(error.contains("--queue-timeout"), "{last}");
    assert!(
        error.contains(&lease),
        "the failure names the lease: {last}"
    );
    assert_volume_free_regional_leases(&log, "hk");
    let seen = log.lock();
    assert!(
        seen.iter().all(|s| s.line.contains("/leases")),
        "queued work must not contact workspace or guest APIs"
    );
    assert!(
        seen.iter().any(|s| s.line.contains(":release")),
        "the queued lease is released"
    );
}

fn pending_build_machine() -> Value {
    json!({"code": "IDEMPOTENCY_IN_PROGRESS", "status": 409,
        "detail": "The first request with this Idempotency-Key is still running.",
        "retryable": true})
}

fn assert_same_machine_request(log: &Log, expected: usize) {
    let seen = log.lock();
    let creates: Vec<_> = seen
        .iter()
        .filter(|s| s.line.starts_with(&format!("POST /v1/{ENV}/leases ")))
        .collect();
    assert_eq!(creates.len(), expected);
    let key = |s: &Seen| {
        s.head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("idempotency-key:")
                    .map(|value| value.trim().to_owned())
            })
            .unwrap()
    };
    assert!(!key(creates[0]).is_empty());
    for request in &creates {
        assert_eq!(key(request), key(creates[0]));
        assert_eq!(request.body, creates[0].body);
    }
}

/// Pending observations exceed the SDK's three transient retries and still
/// return the one accepted lease, then execute the command exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_pending_machine_attaches_and_succeeds() {
    let sb = Sandbox::new("build-pending-success");
    git_init(&sb.dir);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut replies = vec![
        (
            409,
            "application/json",
            pending_build_machine().to_string().into_bytes()
        );
        4
    ];
    replies.extend(attempt_replies(&url));
    replies.extend([envd_process("attached\n", 0), ended()]);
    let (_, log) = serve_bytes_on(listener, replies).await;
    let out = run(&sb, &url, &["build", "run", "--no-cache", "--", "true"]).await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "attached\n");
    assert_same_machine_request(&log, 5);
    assert_eq!(
        log.lock()
            .iter()
            .filter(|s| s.line.contains(":release"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_pending_machine_surfaces_its_terminal_failure() {
    let sb = Sandbox::new("build-pending-failure");
    git_init(&sb.dir);
    let (url, log) = serve(vec![
        (409, pending_build_machine()),
        (
            422,
            json!({"code": "SHAPE_NOT_OFFERED", "status": 422,
            "detail": "build-large is unavailable in this region", "retryable": false}),
        ),
    ])
    .await;
    let out = run(&sb, &url, &["build", "run", "--no-cache", "--", "true"]).await;
    assert_eq!(out.status.code(), Some(125));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("build-large is unavailable in this region"),
        "{err}"
    );
    assert!(!err.contains("IDEMPOTENCY_IN_PROGRESS"), "{err}");
    assert_same_machine_request(&log, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_pending_machine_queue_timeout_is_retryable() {
    let sb = Sandbox::new("build-pending-timeout");
    git_init(&sb.dir);
    let (url, log) = serve(vec![(409, pending_build_machine()); 8]).await;
    let start = std::time::Instant::now();
    let out = run(
        &sb,
        &url,
        &[
            "build",
            "run",
            "-o",
            "json",
            "--queue-timeout",
            "2s",
            "--no-cache",
            "--",
            "true",
        ],
    )
    .await;
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(result["retryable"], true, "{result}");
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("--queue-timeout 2s"), "{result}");
    assert!(
        error.contains("accepted request is still running"),
        "{result}"
    );
    assert!(!error.contains("IDEMPOTENCY_IN_PROGRESS"), "{result}");
    let count = log.lock().len();
    assert_same_machine_request(&log, count);
    assert!(log
        .lock()
        .iter()
        .all(|s| s.line.starts_with(&format!("POST /v1/{ENV}/leases "))));
}

/// A retryable 503 while the machine is being obtained (a stale access
/// snapshot): the SDK's retries of the one create call are bounded by
/// --queue-timeout, and the run then exits 125 with the server's own
/// message, retryable — it neither hangs past the queue budget nor loses
/// the answer to a deadline error.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_stale_snapshot_exits_125_with_the_server_message() {
    stale_snapshot_exits_125("run").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_no_tree_stale_snapshot_exits_125_with_the_server_message() {
    stale_snapshot_exits_125("no-tree").await;
}

async fn stale_snapshot_exits_125(subcommand: &str) {
    let sb = Sandbox::new(&format!("build-{subcommand}-stale-snapshot"));
    let problem = json!({
        "type": "about:blank", "title": "access_snapshot_stale", "status": 503,
        "detail": "the access snapshot is stale; retry once it refreshes",
        "code": "access_snapshot_stale", "grpc_status": "UNAVAILABLE",
        "retryable": true, "effect": "none",
    });
    let (url, log) = serve(vec![(503, problem); 16]).await;
    let t = std::time::Instant::now();
    let mut args = vec!["build", "run", "-o", "json", "--queue-timeout", "3s"];
    if subcommand == "run" {
        git_init(&sb.dir);
        std::fs::write(sb.dir.join("a.txt"), "a\n").unwrap();
        args.extend(["--fresh", "--", "true"]);
    } else {
        args.extend(["--no-tree", "--", "true"]);
    }
    let out = run(&sb, &url, &args).await;
    assert!(
        t.elapsed() < std::time::Duration::from_secs(30),
        "the queue budget bounds the SDK's retries: {:?}",
        t.elapsed()
    );
    assert_eq!(out.status.code(), Some(125));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["retryable"], true, "{last}");
    assert!(
        last["error"]
            .as_str()
            .unwrap()
            .contains("the access snapshot is stale; retry once it refreshes"),
        "the server's message reaches the caller: {last}"
    );
    let seen = log.lock();
    assert!(
        (2..=4).contains(&seen.len()),
        "one logical create, retried by the SDK (no fresh mutation): {} requests",
        seen.len()
    );
    let keys: Vec<_> = seen
        .iter()
        .filter_map(|s| {
            s.head.lines().find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("idempotency-key:")
                    .map(|v| v.trim().to_string())
            })
        })
        .collect();
    assert!(
        !keys.is_empty() && keys.iter().all(|k| k == &keys[0]),
        "every retry carries the one logical call's idempotency key: {keys:?}"
    );
}

/// [`serve_bytes`] on a known address (the guest's URI must be known before
/// the replies are built).
async fn serve_bytes_at(url: &str, replies: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Log) {
    let addr = url.trim_start_matches("http://").to_string();
    serve_bytes_on(TcpListener::bind(addr).await.unwrap(), replies).await
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

/// Run the CLI from another directory inside the sandbox, keeping the
/// sandbox's project and configuration.
async fn run_in_with(
    sb: &Sandbox,
    dir: &std::path::Path,
    url: &str,
    args: &[&str],
    tweak: impl FnOnce(&mut Command),
) -> std::process::Output {
    let mut c = sb.cmd(url, args);
    c.current_dir(dir);
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

/// The sandbox and guest stub up to command start: a volume-free ready lease,
/// its token, and a cold workspace sync.
/// `tail` is what happens from the command on.
async fn build_stub(tail: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Log) {
    build_stub_at("hk", tail).await
}

/// [`build_stub`] with the lease's accepted region. The cache a run uses is
/// the Cell it actually landed in, not the region it asked for, so a test
/// that pins the home region needs the home spelling here.
async fn build_stub_at(region: &str, tail: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut replies = attempt_replies_at(&url, region);
    replies.extend(tail);
    serve_bytes_on(listener, replies).await
}

/// One attempt: a ready lease, its token, source uploads and sync apply.
fn attempt_replies(url: &str) -> Vec<(u16, &'static str, Vec<u8>)> {
    attempt_replies_at(url, "hk")
}

fn attempt_replies_at(url: &str, region: &str) -> Vec<(u16, &'static str, Vec<u8>)> {
    let json_reply = |v: Value| (200u16, "application/json", v.to_string().into_bytes());
    let lease = format!("{ENV}/leases/l1");
    vec![
        json_reply(
            json!({"name": lease, "spec": {"region": region}, "status": {"state": "ready", "endpoints": {"guestUri": url, "e2bSandboxId": "ABCD1234"}}}),
        ),
        json_reply(json!({"token": "h.p.s", "generation": 1})),
        json_reply(json!({})),
        json_reply(json!({})),
        envd_process("", 0),
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
    let mut replies = attempt_replies(&url);
    replies.push(provision);
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

/// Environment of the user action, independent of sync call ordering.
fn started_env(log: &Log) -> Value {
    let seen = log.lock();
    let start = seen
        .iter()
        .find(|request| {
            request.line.starts_with("POST /process.Process/Start")
                && request.body.contains("sylphx-run")
        })
        .expect("the command starts");
    assert!(start.line.starts_with("POST /process.Process/Start"));
    let text = &start.body;
    let from = text.find("{\"process\"").expect("a JSON message");
    let end = text.rfind('}').unwrap();
    let v: Value = serde_json::from_str(&text[from..=end]).unwrap();
    v["process"]["envs"].clone()
}

/// Every regional Build fixture leases without any persistent workspace API.
fn assert_volume_free_regional_leases(log: &Log, region: &str) {
    let seen = log.lock();
    let mut leases = 0;
    for request in seen.iter() {
        assert!(!request.line.contains("/volumes"), "{}", request.line);
        if request.line.starts_with(&format!("POST /v1/{ENV}/leases ")) {
            let body: Value = serde_json::from_str(&request.body).unwrap();
            assert_eq!(body["spec"]["region"], region, "{body}");
            assert!(
                body["spec"]
                    .get("volumes")
                    .is_none_or(|v| v.as_array().is_some_and(|a| a.is_empty())),
                "{body}"
            );
            leases += 1;
        }
    }
    assert!(
        leases > 0,
        "the fixture must request a volume-free regional lease"
    );
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
    let (url, log) = build_stub_at("fsn1", vec![envd_process("built\n", 0), ended()]).await;
    let out = run_with(
        &sb,
        &url,
        &[
            "build",
            "run",
            "--region",
            "fsn1",
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
    assert_volume_free_regional_leases(&log, "fsn1");
    // The mint: the key as a bearer, and the run's access, network and life.
    let seen = clog.lock();
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
        json!({"project": "prj_a", "access": "write", "ttl_seconds": 3300, "network": "cluster"})
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
async fn build_run_refreshes_the_capped_token_after_ready_for_the_longest_run() {
    let sb = Sandbox::new("cache-ttl");
    project(&sb.dir);
    let refreshed_token = "cache-refreshed-token-do-not-print";
    let mut refreshed = cache_reply();
    refreshed["token"] = json!(refreshed_token);
    refreshed["env"]["SCCACHE_WEBDAV_TOKEN"] = json!(refreshed_token);
    refreshed["env"]["TURBO_TOKEN"] = json!(refreshed_token);
    let (cache, clog) = serve(vec![(200, cache_reply()), (200, refreshed)]).await;
    let (url, log) = build_stub_at("fsn1", vec![envd_process("", 0), ended()]).await;
    let out = run_with(
        &sb,
        &url,
        &[
            "build",
            "run",
            "--region",
            "fsn1",
            "--timeout",
            "6h",
            "--",
            "make",
        ],
        |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    assert_eq!(out.status.code(), Some(0));
    assert_volume_free_regional_leases(&log, "fsn1");
    let seen = clog.lock();
    assert_eq!(
        seen.len(),
        2,
        "the capped pre-READY token must be refreshed"
    );
    for mint in seen.iter() {
        let body: Value = serde_json::from_str(&mint.body).unwrap();
        assert_eq!(
            body,
            json!({"project": "prj_a", "access": "write", "ttl_seconds": 22500, "network": "cluster"})
        );
    }
    assert_eq!(started_env(&log)["SCCACHE_WEBDAV_TOKEN"], refreshed_token);
    assert_eq!(started_env(&log)["TURBO_TOKEN"], refreshed_token);
    for secret in [CACHE_TOKEN, refreshed_token] {
        assert!(!String::from_utf8_lossy(&out.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&out.stderr).contains(secret));
    }
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
        let out = run_with(
            &sb,
            &url,
            &["build", "run", "--region", "hk", "--", "make"],
            |c| {
                c.env("SYLPHX_BUILD_CACHE_URL", &cache);
            },
        )
        .await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{name}: {err}");
        assert_volume_free_regional_leases(&log, "hk");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n", "{name}");
        let warn: Vec<&str> = err.lines().filter(|l| l.contains("build cache")).collect();
        assert_eq!(
            warn,
            [format!(
                "sylphx: warning: build cache unavailable ({reason}); building without it"
            )],
            "{name}: {err}"
        );
        assert!(
            err.contains("accepted build machine (lease l1)"),
            "{name}: {err}"
        );
        assert!(err.contains("exit 0 (lease l1)"), "{name}: {err}");
        assert!(sylphx_lines(&err).len() <= 4, "{name}: {err}");
        let env = started_env(&log);
        assert!(
            env.as_object()
                .unwrap()
                .keys()
                .all(|key| !key.starts_with("SCCACHE_")
                    && !key.starts_with("TURBO_")
                    && key != "SYLPHX_CRATES_MIRROR"),
            "{name}: {env}"
        );
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
        &["build", "run", "--region", "hk", "--no-cache", "--", "make"],
        |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_volume_free_regional_leases(&log, "hk");
    assert!(clog.lock().is_empty(), "no token was asked for");
    assert!(!err.contains("build cache"), "{err}");
    assert!(err.contains("accepted build machine (lease l1)"), "{err}");
    assert!(err.contains("exit 0 (lease l1)"), "{err}");
    assert_eq!(sylphx_lines(&err).len(), 4, "{err}");
    let env = started_env(&log);
    assert!(
        env.as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("SCCACHE_")
                && !key.starts_with("TURBO_")
                && key != "SYLPHX_CRATES_MIRROR"),
        "{env}"
    );
}

/// An error from the machine that happens to repeat the token does not
/// carry it out.
#[tokio::test(flavor = "multi_thread")]
async fn build_run_errors_never_carry_the_token() {
    let sb = Sandbox::new("cache-scrub");
    project(&sb.dir);
    let (cache, _) = serve(vec![(200, cache_reply())]).await;
    let echo = format!("boom {CACHE_TOKEN}").into_bytes();
    let (url, log) = build_stub(vec![
        (500, "text/plain", echo),
        (
            200,
            "application/json",
            json!({"name": format!("{ENV}/leases/l1"), "spec": {"region": "hk"}, "status": {"state": "ready"}})
                .to_string()
                .into_bytes(),
        ),
        ended(),
    ])
    .await;
    let out = run_with(
        &sb,
        &url,
        &["build", "run", "--region", "hk", "--", "make"],
        |c| {
            c.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(125), "{err}");
    assert_volume_free_regional_leases(&log, "hk");
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
    let seen = clog.lock();
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
    let body: Value = serde_json::from_str(&clog.lock()[0].body).unwrap();
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
async fn build_cache_env_bootstraps_the_default_project_and_needs_a_key() {
    // No linked project and a key that names only an org: the standard default
    // project is discovered through Access, and the cache is read for it.
    let sb = Sandbox::new("cache-env-noproject");
    std::fs::remove_file(sb.dir.join(".sylphx/project.json")).unwrap();
    let (api, api_log) = serve(vec![
        (200, json!({"org": "orgs/org_a"})),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
        ),
    ])
    .await;
    let (cache, clog) = serve(vec![(200, cache_reply())]).await;
    let out = run_with(&sb, &api, &["build", "cache", "env", "-o", "json"], |c| {
        c.env("SYLPHX_BUILD_CACHE_URL", &cache);
    })
    .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_str(&clog.lock()[0].body).unwrap();
    assert_eq!(body["project"], "prj_a", "{body}");
    let seen = api_log.lock().clone();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[0].line.starts_with("GET /v1/whoami "),
        "{}",
        seen[0].line
    );
    assert!(
        seen[1].line.starts_with("GET /v1/orgs/org_a/projects "),
        "{}",
        seen[1].line
    );
    // Signed out: a platform failure, as for `build run`.
    let out = run_with(&sb, "http://127.0.0.1:9", &["build", "cache", "env"], |c| {
        c.env_remove("SYLPHX_API_KEY");
    })
    .await;
    assert_eq!(out.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not signed in"));
}

fn source_reply(repository: &str) -> Value {
    let git_url = format!("https://cache.test/git/org_a/{repository}.git");
    json!({
        "token": "source-session-test-token",
        "expires_at": 2000000000,
        "git_url": git_url,
        "read_url": "https://cache.test/v1/source:read",
        "env": {
            "CARGO_NET_GIT_FETCH_WITH_CLI": "true",
            "GIT_CONFIG_COUNT": "5",
            "GIT_CONFIG_KEY_0": "url.https://cache.test/git/org_a/.insteadOf",
            "GIT_CONFIG_VALUE_0": "https://github.com/",
            "GIT_CONFIG_KEY_1": format!("http.{git_url}.extraheader"),
            "GIT_CONFIG_VALUE_1": "Authorization: Bearer source-session-test-token",
            "GIT_CONFIG_KEY_2": format!("http.https://cache.test/git/org_a/{repository}.extraheader"),
            "GIT_CONFIG_VALUE_2": "Authorization: Bearer source-session-test-token",
            "GIT_CONFIG_KEY_3": "url.https://github.com/.pushInsteadOf",
            "GIT_CONFIG_VALUE_3": "https://github.com/",
            "GIT_CONFIG_KEY_4": "url.https://github.com/.pushInsteadOf",
            "GIT_CONFIG_VALUE_4": "https://cache.test/git/org_a/",
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn build_source_env_works_before_clone_and_preserves_github_push() {
    let sb = Sandbox::new("source-before-clone");
    assert!(!sb.dir.join(".git").exists());
    let (cache, log) = serve(vec![(200, source_reply("acme/repo"))]).await;
    let out = run_with(
        &sb,
        "http://127.0.0.1:9",
        &[
            "build",
            "source",
            "env",
            "acme/repo",
            "--project",
            "prj_other",
            "--trust-policy",
            "orgs/org_a/broker_trust_policies/source",
            "-o",
            "json",
        ],
        |command| {
            command.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env: BTreeMap<String, String> = serde_json::from_slice(&out.stdout).unwrap();
    let seen = log.lock();
    assert!(seen[0].line.starts_with("POST /v1/source-sessions "));
    assert!(seen[0]
        .head
        .to_ascii_lowercase()
        .contains("authorization: bearer sylphx_sk_test"));
    assert_eq!(
        serde_json::from_str::<Value>(&seen[0].body).unwrap(),
        json!({
            "project": "prj_other", "repository": "acme/repo", "network": "public",
            "trust_policy": "orgs/org_a/broker_trust_policies/source",
        })
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("source-session-test-token"));
    drop(seen);
    git_init(&sb.dir);
    for upstream in [
        "https://github.com/acme/repo",
        "git@github.com:acme/repo.git",
        "https://github.com/acme/repo-other",
        "ssh://git@github.com/acme/repo-other.git",
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&sb.dir)
            .args(["config", "remote.origin.url", upstream])
            .status()
            .unwrap()
            .success());
        for push in [false, true] {
            let mut git = Command::new("git");
            git.arg("-C").arg(&sb.dir).args(["remote", "get-url"]);
            if push {
                git.arg("--push");
            }
            let result = git.arg("origin").envs(&env).output().unwrap();
            assert!(result.status.success());
            let url = String::from_utf8(result.stdout).unwrap();
            if push {
                assert!(url.starts_with("https://github.com/acme/"), "{url}");
                assert_eq!(
                    url.trim_end().ends_with("repo-other.git"),
                    upstream.ends_with("repo-other.git")
                );
            } else {
                let suffix = upstream
                    .strip_prefix("https://github.com/")
                    .or_else(|| upstream.strip_prefix("git@github.com:"))
                    .or_else(|| upstream.strip_prefix("ssh://git@github.com/"))
                    .unwrap();
                assert_eq!(url.trim(), format!("https://cache.test/git/org_a/{suffix}"));
            }
            assert!(!url.contains("source-session-test-token"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn build_source_env_missing_or_denied_authority_never_switches_identity() {
    for status in [401, 403] {
        let sb = Sandbox::new(&format!("source-denied-{status}"));
        let (cache, log) = serve(vec![(
            status,
            json!({"error": {"code": "PERMISSION_DENIED", "message": "missing build:read sylphx_sk_test\n", "request_id": "req_body_sylphx_sk_test"}}),
        )])
        .await;
        let out = run_with(
            &sb,
            "http://127.0.0.1:9",
            &["build", "source", "env", "acme/repo", "--project", "prj_a"],
            |command| {
                command.env("SYLPHX_BUILD_CACHE_URL", &cache);
            },
        )
        .await;
        assert_eq!(out.status.code(), Some(125));
        assert!(out.stdout.is_empty());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("PERMISSION_DENIED: missing build:read [redacted]"),
            "{error}"
        );
        assert!(error.contains("request_id=req_body_[redacted]"), "{error}");
        assert!(!error.contains("sylphx_sk_test"));
        assert_eq!(log.lock().len(), 1);
    }
    let sb = Sandbox::new("source-signed-out");
    let out = run_with(
        &sb,
        "http://127.0.0.1:9",
        &["build", "source", "env", "acme/repo", "--project", "prj_a"],
        |command| {
            command.env_remove("SYLPHX_API_KEY");
        },
    )
    .await;
    assert_eq!(out.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not signed in"));
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_derives_origin_and_locked_sources_using_accepted_lease() {
    let sb = Sandbox::new("source-build-run");
    project(&sb.dir);
    assert!(Command::new("git")
        .arg("-C")
        .arg(&sb.dir)
        .args([
            "config",
            "remote.origin.url",
            "https://github.com/acme/repo.git"
        ])
        .status()
        .unwrap()
        .success());
    std::fs::write(sb.dir.join("Cargo.lock"), r#"version = 4
[[package]]
name = "dependency"
version = "1.0.0"
source = "git+ssh://git@github.com/acme/dependency?rev=main#0123456789012345678901234567890123456789"
"#).unwrap();
    let (cache, requests) = serve(vec![
        (200, cache_reply()),
        (200, source_reply("acme/dependency")),
        (200, source_reply("acme/repo")),
    ])
    .await;
    use base64::Engine;
    let mut stream = envd_frame(0, json!({"event": {"start": {"pid": 7}}}));
    let marker = base64::engine::general_purpose::STANDARD
        .encode(sylphx_build_lease::scripts::COMMAND_STARTED);
    stream.extend(envd_frame(
        0,
        json!({"event": {"data": {"stderr": marker}}}),
    ));
    for (channel, chunk) in [
        (
            "stdout",
            "built https://github.com/acme/repo source-session-",
        ),
        ("stderr", "Authorization: Bearer source-session-"),
        ("stdout", "test-token done\n"),
        ("stderr", "test-token safe\n"),
    ] {
        let data = base64::engine::general_purpose::STANDARD.encode(chunk);
        stream.extend(envd_frame(0, json!({"event": {"data": {channel: data}}})));
    }
    stream.extend(envd_frame(
        0,
        json!({"event": {"end": {"exitCode": 0, "exited": true, "status": "exit status 0"}}}),
    ));
    stream.extend(envd_frame(2, json!({})));
    let (api, log) = build_stub_at(
        "fsn1",
        vec![
            envd_process("", 0),
            (200, "application/json", b"{}".to_vec()),
            envd_process("", 0),
            (200, "application/connect+json", stream),
            envd_process("", 0),
            ended(),
        ],
    )
    .await;
    // Requested gra, returned fsn1: only the accepted lease selects cluster.
    let out = run_with(
        &sb,
        &api,
        &["build", "run", "--region", "gra", "--", "true"],
        |command| {
            command.env("SYLPHX_BUILD_CACHE_URL", &cache);
        },
    )
    .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = requests.lock();
    assert_eq!(requests.len(), 3);
    for (request, repository) in requests[1..].iter().zip(["acme/dependency", "acme/repo"]) {
        assert!(request.line.starts_with("POST /v1/source-sessions "));
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["network"], "cluster");
        assert_eq!(body["repository"], repository);
    }
    let env = started_env(&log);
    assert_eq!(env["CARGO_NET_GIT_FETCH_WITH_CLI"], "true");
    assert_eq!(env["GIT_CONFIG_COUNT"], "15");
    assert!(env.as_object().unwrap().values().all(|value| !value
        .as_str()
        .unwrap_or("")
        .contains("source-session-test-token")));
    assert!(log
        .lock()
        .iter()
        .any(|request| request.body.contains("source-auth-cleanup")));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("source-session-test-token"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("source-session-test-token"));
    assert!(String::from_utf8_lossy(&out.stdout)
        .contains("built https://github.com/acme/repo [redacted] done"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("[redacted] safe"));
    assert!(
        !sb.dir.join(".git/sylphx/git-deps").exists()
            && !sb.dir.join(".git/sylphx/tree-packs").exists(),
        "granted dependencies and known tree packs are not written under shared worktree metadata"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_source_grant_missing_preserves_other_repository_sessions() {
    // Exercise the denied result both before and after a granted dependency.
    for origin in ["acme/a-origin", "acme/z-origin"] {
        let sb = Sandbox::new(&format!("source-mixed-{}", origin.replace('/', "-")));
        project(&sb.dir);
        assert!(Command::new("git")
            .arg("-C")
            .arg(&sb.dir)
            .args([
                "config",
                "remote.origin.url",
                &format!("https://github.com/{origin}")
            ])
            .status()
            .unwrap()
            .success());
        std::fs::write(
            sb.dir.join("Cargo.lock"),
            r#"version = 4
[[package]]
name = "dependency"
version = "1.0.0"
source = "git+https://github.com/acme/private?rev=main#0123456789012345678901234567890123456789"
"#,
        )
        .unwrap();
        let granted = (200, source_reply("acme/private"));
        let denied = (403, json!({"error": {"code": "SOURCE_GRANT_MISSING"}}));
        let results = if origin < "acme/private" {
            [denied, granted]
        } else {
            [granted, denied]
        };
        let (cache, requests) = serve(
            std::iter::once((200, cache_reply()))
                .chain(results)
                .collect(),
        )
        .await;
        let (api, log) = build_stub_at(
            "fsn1",
            vec![
                envd_process("", 0),
                (200, "application/json", b"{}".to_vec()),
                envd_process("", 0),
                envd_process("built\n", 0),
                envd_process("", 0),
                ended(),
            ],
        )
        .await;
        let out = run_with(&sb, &api, &["build", "run", "--", "true"], |command| {
            command.env("SYLPHX_BUILD_CACHE_URL", &cache);
            // No local Git access to the private dependency: an accidental
            // fallback must fail immediately without making a network request.
            command.env("GIT_ALLOW_PROTOCOL", "file");
        })
        .await;
        let error = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{origin}: {error}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("built"));
        assert!(!error.contains("source cache unavailable"), "{error}");
        assert_eq!(requests.lock().len(), 3);
        let env = started_env(&log);
        assert_eq!(env["CARGO_NET_GIT_FETCH_WITH_CLI"], "true");
        assert_eq!(env["GIT_CONFIG_COUNT"], "8");
        assert!(log
            .lock()
            .iter()
            .any(|request| request.body.contains("source-auth-cleanup")));
        assert!(!error.contains("source-session-test-token"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_source_endpoint_absence_uses_existing_inputs_without_claiming_source_cache() {
    let sb = Sandbox::new("source-rollout");
    project(&sb.dir);
    assert!(Command::new("git")
        .arg("-C")
        .arg(&sb.dir)
        .args([
            "config",
            "remote.origin.url",
            "https://github.com/acme/repo"
        ])
        .status()
        .unwrap()
        .success());
    let (cache, requests) = serve(vec![
        (200, cache_reply()),
        (404, json!({"error": {"code": "NOT_FOUND"}})),
    ])
    .await;
    let (api, log) = build_stub(vec![envd_process("built\n", 0), ended()]).await;
    let out = run_with(&sb, &api, &["build", "run", "--", "true"], |command| {
        command.env("SYLPHX_BUILD_CACHE_URL", &cache);
    })
    .await;
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let error = String::from_utf8_lossy(&out.stderr);
    assert_eq!(error.matches("source cache unavailable").count(), 1);
    assert_eq!(requests.lock().len(), 2);
    assert!(started_env(&log).get("GIT_CONFIG_COUNT").is_none());
    assert!(!log
        .lock()
        .iter()
        .any(|request| request.body.contains("source-auth")));
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_source_authorization_denial_warns_and_uses_existing_sources() {
    for status in [401, 403] {
        let sb = Sandbox::new(&format!("source-run-denied-{status}"));
        project(&sb.dir);
        assert!(Command::new("git")
            .arg("-C")
            .arg(&sb.dir)
            .args([
                "config",
                "remote.origin.url",
                "https://github.com/acme/repo"
            ])
            .status()
            .unwrap()
            .success());
        let denial = json!({"error": {
            "code": "PERMISSION_DENIED",
            "message": "The key lacks build:read: sylphx_sk_test\n",
            "request_id": "req_body"
        }});
        let (cache, requests) = serve_bytes_headers_on(
            TcpListener::bind("127.0.0.1:0").await.unwrap(),
            vec![
                (
                    200,
                    "application/json",
                    cache_reply().to_string().into_bytes(),
                    "",
                ),
                (
                    status,
                    "application/json",
                    denial.to_string().into_bytes(),
                    "x-request-id: req_header_sylphx_sk_test\r\n",
                ),
            ],
        )
        .await;
        let (api, log) = build_stub(vec![envd_process("", 0), ended()]).await;
        let out = run_with(&sb, &api, &["build", "run", "--", "true"], |command| {
            command.env("SYLPHX_BUILD_CACHE_URL", &cache);
        })
        .await;
        let error = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{error}");
        assert!(error.contains(&format!("HTTP {status} PERMISSION_DENIED: The key lacks build:read: [redacted]; request_id=req_header_[redacted]")), "{error}");
        assert_eq!(
            error.matches("warning: source cache unavailable").count(),
            1
        );
        assert!(!error.contains("sylphx_sk_test"), "{error}");
        assert!(!error.contains("req_body"), "{error}");
        assert_eq!(requests.lock().len(), 2);
        assert!(log
            .lock()
            .iter()
            .any(|request| request.line.starts_with("POST /process.Process/Start")));
        assert!(started_env(&log).get("GIT_CONFIG_COUNT").is_none());
        assert!(!log
            .lock()
            .iter()
            .any(|request| request.body.contains("source-auth")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_respects_no_cache_and_external_source_environment() {
    for external in [false, true] {
        let sb = Sandbox::new(&format!("source-override-{external}"));
        project(&sb.dir);
        assert!(Command::new("git")
            .arg("-C")
            .arg(&sb.dir)
            .args([
                "config",
                "remote.origin.url",
                "https://github.com/acme/repo"
            ])
            .status()
            .unwrap()
            .success());
        let (cache, requests) = serve(if external {
            vec![(200, cache_reply())]
        } else {
            vec![]
        })
        .await;
        let (api, log) = build_stub(vec![envd_process("", 0), ended()]).await;
        let args = if external {
            vec![
                "build",
                "run",
                "--env",
                "GIT_CONFIG_COUNT=1",
                "--env",
                "GIT_CONFIG_KEY_0=url.https://external/repo.insteadOf",
                "--env",
                "GIT_CONFIG_VALUE_0=https://github.com/acme/repo",
                "--",
                "true",
            ]
        } else {
            vec!["build", "run", "--no-cache", "--", "true"]
        };
        let out = run_with(&sb, &api, &args, |command| {
            command.env("SYLPHX_BUILD_CACHE_URL", &cache);
        })
        .await;
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(requests.lock().len(), usize::from(external));
        let env = started_env(&log);
        if external {
            assert_eq!(env["GIT_CONFIG_COUNT"], "1");
            assert_eq!(
                env["GIT_CONFIG_KEY_0"],
                "url.https://external/repo.insteadOf"
            );
        } else {
            assert!(env.get("GIT_CONFIG_COUNT").is_none());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn build_run_caches_pinned_dependencies_outside_shared_worktree_metadata() {
    let sb = Sandbox::new("source-hub-linked-cache");
    project(&sb.dir);
    // A linked worktree shares its hub's Git directory, so anything written
    // under `.git/<name>` is shared by every worktree of that hub. The
    // dependency pack cache must resolve to per-source user storage instead.
    let hub = sb.dir.join("hub");
    let linked = sb.dir.join("linked");
    std::fs::create_dir_all(&hub).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .arg(&hub)
        .status()
        .unwrap()
        .success());
    std::fs::write(hub.join("README.md"), "hub\n").unwrap();
    assert!(Command::new("git")
        .arg("-C")
        .arg(&hub)
        .args(["add", "README.md"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&hub)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qm",
            "hub"
        ])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&hub)
        .args(["worktree", "add", "-q", linked.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&linked)
        .args([
            "config",
            "remote.origin.url",
            "https://github.com/acme/repo.git"
        ])
        .status()
        .unwrap()
        .success());
    // The locked dependency is a local repository, so packing it needs no network.
    let upstream = sb.dir.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git_init(&upstream);
    std::fs::write(upstream.join("lib.rs"), "pub fn dependency() {}\n").unwrap();
    for args in [
        vec!["add", "lib.rs"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-qm",
            "dependency",
        ],
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&upstream)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let locked = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&upstream)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    std::fs::write(
        linked.join("Cargo.lock"),
        format!(
            "version = 4\n[[package]]\nname = \"dependency\"\nversion = \"1.0.0\"\nsource = \"git+file://{}?rev=main#{}\"\n",
            upstream.display(),
            locked.trim()
        ),
    )
    .unwrap();
    let user_cache = sb.dir.join("user-cache");
    // After the sync: the dependency's tarball upload, then the command.
    let (api, _log) = build_stub(vec![
        (200, "application/json", b"{}".to_vec()),
        envd_process("", 0),
        ended(),
    ])
    .await;
    let output = run_in_with(
        &sb,
        &linked,
        &api,
        &["build", "run", "--no-cache", "--", "true"],
        |command| {
            command.env("XDG_CACHE_HOME", &user_cache);
        },
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The pinned dependency was packed locally, into this source's own user
    // cache, and nothing was written under the hub's shared Git directory.
    fn tarballs(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(tarballs(&path));
            } else if path.extension().is_some_and(|ext| ext == "tgz") {
                found.push(path);
            }
        }
        found
    }
    let packed = tarballs(&user_cache);
    assert_eq!(packed.len(), 1, "{packed:?}");
    assert!(packed[0].starts_with(&user_cache));
    assert!(!hub.join(".git/sylphx").exists());
    assert!(!hub.join(".git/worktrees/linked/sylphx").exists());
    assert!(!linked.join(".sylphx/git-deps").exists());
}

/// The test vector secret of the Standard Webhooks reference libraries; not a
/// credential.
const LISTEN_SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"; // gitleaks:allow

/// `sylphx events listen --forward URL`: a temporary Queue subscribed to the
/// env's bus, each leased event POSTed to the local receiver with a valid
/// Standard Webhooks signature, acked, and the Queue and Subscription deleted
/// on the way out.
#[tokio::test(flavor = "multi_thread")]
async fn events_listen_relays_a_published_event_signed_to_the_local_url() {
    use base64::Engine as _;
    let (url, log) = serve(vec![
        (200, json!({"name": format!("{ENV}/queues/q")})),
        (200, json!({"name": format!("{ENV}/subscriptions/s")})),
        (
            200,
            json!({"messages": [{
                "message_id": "01HMSG",
                "event": {"id": "evt-1", "source": "test", "type": "order.created",
                          "data": {"n": 1}},
                "lease": {"message_id": "01HMSG", "lease_generation": "3"},
                "delivery_count": 1
            }]}),
        ),
        (200, json!({})),
        (200, json!({})),
        (200, json!({})),
    ])
    .await;
    let (hook, received) = serve(vec![(204, json!({}))]).await;
    let sb = Sandbox::new("events-listen");
    let forward = format!("{hook}/hook");
    let out = run(
        &sb,
        &url,
        &[
            "events",
            "listen",
            "--forward",
            &forward,
            "--secret",
            LISTEN_SECRET,
            "--count",
            "1",
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

    // The receiver got the CloudEvent, signed under the secret.
    let got = received.lock()[0].clone();
    assert!(got.line.starts_with("POST /hook "), "{}", got.line);
    let header = |name: &str| {
        got.head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
            })
            .unwrap_or_default()
    };
    assert_eq!(header("webhook-id"), "01HMSG");
    let timestamp = header("webhook-timestamp");
    let now = now_secs() as i64;
    assert!((now - timestamp.parse::<i64>().unwrap()).abs() < 300);
    let key = base64::engine::general_purpose::STANDARD
        .decode(LISTEN_SECRET.trim_start_matches("whsec_"))
        .unwrap();
    let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(&key).unwrap();
    hmac::Mac::update(
        &mut mac,
        format!("01HMSG.{timestamp}.{}", got.body).as_bytes(),
    );
    let expected = format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(hmac::Mac::finalize(mac).into_bytes())
    );
    assert_eq!(header("webhook-signature"), expected);
    assert_eq!(
        serde_json::from_str::<Value>(&got.body).unwrap(),
        json!({"specversion": "1.0", "id": "evt-1", "source": "test",
               "type": "order.created", "data": {"n": 1}})
    );

    // The listener's own calls, in order.
    let log = log.lock().clone();
    let lines: Vec<&str> = log.iter().map(|s| s.line.as_str()).collect();
    assert!(
        lines[0].starts_with(&format!("POST /v1/{ENV}/queues?queue_id=listen-")),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with(&format!(
            "POST /v1/{ENV}/subscriptions?subscription_id=listen-"
        )),
        "{lines:?}"
    );
    let sub: Value = serde_json::from_str(&log[1].body).unwrap();
    assert_eq!(sub["spec"]["topic"], format!("{ENV}/topics/default"));
    assert!(sub["spec"]["queue"]
        .as_str()
        .unwrap()
        .starts_with(&format!("{ENV}/queues/listen-")));
    assert!(lines[2].contains(":lease "), "{lines:?}");
    assert!(lines[3].contains(":ack "), "{lines:?}");
    let ack: Value = serde_json::from_str(&log[3].body).unwrap();
    assert_eq!(ack["leases"][0]["message_id"], "01HMSG");
    assert!(
        lines[4].starts_with(&format!("DELETE /v1/{ENV}/subscriptions/listen-")),
        "{lines:?}"
    );
    assert!(
        lines[5].starts_with(&format!("DELETE /v1/{ENV}/queues/listen-")),
        "{lines:?}"
    );

    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed["status"], 204);
    assert_eq!(printed["type"], "order.created");
}

// ---- first-use defaults: discover or create the standard project and env ---

/// The `whoami` of an org-scoped credential: the organization, and no project
/// or environment (the key is bound to the org, not to a checkout).
fn whoami_org_only() -> Value {
    json!({"principal": "principal_x", "api_key": "orgs/org_a/api_keys/key_self",
           "org": "orgs/org_a", "scopes": ["*:write"]})
}

/// Writes the stored, org-scoped credential a device `sylphx login` leaves in
/// `SYLPHX_CONFIG_DIR` (so runs use it and not `SYLPHX_API_KEY`): one key per
/// approved organization, with `active` selected.
fn store_org_keys(sb: &Sandbox, url: &str, orgs: &[(&str, &str, &str)], active: &str) {
    let orgs: Vec<Value> = orgs
        .iter()
        .map(|(id, slug, key)| {
            json!({"id": id, "slug": slug,
                   "key_name": format!("orgs/{id}/api_keys/key_cli"),
                   "store": "file", "api_key": key})
        })
        .collect();
    let creds = json!({"base_url": url, "store": "file", "active_org": active, "orgs": orgs});
    std::fs::create_dir_all(sb.dir.join("config")).unwrap();
    std::fs::write(sb.dir.join("config/credentials.json"), creds.to_string()).unwrap();
}

fn project_row(name: &str, slug: &str) -> Value {
    json!({"name": name, "spec": {"slug": slug}})
}

fn env_row(name: &str, slug: &str) -> Value {
    json!({"name": name, "spec": {"slug": slug}})
}

fn projects_page(rows: Vec<Value>) -> Value {
    json!({"projects": rows})
}

fn envs_page(rows: Vec<Value>) -> Value {
    json!({"envs": rows})
}

fn identity_bindings() -> Value {
    json!({"bindings": [{"slot": "identity", "environmentId": "env_a",
                         "providerId": "sylphx-auth", "status": "active",
                         "config": {"organizationId": "org-1"}}]})
}

/// Removes the sandbox's default `.sylphx/project.json` so a run starts in a
/// fresh directory (the `.sylphx` directory itself stays).
fn unlink(sb: &Sandbox) {
    std::fs::remove_file(sb.dir.join(".sylphx/project.json")).unwrap();
}

fn read_link(sb: &Sandbox) -> Value {
    serde_json::from_str(&std::fs::read_to_string(sb.dir.join(".sylphx/project.json")).unwrap())
        .unwrap()
}

/// A fresh, org-scoped checkout: the first `sylphx auth status` discovers the
/// standard `default` project and `production` environment from Access, writes
/// the full `.sylphx/project.json`, and reaches the binding it reports. The
/// second run uses that link: no discovery, no create, no rewrite.
#[tokio::test(flavor = "multi_thread")]
async fn first_use_resolves_the_standard_defaults_and_persists_the_link() {
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
        ),
        (200, envs_page(vec![env_row(ENV, "production")])),
        (200, identity_bindings()),
    ])
    .await;
    let sb = Sandbox::new("first-use-default");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status", "-o", "json"]).await;
    assert_success(&out);
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(printed["enabled"], true, "{printed}");
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a", "project": "orgs/org_a/projects/prj_a", "env": ENV})
    );
    let seen = log.lock().clone();
    assert_eq!(seen.len(), 4);
    assert!(
        seen[0].line.starts_with("GET /v1/whoami "),
        "{}",
        seen[0].line
    );
    assert!(
        seen[1].line.starts_with("GET /v1/orgs/org_a/projects "),
        "{}",
        seen[1].line
    );
    assert!(
        seen[2]
            .line
            .starts_with("GET /v1/orgs/org_a/projects/prj_a/envs "),
        "{}",
        seen[2].line
    );
    assert!(
        seen[3]
            .line
            .starts_with("GET /v1/projects/prj_a/composition/bindings "),
        "{}",
        seen[3].line
    );
    for request in &seen {
        assert!(
            request
                .head
                .to_ascii_lowercase()
                .contains("authorization: bearer sylphx_sk_org_a"),
            "{}",
            request.head
        );
    }

    // Rerun in the same checkout: the complete link is used as it is.
    let (url2, log2) = serve(vec![(200, identity_bindings())]).await;
    let out = run_stored(&sb, &url2, &["auth", "status", "-o", "json"]).await;
    assert_success(&out);
    let seen2 = log2.lock().clone();
    assert_eq!(
        seen2.len(),
        1,
        "no whoami, list, or create on the second run"
    );
    assert!(
        seen2[0]
            .line
            .starts_with("GET /v1/projects/prj_a/composition/bindings "),
        "{}",
        seen2[0].line
    );
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a", "project": "orgs/org_a/projects/prj_a", "env": ENV})
    );
}
#[tokio::test(flavor = "multi_thread")]
async fn first_use_with_an_unwritable_link_destination_continues_on_rerun() {
    let sb = Sandbox::new("first-use-unwritable");
    unlink(&sb);
    // A directory at the file destination fails even for privileged runners.
    let destination = sb.dir.join(".sylphx/project.json");
    std::fs::create_dir(&destination).unwrap();
    for _ in 0..2 {
        let (url, log) = serve(vec![
            (200, whoami_org_only()),
            (
                200,
                projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
            ),
            (200, envs_page(vec![env_row(ENV, "production")])),
            (200, identity_bindings()),
        ])
        .await;
        store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
        let out = run_stored(&sb, &url, &["auth", "status", "-o", "json"]).await;
        assert_success(&out);
        let printed: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(printed["enabled"], true, "{printed}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("warning:") && err.contains("project.json"),
            "{err}"
        );
        let seen = log.lock();
        assert_eq!(seen.len(), 4);
        assert!(
            seen[3]
                .line
                .starts_with("GET /v1/projects/prj_a/composition/bindings "),
            "{}",
            seen[3].line
        );
        assert!(
            seen.iter().all(|request| request.line.starts_with("GET ")),
            "{seen:?}"
        );
        assert!(destination.is_dir());
    }
    // Explicit link promises persistence and must still fail at this boundary.
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
        ),
        (200, envs_page(vec![env_row(ENV, "production")])),
    ])
    .await;
    let out = run_stored(&sb, &url, &["link"]).await;
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("project.json"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("Linked"));
    assert_eq!(log.lock().len(), 3);
    assert!(destination.is_dir());
}
/// No standard slug anywhere: a lone existing project and environment are
/// chosen, and the full link is persisted.
#[tokio::test(flavor = "multi_thread")]
async fn unique_existing_targets_are_used_when_no_standard_slug_exists() {
    let (url, _log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_web", "web")]),
        ),
        (
            200,
            envs_page(vec![env_row(
                "orgs/org_a/projects/prj_web/envs/env_stg",
                "staging",
            )]),
        ),
        (200, identity_bindings()),
    ])
    .await;
    let sb = Sandbox::new("first-use-unique");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status", "-o", "json"]).await;
    assert_success(&out);
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a",
               "project": "orgs/org_a/projects/prj_web",
               "env": "orgs/org_a/projects/prj_web/envs/env_stg"})
    );
}

/// An empty organization: the standard `default` project is created through
/// `access.projects.create` with `spec.slug = default`, its (auto-seeded)
/// `production` environment is listed, and the run reaches the binding.
#[tokio::test(flavor = "multi_thread")]
async fn first_use_creates_the_default_project_when_the_org_is_empty() {
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (200, projects_page(vec![])),
        (200, project_row("orgs/org_a/projects/prj_new", "default")),
        (
            200,
            envs_page(vec![env_row(
                "orgs/org_a/projects/prj_new/envs/env_prod",
                "production",
            )]),
        ),
        (200, identity_bindings()),
    ])
    .await;
    let sb = Sandbox::new("first-use-create");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status", "-o", "json"]).await;
    assert_success(&out);
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a",
               "project": "orgs/org_a/projects/prj_new",
               "env": "orgs/org_a/projects/prj_new/envs/env_prod"})
    );
    let seen = log.lock().clone();
    assert_eq!(seen.len(), 5);
    assert!(
        seen[2].line.starts_with("POST /v1/orgs/org_a/projects "),
        "{}",
        seen[2].line
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen[2].body).unwrap(),
        json!({"spec": {"slug": "default"}})
    );
    assert!(
        seen[3]
            .line
            .starts_with("GET /v1/orgs/org_a/projects/prj_new/envs "),
        "{}",
        seen[3].line
    );
    assert!(
        seen[4]
            .line
            .starts_with("GET /v1/projects/prj_new/composition/bindings "),
        "{}",
        seen[4].line
    );
}

/// Multiple existing targets without a standard default require a choice;
/// discovery must not invent a target or select the first row.
#[tokio::test(flavor = "multi_thread")]
async fn ambiguous_defaults_fail_and_list_the_choices_without_mutating() {
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            projects_page(vec![
                project_row("orgs/org_a/projects/prj_1", "web"),
                project_row("orgs/org_a/projects/prj_2", "api"),
            ]),
        ),
    ])
    .await;
    let sb = Sandbox::new("first-use-ambiguous-project");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status"]).await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("orgs/org_a/projects/prj_1")
            && err.contains("orgs/org_a/projects/prj_2")
            && err.contains("choose"),
        "{err}"
    );
    let seen = log.lock().clone();
    assert_eq!(seen.len(), 2, "no create is attempted on ambiguity");
    assert!(!seen.iter().any(|r| r.line.starts_with("POST ")));
    assert!(
        !sb.dir.join(".sylphx/project.json").exists(),
        "nothing is linked on ambiguity"
    );

    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
        ),
        (
            200,
            envs_page(vec![
                env_row("orgs/org_a/projects/prj_a/envs/env_1", "staging"),
                env_row("orgs/org_a/projects/prj_a/envs/env_2", "development"),
            ]),
        ),
    ])
    .await;
    let sb = Sandbox::new("first-use-ambiguous-env");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status"]).await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("env_1") && err.contains("env_2") && err.contains("choose"),
        "{err}"
    );
    assert_eq!(log.lock().len(), 3);
    assert!(!sb.dir.join(".sylphx/project.json").exists());
}

/// An explicit full environment name resolves the target and skips discovery
/// entirely: no `whoami`, no list, no create.
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_environment_override_skips_discovery() {
    let (url, log) = serve(vec![(
        200,
        json!({"status": "active", "providerId": "sylphx-auth",
               "config": {"organizationId": "org-1"}}),
    )])
    .await;
    let sb = Sandbox::new("first-use-explicit-env");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(
        &sb,
        &url,
        &[
            "auth",
            "enable",
            "--env",
            "orgs/org_a/projects/prj_a/envs/env_b",
        ],
    )
    .await;
    assert_success(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("org-1"));
    let seen = log.lock().clone();
    assert_eq!(
        seen.len(),
        1,
        "an explicit environment name skips whoami and every list"
    );
    assert!(
        seen[0]
            .line
            .starts_with("PUT /v1/projects/prj_a/composition/bindings "),
        "{}",
        seen[0].line
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen[0].body).unwrap(),
        json!({"capabilityId": "identity", "slot": "identity",
               "providerId": "sylphx-auth", "environmentId": "env_b"})
    );
    assert!(
        !sb.dir.join(".sylphx/project.json").exists(),
        "an explicit target writes no link"
    );
}

/// An explicit `--org` resolves under it, and a checkout linked to another
/// organization is never rewritten.
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_org_resolves_under_it_and_leaves_a_foreign_link_alone() {
    let foreign = json!({"org": "orgs/org_b", "project": "orgs/org_b/projects/prj_b",
                         "env": "orgs/org_b/projects/prj_b/envs/env_b"});
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
        ),
        (200, envs_page(vec![env_row(ENV, "production")])),
        (
            200,
            json!({"name": format!("{ENV}/databases"), "databases": []}),
        ),
    ])
    .await;
    let sb = Sandbox::new("first-use-foreign-org");
    std::fs::write(sb.dir.join(".sylphx/project.json"), foreign.to_string()).unwrap();
    store_org_keys(
        &sb,
        &url,
        &[
            ("org_a", "acme", "sylphx_sk_org_a"),
            ("org_b", "beta", "sylphx_sk_org_b"),
        ],
        "org_b",
    );
    let out = run_stored(
        &sb,
        &url,
        &["data", "databases", "list", "--org", "acme", "-o", "json"],
    )
    .await;
    assert_success(&out);
    assert_eq!(
        read_link(&sb),
        foreign,
        "the foreign checkout is left exactly as it was"
    );
    let seen = log.lock().clone();
    assert_eq!(seen.len(), 4);
    assert!(
        seen[0].line.starts_with("GET /v1/whoami "),
        "{}",
        seen[0].line
    );
    assert!(
        seen[0]
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer sylphx_sk_org_a"),
        "{}",
        seen[0].head
    );
    assert!(
        seen[1].line.starts_with("GET /v1/orgs/org_a/projects "),
        "{}",
        seen[1].line
    );
    assert!(
        seen[3]
            .line
            .starts_with(&format!("GET /v1/{ENV}/databases ")),
        "{}",
        seen[3].line
    );
}

/// A list row that is malformed, or that names a target outside the parent, is
/// refused before any choice or create.
#[tokio::test(flavor = "multi_thread")]
async fn a_list_row_outside_the_parent_is_refused() {
    for (name, row, said) in [
        (
            "cross-tenant",
            project_row("orgs/org_b/projects/prj_x", "default"),
            "outside orgs/org_a/projects/",
        ),
        (
            "nested",
            project_row("orgs/org_a/projects/prj_x/extra", "default"),
            "outside orgs/org_a/projects/",
        ),
        (
            "nameless",
            json!({"spec": {"slug": "default"}}),
            "outside orgs/org_a/projects/",
        ),
    ] {
        let (url, log) = serve(vec![
            (200, whoami_org_only()),
            (200, projects_page(vec![row])),
        ])
        .await;
        let sb = Sandbox::new(&format!("first-use-row-{name}"));
        unlink(&sb);
        store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
        let out = run_stored(&sb, &url, &["auth", "status"]).await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{name}: {err}");
        assert!(err.contains(said), "{name}: {err}");
        let seen = log.lock().clone();
        assert_eq!(seen.len(), 2, "{name}: only whoami and the list");
        assert!(
            !seen.iter().any(|r| r.line.starts_with("POST ")),
            "{name}: no create"
        );
        assert!(!sb.dir.join(".sylphx/project.json").exists(), "{name}");
    }
}

/// Project discovery follows pagination, so a `default` project on a later
/// page is still found.
#[tokio::test(flavor = "multi_thread")]
async fn paginated_project_discovery_accounts_for_a_later_default() {
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            json!({"projects": [project_row("orgs/org_a/projects/prj_x", "api")],
                   "next_page_token": "page2"}),
        ),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
        ),
        (200, envs_page(vec![env_row(ENV, "production")])),
        (200, identity_bindings()),
    ])
    .await;
    let sb = Sandbox::new("first-use-paged");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status", "-o", "json"]).await;
    assert_success(&out);
    let link = read_link(&sb);
    assert_eq!(link["project"], "orgs/org_a/projects/prj_a", "{link}");
    let seen = log.lock().clone();
    assert!(
        seen[1].line.starts_with("GET /v1/orgs/org_a/projects "),
        "{}",
        seen[1].line
    );
    assert!(
        seen[2]
            .line
            .starts_with("GET /v1/orgs/org_a/projects?page_token=page2 "),
        "{}",
        seen[2].line
    );
}

/// No-flags `sylphx link` bootstraps the standard project and environment from
/// the key's org and writes the full link.
#[tokio::test(flavor = "multi_thread")]
async fn link_without_flags_bootstraps_the_default_targets() {
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (
            200,
            projects_page(vec![project_row("orgs/org_a/projects/prj_a", "default")]),
        ),
        (200, envs_page(vec![env_row(ENV, "production")])),
    ])
    .await;
    let sb = Sandbox::new("link-bootstrap");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["link"]).await;
    assert_success(&out);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Linked "));
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a", "project": "orgs/org_a/projects/prj_a", "env": ENV})
    );
    let seen = log.lock().clone();
    assert_eq!(seen.len(), 3);
    assert!(
        seen[0].line.starts_with("GET /v1/whoami "),
        "{}",
        seen[0].line
    );
    assert!(
        seen[1].line.starts_with("GET /v1/orgs/org_a/projects "),
        "{}",
        seen[1].line
    );
    assert!(
        seen[2]
            .line
            .starts_with("GET /v1/orgs/org_a/projects/prj_a/envs "),
        "{}",
        seen[2].line
    );
}

/// The bootstrap create carries a stable `Idempotency-Key` derived from the
/// target, so a lost reply cannot duplicate a first-use project: two separate
/// empty-org discoveries replay the very same key.
#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_create_replays_with_a_stable_idempotency_key() {
    let mut keys = Vec::new();
    for run in ["a", "b"] {
        let (url, log) = serve(vec![
            (200, whoami_org_only()),
            (200, projects_page(vec![])),
            (200, project_row("orgs/org_a/projects/prj_new", "default")),
            (
                200,
                envs_page(vec![env_row(
                    "orgs/org_a/projects/prj_new/envs/env_prod",
                    "production",
                )]),
            ),
            (200, identity_bindings()),
        ])
        .await;
        let sb = Sandbox::new(&format!("bootstrap-key-{run}"));
        unlink(&sb);
        store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
        let out = run_stored(&sb, &url, &["auth", "status"]).await;
        assert_success(&out);
        let seen = log.lock().clone();
        let create = seen
            .iter()
            .find(|r| r.line.starts_with("POST /v1/orgs/org_a/projects "))
            .expect("the empty org creates the default project");
        let key = create
            .head
            .to_ascii_lowercase()
            .lines()
            .find(|l| l.starts_with("idempotency-key:"))
            .expect("bootstrap creation must carry a replay key")
            .to_string();
        keys.push(key);
    }
    assert_eq!(
        keys[0], keys[1],
        "the same discovery replays the same idempotency key"
    );
}

/// `link --env` may name a whole environment on its own: the project comes
/// from the name, without a `--project` flag.
#[tokio::test(flavor = "multi_thread")]
async fn link_accepts_a_full_environment_name_without_a_project_flag() {
    let (url, log) = serve(vec![(200, whoami_org_only())]).await;
    let sb = Sandbox::new("link-env-full");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(
        &sb,
        &url,
        &["link", "--env", "orgs/org_a/projects/prj_a/envs/env_b"],
    )
    .await;
    assert_success(&out);
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a", "project": "orgs/org_a/projects/prj_a",
               "env": "orgs/org_a/projects/prj_a/envs/env_b"})
    );
    let seen = log.lock().clone();
    assert_eq!(seen.len(), 1, "the full name needs no lookup");
    assert!(
        seen[0].line.starts_with("GET /v1/whoami "),
        "{}",
        seen[0].line
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn link_reuses_the_selected_context_and_resolves_bare_environment_names() {
    for (index, env) in [None, Some("env_b"), Some("preview")]
        .into_iter()
        .enumerate()
    {
        let mut replies = vec![(200, whoami_org_only())];
        if env == Some("preview") {
            // Public Resources need only name and spec, not a second uid.
            replies.push((
                200,
                envs_page(vec![json!({
                    "name": "orgs/org_a/projects/prj_a/envs/env_b",
                    "spec": {"slug": "preview"}
                })]),
            ));
        }
        let (url, _log) = serve(replies).await;
        let sb = Sandbox::new(&format!("link-selected-{index}"));
        let mut args = vec!["link"];
        if let Some(env) = env {
            args.extend(["--env", env]);
        }
        assert_success(&run(&sb, &url, &args).await);
        let expected = if env.is_some() {
            "orgs/org_a/projects/prj_a/envs/env_b"
        } else {
            ENV
        };
        assert_eq!(
            read_link(&sb),
            json!({"org": "orgs/org_a",
            "project": "orgs/org_a/projects/prj_a", "env": expected})
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn link_process_context_is_explicitly_persisted_but_target_flags_win() {
    let (url, _log) = serve(vec![(200, whoami_org_only()); 2]).await;
    let sb = Sandbox::new("link-process-context");
    let other = "orgs/org_a/projects/prj_b/envs/env_b";
    let mut command = sb.cmd(&url, &["link"]);
    command.env("SYLPHX_ENVIRONMENT", other);
    assert_success(
        &tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a",
        "project": "orgs/org_a/projects/prj_b", "env": other})
    );
    let mut command = sb.cmd(&url, &["link", "--env", ENV]);
    command.env("SYLPHX_ENVIRONMENT", "invalid");
    assert_success(
        &tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(
        read_link(&sb),
        json!({"org": "orgs/org_a",
        "project": "orgs/org_a/projects/prj_a", "env": ENV})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn link_project_override_discards_the_old_environment() {
    let project = "orgs/org_a/projects/prj_b";
    for (index, flag) in ["web", "prj_b", project].into_iter().enumerate() {
        let mut replies = vec![(200, whoami_org_only())];
        if flag == "web" {
            replies.push((200, projects_page(vec![project_row(project, "web")])));
        }
        let env = format!("{project}/envs/env_b");
        replies.push((200, envs_page(vec![env_row(&env, "production")])));
        let (url, _log) = serve(replies).await;
        let sb = Sandbox::new(&format!("link-project-change-{index}"));
        assert_success(&run(&sb, &url, &["link", "--project", flag]).await);
        assert_eq!(
            read_link(&sb),
            json!({"org": "orgs/org_a", "project": project, "env": env})
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn link_refuses_conflicting_or_malformed_explicit_targets_without_rewriting() {
    let cases: &[&[&str]] = &[
        &["--project", "orgs/org_b/projects/prj_a", "--env", ENV],
        &["--project", "prj_b", "--env", ENV],
        &["--project", "orgs/org_a/projects/prj_a/envs/env_a"],
        &["--project", "projects/prj_a"],
        &["--env", "envs/env_a"],
        &["--env", "orgs/org_a/projects/prj_a/envs/env_a/extra"],
        &["--env", "orgs/org_a/projects/prj_a/envs/"],
    ];
    for (index, flags) in cases.iter().enumerate() {
        let (url, _log) = serve(vec![(200, whoami_org_only())]).await;
        let sb = Sandbox::new(&format!("link-invalid-{index}"));
        let before = std::fs::read(sb.dir.join(".sylphx/project.json")).unwrap();
        let args: Vec<&str> = std::iter::once("link")
            .chain(flags.iter().copied())
            .collect();
        let out = run(&sb, &url, &args).await;
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read(sb.dir.join(".sylphx/project.json")).unwrap(),
            before
        );
    }
}

/// `--org` that disagrees with the credential's own `whoami` is refused before
/// anything is discovered or written.
#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_org_that_disagrees_with_the_key_is_rejected() {
    let (url, log) = serve(vec![(200, whoami_org_only())]).await;
    let sb = Sandbox::new("org-conflict");
    unlink(&sb);
    store_org_keys(
        &sb,
        &url,
        &[
            ("org_a", "acme", "sylphx_sk_org_a"),
            ("org_b", "beta", "sylphx_sk_org_b"),
        ],
        "org_a",
    );
    let out = run_stored(&sb, &url, &["auth", "status", "--org", "beta"]).await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--org does not match the signed-in credential"),
        "{err}"
    );
    assert_eq!(log.lock().len(), 1, "only whoami was called");
    assert!(!sb.dir.join(".sylphx/project.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn first_use_provisions_a_missing_production_environment() {
    let project = "orgs/org_a/projects/prj_a";
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (200, projects_page(vec![project_row(project, "default")])),
        (200, envs_page(vec![])),
        (200, env_row(ENV, "production")),
        (200, identity_bindings()),
    ])
    .await;
    let sb = Sandbox::new("first-use-missing-env");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status", "-o", "json"]).await;
    assert_success(&out);
    assert_eq!(read_link(&sb)["env"], ENV);
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["enabled"],
        true
    );
    let seen = log.lock();
    assert!(seen[3]
        .line
        .starts_with(&format!("POST /v1/{project}/envs ")));
    assert_eq!(
        serde_json::from_str::<Value>(&seen[3].body).unwrap(),
        json!({"spec": {"slug": "production", "mode": "live"}})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_use_adopts_the_winning_default_project() {
    let project = "orgs/org_a/projects/prj_a";
    let (url, log) = serve(vec![
        (200, whoami_org_only()),
        (200, projects_page(vec![])),
        (
            409,
            json!({"code": "RESOURCE_ALREADY_EXISTS", "status": 409,
            "detail": "default already exists", "retryable": false}),
        ),
        (200, projects_page(vec![project_row(project, "default")])),
        (200, envs_page(vec![env_row(ENV, "production")])),
        (200, identity_bindings()),
    ])
    .await;
    let sb = Sandbox::new("first-use-race");
    unlink(&sb);
    store_org_keys(&sb, &url, &[("org_a", "acme", "sylphx_sk_org_a")], "org_a");
    let out = run_stored(&sb, &url, &["auth", "status", "-o", "json"]).await;
    assert_success(&out);
    assert_eq!(read_link(&sb)["project"], project);
    assert_eq!(read_link(&sb)["env"], ENV);
    assert_eq!(
        log.lock()
            .iter()
            .filter(|r| r.line.starts_with("POST "))
            .count(),
        1
    );
}
