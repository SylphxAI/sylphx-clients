//! `sylphx work runner` against a local fake of Work's claim protocol
//! (`POST /v2/claim`, `/v2/beat`, `/v2/note`): with two ready steps it takes
//! exactly one, runs the command with the claim on standard input and the
//! claim token (not the runner's key) in its environment, hands the step over
//! and exits 0, leaving the second step untouched; a step the command handed
//! over itself is not released again; a cancel answered by a beat stops the
//! command's process group and releases the step.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const KEY: &str = "sylphx_sk_runner";
const LABEL: &str = "member:group:builder";

/// One request the fake saw: its path, body and Authorization header.
#[derive(Debug, Clone)]
struct Call {
    path: String,
    body: Value,
    auth: String,
}

/// The fake's state: the ready steps, the claim's time-to-live, and how it
/// answers beats and releases.
struct Fake {
    ready: Vec<&'static str>,
    ttl: i64,
    cancel_beats: bool,
    /// The release answers this refusal: the command handed the step over.
    release_refused: bool,
    calls: Vec<Call>,
}

type Shared = Arc<Mutex<Fake>>;

fn answer(fake: &Shared, path: &str, body: &Value) -> (u16, Value) {
    let mut f = fake.lock().unwrap();
    match path {
        "/v2/claim" => {
            if f.ready.is_empty() {
                return (200, json!({ "claims": [] }));
            }
            let item = f.ready.remove(0);
            let name = body["runner"].as_str().unwrap_or_default();
            (
                200,
                json!({ "claims": [{
                    "claim": { "item": item, "principal": "p-host", "unit": "build",
                               "runner": format!("runner:p-host/{name}"), "generation": 7,
                               "ttlSeconds": f.ttl },
                    "item": { "key": item, "title": "Build it" },
                    "token": format!("tok-{item}"),
                    "profile": { "member": "group:builder", "version": 3, "body": "The builder job." },
                    "workspaceProfile": { "member": "workspace", "version": 2, "body": "Our rules." }
                }] }),
            )
        }
        "/v2/beat" => {
            let entry = body["claims"][0].as_str().unwrap_or_default();
            let (item, gen) = entry.split_once('@').unwrap_or_default();
            let gen: i64 = gen.parse().unwrap_or_default();
            if f.cancel_beats {
                (
                    200,
                    json!({ "claims": [{ "item": item, "generation": gen, "cancel": true, "reason": "dropped" }] }),
                )
            } else {
                (
                    200,
                    json!({ "claims": [{ "item": item, "generation": gen,
                                         "expiresAt": "2026-10-07T12:00:00Z", "token": "tok-renewed" }] }),
                )
            }
        }
        "/v2/note" if f.release_refused => (
            409,
            json!({ "code": "stale_claim", "message": "item:W-1 has no claim of yours" }),
        ),
        "/v2/note" => (200, json!({ "event": { "seq": 99, "kind": "released" } })),
        _ => (404, json!({ "code": "not_found", "message": path })),
    }
}

async fn serve(fake: Shared) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let fake = fake.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, body) = loop {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(i) = text.find("\r\n\r\n") {
                        let head = text[..i].to_string();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse().ok())?
                            })
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let Ok(n) = sock.read(&mut chunk).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head, buf[i + 4..i + 4 + len].to_vec());
                    }
                };
                let line = head.lines().next().unwrap_or_default().to_string();
                let path = line.split(' ').nth(1).unwrap_or_default().to_string();
                let auth = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_default();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                fake.lock().unwrap().calls.push(Call {
                    path: path.clone(),
                    body: body.clone(),
                    auth,
                });
                let (status, reply) = answer(&fake, &path, &body);
                if path == "/v2/claim" && reply["claims"].as_array().is_some_and(|c| c.is_empty()) {
                    // a waiting claim that finds nothing holds the call a while
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                let text = reply.to_string();
                let out = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    url
}

fn fake(ttl: i64, cancel_beats: bool, release_refused: bool) -> Shared {
    Arc::new(Mutex::new(Fake {
        ready: vec!["W-1", "W-2"],
        ttl,
        cancel_beats,
        release_refused,
        calls: Vec::new(),
    }))
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sylphx-work-runner-test-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("config")).unwrap();
    d
}

/// Runs `sylphx work runner` with `script` as its command, in `dir`.
async fn runner(url: &str, dir: &Path, script: &str, extra: &[&str]) -> (Output, Duration) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_sylphx"));
    c.args([
        "work",
        "runner",
        "--label",
        LABEL,
        "--name",
        "host-1",
        "--work-url",
        url,
    ])
    .args(extra)
    .args(["--", "sh", "-c", script])
    .current_dir(dir)
    .env("SYLPHX_API_KEY", KEY)
    .env("SYLPHX_CONFIG_DIR", dir.join("config"))
    .env("OUT", dir)
    .env_remove("SYLPHX_WORK_URL")
    .stdin(Stdio::null());
    let started = Instant::now();
    let out = tokio::task::spawn_blocking(move || c.output().unwrap())
        .await
        .unwrap();
    (out, started.elapsed())
}

