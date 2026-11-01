//! Native capture transport checks reuse the CLI's ordinary fake public API.
use super::*;
use base64::Engine;

fn exec_reply(stdout: &str) -> (u16, Value) {
    (
        200,
        json!({"exit_code":0,"stdout":base64::engine::general_purpose::STANDARD.encode(stdout)}),
    )
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
            exec_reply(""), // clear logcat
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
        if failure == "foreground" || failure == "none" {
            replies.push((
                200,
                json!({"exit_code":0,"truncated":true,
                "stdout":base64::engine::general_purpose::STANDARD.encode("early boot noise")}),
            ));
        } else {
            replies.push(exec_reply("quiet"));
        }
        replies.push(exec_reply("")); // crash buffer
        if failure == "none" {
            replies.push(exec_reply("quiet")); // capped successful capture needs only its log tail
        }
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
                    std::fs::read_to_string(sb.dir.join("captures/logcat-tail.txt")).unwrap(),
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
                assert_eq!(
                    std::fs::read_to_string(sb.dir.join("captures/logcat.txt")).unwrap(),
                    "early boot noise"
                );
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
