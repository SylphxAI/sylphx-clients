//! Exercise the installed command boundary without a live backend or user credentials.

use std::{
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

fn apply(file: &str, text: &str, offline: bool) -> Output {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "sylphx-apply-preflight-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join(file), text).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sylphx"));
    cmd.env_remove("SYLPHX_API_KEY")
        .env("SYLPHX_CONFIG_DIR", &dir)
        .env("SYLPHX_NO_KEYCHAIN", "1")
        .current_dir(&dir)
        .args([
            "--base-url",
            "http://127.0.0.1:1",
            "apply",
            "--manifest",
            file,
            "-o",
            "json",
        ]);
    if offline {
        cmd.arg("--offline");
    }
    let output = cmd.output().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    output
}

fn assert_empty_report(out: Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"{\"resources\":[],\"unsettled\":[]}\n");
}

fn assert_refused(out: Output, detail: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(out.stdout.is_empty());
    assert!(stderr.contains(detail), "{stderr}");
}

const INVALID_MODE: &str = "[[services]]\nname = 'web'\nenv_isolation = 'not-a-valid-mode'\n";

#[test]
fn offline_refuses_invalid_mode_missing_env_and_invalid_override() {
    for (text, detail) in [
        (INVALID_MODE, "service web: env_isolation must be \"inherit\" or \"strict\""),
        ("[[services]]\nname = 'web'\nenv_isolation = 'strict'\n", "without an env list"),
        ("[[services]]\nname = 'web'\nenv_isolation = 'strict'\nenv = []\n[[environments.preview.services]]\nname = 'web'\nenv = 'NOT_A_LIST'\n", "env must be a list"),
    ] {
        assert_refused(apply("sylphx.toml", text, true), detail);
    }
}

#[test]
fn offline_accepts_strict_empty_and_fieldwise_environment_overrides() {
    let text = r#"
[[services]]
name = 'edge'
env_isolation = 'strict'
env = []

[[services]]
name = 'web'
env = ['PUBLIC_API_URL']

[[environments.production.services]]
name = 'web'
env_isolation = 'strict'

[[environments.preview.services]]
name = 'web'
env_isolation = 'strict'
env = []

[[environments.preview.services]]
name = 'edge'
env_isolation = 'inherit'
"#;
    assert_empty_report(apply("sylphx.toml", text, true));
}

#[test]
fn offline_still_checks_resource_declarations() {
    assert_refused(
        apply(
            "sylphx.toml",
            "[[resource]]\nkind = 'hosting.sylphx.com/Service'\nname = 'main'\n",
            true,
        ),
        "is written by",
    );
}

#[test]
fn json_declarations_and_online_apply_keep_resource_only_contract() {
    assert_empty_report(apply(
        "declarations.json",
        r#"{"services":[{"name":"web","env_isolation":"not-a-valid-mode"}]}"#,
        true,
    ));
    // The online path must reach client resolution, not reject an unrelated service.
    assert_refused(apply("sylphx.toml", INVALID_MODE, false), "not signed in");
}