fn calls(fake: &Shared, path: &str) -> Vec<Call> {
    fake.lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.path == path)
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn takes_one_step_runs_it_with_the_claim_token_hands_it_over_and_exits_0() {
    let f = fake(900, false, false);
    let url = serve(f.clone()).await;
    let dir = scratch("one");
    std::fs::write(dir.join("config/credentials.json"), "runner-login-marker").unwrap();
    let script = r#"cat > "$OUT/stdin.json"; printf %s "$SYLPHX_WORK_CLAIM_TOKEN" > "$OUT/token"; printf %s "${SYLPHX_API_KEY:-none}" > "$OUT/key"; cp "$SYLPHX_WORK_CLAIM_FILE" "$OUT/file.json"; printf '%s %s %s' "$SYLPHX_WORK_ITEM" "$SYLPHX_WORK_GENERATION" "$SYLPHX_WORK_RUNNER" > "$OUT/env"; printf %s "$SYLPHX_CONFIG_DIR" > "$OUT/config-path"; ls -A "$SYLPHX_CONFIG_DIR" > "$OUT/config-entries"; exit 3"#;
    let (out, _) = runner(&url, &dir, script, &["--ephemeral", "-o", "json"]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");

    // exactly one waiting claim, for one step, with the runner's labels and name
    let claims = calls(&f, "/v2/claim");
    assert_eq!(claims.len(), 1, "{claims:?}");
    let c = &claims[0];
    assert_eq!(c.auth, format!("Bearer {KEY}"));
    assert_eq!(c.body["n"], 1);
    assert_eq!(c.body["labels"], json!([LABEL]));
    assert_eq!(c.body["runner"], "host-1");
    assert_eq!(c.body["wait"], true);
    assert!(c.body["opId"].as_str().is_some_and(|o| !o.is_empty()));

    // the command got the claim, its token, and not the runner's key
    let stdin: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("stdin.json")).unwrap()).unwrap();
    assert_eq!(stdin["claim"]["item"], "W-1");
    assert_eq!(stdin["profile"]["body"], "The builder job.");
    assert_eq!(stdin["workspaceProfile"]["body"], "Our rules.");
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("file.json")).unwrap()).unwrap();
    assert_eq!(file, stdin);
    assert_eq!(
        std::fs::read_to_string(dir.join("token")).unwrap(),
        "tok-W-1"
    );
    assert_eq!(std::fs::read_to_string(dir.join("key")).unwrap(), "none");
    let config = std::fs::read_to_string(dir.join("config-path")).unwrap();
    assert_ne!(Path::new(&config), dir.join("config"));
    assert_eq!(
        std::fs::read_to_string(dir.join("config-entries")).unwrap(),
        ""
    );
    assert!(
        !Path::new(&config).exists(),
        "step config should be removed"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("config/credentials.json")).unwrap(),
        "runner-login-marker"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("env")).unwrap(),
        "W-1 7 runner:p-host/host-1"
    );

    // the step is handed over with a checkpoint naming the exit status
    let notes = calls(&f, "/v2/note");
    assert_eq!(notes.len(), 1, "{notes:?}");
    let n = &notes[0].body;
    assert_eq!(n["node"], "item:W-1");
    assert_eq!(n["release"], true);
    assert_eq!(n["generation"], 7);
    assert_eq!(n["kind"], "checkpoint");
    assert!(n["done"].as_str().unwrap().contains("status 3"), "{n}");
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["item"], "W-1");
    assert_eq!(result["exitCode"], 3);
    assert_eq!(result["handedOverBy"], "runner");

    // the second ready step was never touched
    let all = f.lock().unwrap().calls.clone();
    assert!(
        all.iter().all(|c| !c.body.to_string().contains("W-2")),
        "{all:?}"
    );
    assert_eq!(f.lock().unwrap().ready, vec!["W-2"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_the_command_handed_over_is_not_released_again() {
    let f = fake(900, false, true);
    let url = serve(f.clone()).await;
    let dir = scratch("self");
    let (out, _) = runner(&url, &dir, "exit 0", &[]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stderr.contains("already handed over (stale_claim)"),
        "{stderr}"
    );
    assert_eq!(calls(&f, "/v2/note").len(), 1);
    assert_eq!(calls(&f, "/v2/claim").len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_from_the_beat_stops_the_command_and_releases_the_step() {
    // a 2-second time-to-live: the runner beats every second
    let f = fake(2, true, false);
    let url = serve(f.clone()).await;
    let dir = scratch("cancel");
    // the command starts a grandchild in its own group and waits on it
    let script = r#"sleep 60 & echo $! > "$OUT/pid"; wait; touch "$OUT/finished""#;
    let (out, took) = runner(&url, &dir, script, &[]).await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(took < Duration::from_secs(20), "took {took:?}: {stderr}");
    assert!(stderr.contains("cancelled (dropped)"), "{stderr}");
    assert!(!dir.join("finished").exists());
    // the whole process group was stopped: the grandchild is gone too
    let pid = std::fs::read_to_string(dir.join("pid")).unwrap();
    // (an orphan is reaped by init shortly after it dies)
    let deadline = Instant::now() + Duration::from_secs(5);
    let alive = loop {
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive || Instant::now() > deadline {
            break alive;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(!alive, "the command's child {pid} still runs");

    let beats = calls(&f, "/v2/beat");
    assert_eq!(beats.len(), 1, "{beats:?}");
    assert_eq!(beats[0].body["claims"], json!(["W-1@7"]));
    assert_eq!(beats[0].body["runner"], "host-1");
    assert_eq!(beats[0].body["freeSlots"], 0);
    let notes = calls(&f, "/v2/note");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].body["release"], true);
    assert!(notes[0].body["done"]
        .as_str()
        .unwrap()
        .contains("cancelled (dropped)"));
    assert_eq!(calls(&f, "/v2/claim").len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
