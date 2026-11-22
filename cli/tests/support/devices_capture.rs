//! Native capture transport checks reuse the CLI's ordinary fake public API.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha256};

const MAIN_LOG: &str = "/data/local/tmp/sylphx-log.aaaaaaaaaa";
const CRASH_LOG: &str = "/data/local/tmp/sylphx-log.bbbbbbbbbb";

fn exec_bytes(stdout: &[u8], truncated: bool) -> (u16, Value) {
    // Exercise both the raw stream ceiling and base64 wire overhead.
    let stdout = STANDARD.encode(stdout);
    assert!(stdout.len() < 1024 * 1024);
    (
        200,
        json!({"exit_code":0,"stdout":stdout,"truncated":truncated}),
    )
}

fn snapshot_metadata(path: &str, bytes: &[u8]) -> (u16, Value) {
    exec_reply(&format!(
        "{path} {} {:x}
",
        bytes.len(),
        Sha256::digest(bytes)
    ))
}

fn small_snapshot(path: &str, contents: &str) -> Vec<(u16, Value)> {
    let mut replies = vec![snapshot_metadata(path, contents.as_bytes())];
    if !contents.is_empty() {
        replies.push(exec_reply(contents));
    }
    replies.push(exec_reply("")); // rm
    replies
}

fn exec_reply(stdout: &str) -> (u16, Value) {
    exec_bytes(stdout.as_bytes(), false)
}

fn screenshot(width: u32, height: u32) -> (u16, Value) {
    // This transport fixture exercises IHDR and coordinate verification, not a
    // claim that a meaningful game frame was visually inspected.
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend(width.to_be_bytes());
    png.extend(height.to_be_bytes());
    png.extend([0; 9]);
    (
        200,
        json!({"screenshot":{"data":base64::engine::general_purpose::STANDARD.encode(png),
        "media_type":"image/png","width":width,"height":height}}),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn device_capture_uses_one_public_lease_and_guards_actual_png_coordinates() {
    for failure in ["none", "no-play-store", "foreground", "dimensions"] {
        let sb = Sandbox::new(&format!("device-capture-{failure}"));
        std::fs::write(sb.dir.join("app.apk"), b"APK transport fixture").unwrap();
        std::fs::write(sb.dir.join("scenario.json"), json!({
            "package":"com.example.game","sourceCommit":"abc123", "sizes":{"tablet":["2560x1600",240]},
            "steps":[{"launch":true},{"tap":[1.0,1.0]},{"shot":"lobby"}]
        }).to_string()).unwrap();
        let lease = format!("{ENV}/leases/capture");
        let mut replies = vec![
            (
                200,
                json!({"name":lease,"spec":{"device":{"model":"fixture","os_version":"14"}},"status":{"state":"ready"}}),
            ),
            (200, json!({})), // APK upload
            (200, json!({"package_name":"com.example.game"})),
            exec_reply(""), // native wm size
            exec_reply(""), // native wm density
            exec_reply("Physical size: 1080x2400\nOverride size: 2560x1600\n"),
            exec_reply("Physical density: 420\nOverride density: 240\n"),
        ];
        if failure == "no-play-store" {
            replies.push(exec_reply("package:android\npackage:com.example.game\n"));
        } else {
            replies.extend([
                exec_reply("package:android\npackage:com.android.vending\n"),
                exec_reply("disabled"),
                exec_reply("package:com.android.vending\n"),
            ]);
        }
        replies.extend([
            if failure == "no-play-store" {
                (200, json!({"exit_code":1})) // unsupported clear is advisory
            } else {
                exec_reply("")
            },
            exec_reply("Events injected: 1"), // launch
            exec_reply("mResumedActivity: ActivityRecord{a u0 com.example.game/.Main t1}"),
            exec_reply(if failure == "foreground" {
                "mCurrentFocus=Window{a u0 com.android.launcher/.Home}"
            } else {
                "mCurrentFocus=Window{a u0 com.example.game/.Main}"
            }),
        ]);
        if failure != "foreground" {
            replies.push(if failure == "dimensions" {
                screenshot(1280, 800)
            } else {
                screenshot(2560, 1600)
            });
        }
        if failure == "none" || failure == "no-play-store" {
            replies.extend([
                exec_reply(""), // tap
                exec_reply("mResumedActivity: ActivityRecord{a u0 com.example.game/.Main t1}"),
                exec_reply("mCurrentFocus=Window{a u0 com.example.game/.Main}"),
                screenshot(2560, 1600),
            ]);
        }
        replies.push(exec_reply("1234")); // still running
        if failure == "foreground" {
            replies.push(exec_bytes(b"partial metadata", true));
        } else {
            replies.extend(small_snapshot(MAIN_LOG, "quiet"));
        }
        replies.extend(small_snapshot(CRASH_LOG, ""));
        if failure == "foreground" {
            replies.extend([
                exec_reply("ANR in com.example.game\nInput dispatching timed out"),
                exec_reply("ANR in com.example.game\nReason: Input dispatching timed out"),
                exec_reply("Process: com.example.game\n\"main\" tid=1 Native\n  at example.Frame.draw(Frame.java:1)"),
            ]);
        } else if failure == "dimensions" {
            // A diagnostic refusal cannot replace the original capture error.
            replies.extend([
                (
                    403,
                    json!({"error":{"code":"PERMISSION_DENIED","message":"fixture refusal"}}),
                ),
                exec_reply("ANR in com.example.game\nold event from before this launch"),
            ]);
        }
        replies.extend([
            if failure == "foreground" {
                (403, json!({"error":{"code":"PERMISSION_DENIED","message":"final screenshot unavailable"}}))
            } else {
                screenshot(2560, 1600)
            },
            (200, json!({"name":lease,"status":{"state":"ending"}})),
        ]);
        let (url, log) = serve(replies).await;
        let out = run(
            &sb,
            &url,
            &[
                "devices",
                "run",
                "--app",
                "app.apk",
                "--env",
                ENV,
                "--scenario",
                "scenario.json",
                "--size",
                "tablet",
                "--out",
                "captures",
                "-o",
                "json",
            ],
        )
        .await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(if failure == "none" || failure == "no-play-store" {
                0
            } else {
                2
            }),
            "{failure}: {stderr}"
        );
        let seen = log.lock();
        assert_eq!(
            seen.iter()
                .filter(|row| row.line.starts_with(&format!("POST /v1/{ENV}/leases ")))
                .count(),
            1
        );
        assert_eq!(
            seen.iter()
                .filter(|row| row.line.contains(":release"))
                .count(),
            1,
            "lease is released after input refusal too"
        );
        let commands: Vec<Value> = seen
            .iter()
            .filter(|row| row.line.contains(":exec"))
            .map(|row| serde_json::from_str(&row.body).unwrap())
            .map(|body: Value| body["command"].clone())
            .collect();
        let launch = commands
            .iter()
            .position(|argv| argv[0] == "monkey")
            .unwrap();
        assert_eq!(
            commands[launch - 1],
            json!(["logcat", "-b", "main,system,crash", "-c"])
        );
        assert_eq!(
            commands.iter().filter(|argv| argv[0] == "logcat").count(),
            1
        );
        if failure == "no-play-store" {
            assert!(stderr.contains("warning: clearing the Android log before launch failed"));
        }
        let taps: Vec<_> = commands
            .iter()
            .filter(|command| command[0] == "input")
            .collect();
        if failure == "none" || failure == "no-play-store" {
            assert_eq!(taps, vec![&json!(["input", "tap", "2559", "1599"])]);
            let receipt: Value = serde_json::from_slice(
                &std::fs::read(sb.dir.join("captures/capture.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["steps"][2]["width"], 2560);
            assert_eq!(receipt["steps"][2]["height"], 1600);
            assert_eq!(receipt["steps"][2]["state"], "success");
            assert!(receipt["apk_sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64));
            assert!(sb.dir.join("captures/lobby.png").is_file());
            if failure == "none" {
                assert_eq!(
                    std::fs::read_to_string(sb.dir.join("captures/logcat.txt")).unwrap(),
                    "quiet"
                );
                assert!(!sb.dir.join("captures/last-anr.txt").exists());
                assert!(!sb.dir.join("captures/anr-dropbox-tail.txt").exists());
            }
        } else {
            assert!(
                taps.is_empty(),
                "wrong foreground/dimensions never sends input"
            );
            assert!(!sb.dir.join("captures/lobby.png").exists());
            let result: Value = serde_json::from_slice(
                &std::fs::read(sb.dir.join("captures/result.json")).unwrap(),
            )
            .unwrap();
            let diagnostics: Value = serde_json::from_slice(
                &std::fs::read(sb.dir.join("captures/diagnostics.json")).unwrap(),
            )
            .unwrap();
            if failure == "foreground" {
                assert_eq!(result["log_truncation"]["logcat"], true);
                assert_eq!(result["crashed"], true, "late ANR evidence is retained");
                assert!(!sb.dir.join("captures/logcat.txt").exists());
                assert!(
                    std::fs::read_to_string(sb.dir.join("captures/logcat-tail.txt"))
                        .unwrap()
                        .contains("ANR in com.example.game")
                );
                assert!(
                    std::fs::read_to_string(sb.dir.join("captures/anr-dropbox-tail.txt"))
                        .unwrap()
                        .contains("example.Frame.draw")
                );
                assert_eq!(diagnostics["logcat-tail.txt"]["api_truncated"], false);
            } else {
                assert!(result["error"].as_str().unwrap().contains("native PNG"));
                assert!(diagnostics["last-anr.txt"]["error"].is_string());
                assert_eq!(
                    result["crashed"], false,
                    "diagnostic snapshots alone do not change the verdict"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_device_scenario_fails_before_a_lease_request() {
    let sb = Sandbox::new("device-capture-invalid");
    std::fs::write(sb.dir.join("app.apk"), b"fixture").unwrap();
    std::fs::write(
        sb.dir.join("scenario.json"),
        json!({"package":"com.example.game",
        "sizes":{"phone":["1080x2400",420]},"steps":[{"shot":"../escape"}]})
        .to_string(),
    )
    .unwrap();
    let (url, log) = serve(vec![(200, json!({}))]).await;
    let out = run(
        &sb,
        &url,
        &[
            "devices",
            "run",
            "--app",
            "app.apk",
            "--scenario",
            "scenario.json",
            "--size",
            "phone",
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(2));
    assert!(log.lock().is_empty());
    // A previous partial run must not leave images that a new run overwrites
    // or mistakes for current evidence, even if result.json was never written.
    std::fs::write(
        sb.dir.join("scenario.json"),
        json!({"package":"com.example.game",
        "sizes":{"phone":["1080x2400",420]},"steps":[{"launch":true},{"shot":"first"}]})
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir(sb.dir.join("existing")).unwrap();
    std::fs::write(sb.dir.join("existing/first.png"), b"prior evidence").unwrap();
    let out = run(
        &sb,
        &url,
        &[
            "devices",
            "run",
            "--app",
            "app.apk",
            "--scenario",
            "scenario.json",
            "--size",
            "phone",
            "--out",
            "existing",
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(2));
    assert!(log.lock().is_empty());
    assert_eq!(
        std::fs::read(sb.dir.join("existing/first.png")).unwrap(),
        b"prior evidence"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn device_log_snapshot_is_lossless_or_fails_closed() {
    const BLOCK: usize = 512 * 1024;
    let logcat: Vec<u8> = (0..(5 * BLOCK + 97))
        .map(|i| ((i ^ (i >> 8) ^ (i >> 17)) % 256) as u8)
        .collect();
    let crash = "separate crash buffer\n";
    for (case, error, expected_reads) in [
        ("complete", "", 6),
        ("wrong-hash", "SHA-256 mismatch", 6),
        ("truncated", "the API truncated", 2),
        ("short", "incorrect log chunk length", 2),
        ("failed", "did not succeed", 2),
        ("oversize", "32 MiB limit", 0),
    ] {
        let sb = Sandbox::new(&format!("device-logs-{case}"));
        std::fs::write(sb.dir.join("app.apk"), b"APK fixture").unwrap();
        let lease = format!("{ENV}/leases/logs");
        let mut replies = vec![
            (
                200,
                json!({"name":lease,"spec":{"device":{"os_version":"14"}},"status":{"state":"ready"}}),
            ),
            (200, json!({})), // APK upload
            (200, json!({"package_name":"com.example.app"})),
            exec_reply(""), // clear before launch
            exec_reply("Events injected: 1"),
            exec_reply("1234"), // pidof at collection
            if case == "oversize" {
                exec_reply(&format!("{MAIN_LOG} {} {:064x}\n", 32 * 1024 * 1024 + 1, 0))
            } else {
                snapshot_metadata(MAIN_LOG, &logcat)
            },
        ];
        for (index, chunk) in logcat.chunks(BLOCK).take(expected_reads).enumerate() {
            replies.push(match (case, index) {
                ("wrong-hash", 1) => {
                    let mut corrupt = chunk.to_vec();
                    corrupt[0] ^= 1;
                    exec_bytes(&corrupt, false)
                }
                // Even a correctly sized reply cannot pass a truncation flag.
                ("truncated", 1) => exec_bytes(chunk, true),
                ("short", 1) => exec_bytes(&chunk[..chunk.len() - 1], false),
                ("failed", 1) => (200, json!({"exit_code":1})),
                _ => exec_bytes(chunk, false),
            });
        }
        replies.push(exec_reply("")); // rm main snapshot, including on failure
        replies.extend(small_snapshot(CRASH_LOG, crash));
        if !error.is_empty() {
            replies.push(exec_reply("diagnostic only"));
        }
        replies.extend([
            screenshot(1080, 2400),
            (200, json!({"name":lease,"status":{"state":"ending"}})),
        ]);
        let (url, log) = serve(replies).await;
        let out = run(
            &sb,
            &url,
            &[
                "devices",
                "run",
                "--app",
                "app.apk",
                "--env",
                ENV,
                "--wait",
                "0s",
                "--screenshot-every",
                "0s",
                "--out",
                "capture",
                "-o",
                "json",
            ],
        )
        .await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(if error.is_empty() { 0 } else { 2 }),
            "{case}: {stderr}"
        );
        let capture = sb.dir.join("capture");
        let result: Value =
            serde_json::from_slice(&std::fs::read(capture.join("result.json")).unwrap()).unwrap();
        assert_eq!(
            result["log_truncation"],
            json!({"logcat":!error.is_empty(),"crash":false}),
            "{case}"
        );
        if error.is_empty() {
            assert_eq!(std::fs::read(capture.join("logcat.txt")).unwrap(), logcat);
        } else {
            assert!(
                result["error"].as_str().unwrap().contains(error),
                "{case}: {result}"
            );
            assert!(!capture.join("logcat.txt").exists(), "{case}");
            assert!(capture.join("logcat-tail.txt").is_file(), "{case}");
        }
        assert_eq!(
            std::fs::read(capture.join("crash.txt")).unwrap(),
            crash.as_bytes()
        );
        let commands: Vec<Value> = log
            .lock()
            .iter()
            .filter(|row| row.line.contains(":exec"))
            .map(|row| serde_json::from_str::<Value>(&row.body).unwrap()["command"].clone())
            .collect();
        assert_eq!(
            commands[0],
            json!(["logcat", "-b", "main,system,crash", "-c"])
        );
        assert_eq!(commands[1][0], "monkey");
        let input = format!("if={MAIN_LOG}");
        let reads: Vec<_> = commands
            .iter()
            .filter(|argv| argv[0] == "dd" && argv[1] == input)
            .cloned()
            .collect();
        let expected: Vec<_> = (0..expected_reads)
            .map(|i| {
                json!([
                    "dd",
                    input,
                    format!("bs={BLOCK}"),
                    format!("skip={i}"),
                    "count=1"
                ])
            })
            .collect();
        assert_eq!(reads, expected, "{case}");
        assert!(commands.contains(&json!(["rm", "-f", MAIN_LOG])), "{case}");
    }
}
