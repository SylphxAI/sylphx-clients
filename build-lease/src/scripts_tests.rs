use super::{start_script, Startup, COMMAND_STARTED, PROVISION, RUN};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "sylphx-guest-scripts-{}-{}",
            std::process::id(),
            crate::guest::rand_u64(),
        ));
        let workspace = Self(path);
        for directory in ["tree", ".sylphx", ".cargo", "bin"] {
            std::fs::create_dir_all(workspace.0.join(directory)).unwrap();
        }
        workspace.file(".cargo/config.toml", "stale\n");
        workspace.executable("rustup", "echo which >> \"$TEST_WORKSPACE/rustup-calls\"\n[ \"$RUSTUP_HOME\" = \"${TEST_RUSTUP_HOME:-/opt/rustup}\" ]\n");
        workspace.executable("curl", "echo probe >> \"$TEST_WORKSPACE/mirror-calls\"\n[ \"${TEST_BLOCK_CACHE:-0}\" = 1 ] && sleep 30\nexit \"${TEST_MIRROR_STATUS:-0}\"\n");
        workspace.executable("cargo", "if [ -f \"$TEST_WORKSPACE/.cargo/config.toml\" ]; then cat \"$TEST_WORKSPACE/.cargo/config.toml\"; else echo direct; fi\n");
        workspace.executable("sccache", "if [ \"$1\" = --start-server ]; then\n  echo start >> \"$TEST_WORKSPACE/cache-calls\"\n  [ \"${TEST_BLOCK_CACHE:-0}\" = 1 ] && sleep 30\n  if [ \"${TEST_CACHE_STATUS:-0}\" != 0 ]; then echo 'cache storage failed to read' >&2; exit \"$TEST_CACHE_STATUS\"; fi\n  exit 0\nfi\necho cached >> \"$TEST_WORKSPACE/cache-calls\"\nexec \"$@\"\n");
        workspace.executable("compiler", "echo compile >> \"$TEST_WORKSPACE/compiler-calls\"\nprintf 'compiled\\n'\nexit \"${TEST_COMPILE_STATUS:-0}\"\n");
        workspace
    }

    fn file(&self, relative: &str, body: &str) {
        std::fs::write(self.0.join(relative), body).unwrap();
    }

    fn executable(&self, name: &str, body: &str) {
        let path = self.0.join("bin").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, script: &str, command: &str) -> Command {
        let mut process = Command::new("/bin/sh");
        process
            .args(["-c", script, "sylphx-test"])
            .arg(&self.0)
            .args([".", "1", "/bin/sh", "-c", command])
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .env("TEST_WORKSPACE", &self.0)
            .env("SYLPHX_BUILD_SOURCE_HISTORY", "1")
            .env("SCCACHE_WEBDAV_ENDPOINT", "https://cache.test/sccache");
        process
    }

    fn run(&self, command: &str, env: &[(&str, &str)]) -> Output {
        self.command(RUN, command)
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }

    fn calls(&self, name: &str) -> usize {
        std::fs::read_to_string(self.0.join(name))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn startup_filters_every_marker_split_without_changing_output_bytes() {
    let marker = COMMAND_STARTED.as_bytes();
    for split in 0..=marker.len() {
        let mut startup = Startup::default();
        let mut result = startup.push(b"\xffsetup\x00").into_owned();
        result.extend_from_slice(&startup.push(&marker[..split]));
        result.extend_from_slice(&startup.push(&marker[split..]));
        result.extend_from_slice(&startup.push(b"\xfecommand\x00"));
        result.extend(startup.finish());
        assert!(startup.started());
        assert_eq!(result, b"\xffsetup\x00\xfecommand\x00");
    }
}

#[test]
fn startup_forwards_incomplete_and_false_marker_prefixes() {
    let marker = COMMAND_STARTED.as_bytes();
    for length in 1..marker.len() {
        let mut startup = Startup::default();
        assert!(startup.push(&marker[..length]).is_empty());
        assert_eq!(startup.finish(), marker[..length]);
        assert!(!startup.started());
        let mut startup = Startup::default();
        let mut input = marker[..length].to_vec();
        input.push(0xff);
        let mut output = startup.push(&input).into_owned();
        output.extend(startup.finish());
        assert_eq!(output, input);
        assert!(!startup.started());
    }
}

#[test]
fn setup_failure_and_command_exit_125_are_distinct() {
    let workspace = Workspace::new();
    for (setup, provision, command, started) in [
        (
            "printf 'setup diagnostic' >&2; false; printf 'must not run'",
            true,
            "echo command",
            false,
        ),
        ("cd /; export RUSTC_WRAPPER=lost", false, "exit 125", true),
    ] {
        let output = workspace
            .command(&start_script(setup, provision), command)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125));
        let mut startup = Startup::default();
        let mut stderr = startup.push(&output.stderr).into_owned();
        stderr.extend(startup.finish());
        assert_eq!(startup.started(), started);
        if !started {
            assert_eq!(stderr, b"setup diagnostic");
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn setup_runs_before_provision_and_failed_provision_never_starts_command() {
    let workspace = Workspace::new();
    workspace.executable(
        "rustup",
        "echo provision >> \"$TEST_WORKSPACE/rustup-calls\"; exit 1\n",
    );
    let script = start_script("printf '[workspace]\\n' > \"$W/tree/Cargo.toml\"", true);
    let output = workspace.command(&script, "echo command").output().unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(output.stdout.is_empty());
    assert!(workspace.calls("rustup-calls") > 0);
    let mut startup = Startup::default();
    startup.push(&output.stderr);
    assert!(!startup.started());
}

#[test]
fn provisioning_timeout_is_bounded_and_maps_to_startup_failure() {
    let workspace = Workspace::new();
    workspace.executable(
        "timeout",
        "echo \"$1\" > \"$TEST_WORKSPACE/timeout\"; exit 124\n",
    );
    let output = workspace
        .command(&start_script(":", true), "echo command")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("timeout")).unwrap(),
        "900\n"
    );
    let mut startup = Startup::default();
    startup.push(&output.stderr);
    assert!(!startup.started());
}

#[test]
fn run_preparation_failure_does_not_emit_started() {
    let workspace = Workspace::new();
    let output = workspace
        .command(&start_script("rm -rf \"$W/tree\"", false), "echo command")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    let mut startup = Startup::default();
    startup.push(&output.stderr);
    assert!(!startup.started());
}

#[test]
fn non_rust_provision_never_invokes_rustup() {
    let workspace = Workspace::new();
    workspace.file("tree/package.json", "{}");
    let output = workspace
        .command(PROVISION, "echo command")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(workspace.calls("rustup-calls"), 0);
}

#[test]
fn noncompile_first_byte_never_waits_for_blocked_cache_or_mirror() {
    for (command, expected) in [
        ("printf x", b'x'),
        ("cargo --version", b'c'),
        ("cargo version", b'c'),
    ] {
        let workspace = Workspace::new();
        workspace.executable("cargo", "exec \"$TEST_REAL_CARGO\" \"$@\"\n");
        let mut child = workspace
            .command(&start_script(":", false), command)
            .env("TEST_BLOCK_CACHE", "1")
            .env("TEST_REAL_CARGO", env!("CARGO"))
            .env(
                "SYLPHX_CRATES_MIRROR",
                "sparse+https://cache.test/crates/index/",
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut byte = [0];
            sender
                .send(stdout.read_exact(&mut byte).map(|()| byte[0]))
                .unwrap();
        });
        let first = receiver.recv_timeout(Duration::from_secs(3));
        if first.is_err() {
            let _ = child.kill();
        }
        let output = child.wait_with_output().unwrap();
        reader.join().unwrap();
        assert_eq!(first.unwrap().unwrap(), expected);
        assert!(output.status.success());
        assert_eq!(workspace.calls("cache-calls"), 0);
        assert_eq!(workspace.calls("mirror-calls"), 0);
    }
}

#[test]
fn concurrent_compilers_share_one_outage_warning_and_start_attempt() {
    let workspace = Workspace::new();
    let output = workspace.run(
        "for n in 1 2 3 4 5 6 7 8; do \"$RUSTC_WRAPPER\" compiler & done; wait",
        &[("TEST_CACHE_STATUS", "2")],
    );
    assert!(output.status.success());
    assert_eq!(workspace.calls("compiler-calls"), 8);
    assert_eq!(workspace.calls("cache-calls"), 1);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .matches("sylphx: warning: the build cache")
            .count(),
        1
    );
}
#[test]
fn unreachable_gateway_bypasses_even_a_silently_fail_open_sccache_server() {
    let workspace = Workspace::new();
    let output = workspace.run(
        "for n in 1 2 3 4; do \"$RUSTC_WRAPPER\" compiler & done; wait",
        &[("TEST_MIRROR_STATUS", "7")],
    );
    assert!(output.status.success());
    assert_eq!(workspace.calls("compiler-calls"), 4);
    assert_eq!(workspace.calls("cache-calls"), 0);
    assert_eq!(workspace.calls("mirror-calls"), 1);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .matches("sylphx: warning:")
            .count(),
        1
    );
}

#[test]
fn successful_compiler_metadata_queries_do_not_consume_the_outage_warning() {
    let workspace = Workspace::new();
    let output = workspace.run(
        "\"$RUSTC_WRAPPER\" compiler -vV 2> \"$TEST_WORKSPACE/query-stderr\"; \"$RUSTC_WRAPPER\" compiler",
        &[("TEST_CACHE_STATUS", "2")],
    );
    assert!(output.status.success());
    assert!(std::fs::read(workspace.0.join("query-stderr"))
        .unwrap()
        .is_empty());
    assert_eq!(workspace.calls("compiler-calls"), 2);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .matches("sylphx: warning:")
            .count(),
        1
    );
}

#[test]
fn compiler_errors_are_never_retried_with_or_without_cache() {
    for (status, cache_calls, warnings) in [("0", 2, 0), ("2", 1, 1)] {
        let workspace = Workspace::new();
        let output = workspace.run(
            "\"$RUSTC_WRAPPER\" compiler",
            &[("TEST_COMPILE_STATUS", "23"), ("TEST_CACHE_STATUS", status)],
        );
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(workspace.calls("compiler-calls"), 1);
        assert_eq!(workspace.calls("cache-calls"), cache_calls);
        assert_eq!(
            String::from_utf8_lossy(&output.stderr)
                .matches("sylphx: warning:")
                .count(),
            warnings,
        );
    }
}

#[test]
fn compiler_wrapper_preserves_cargo_environment_arguments_and_exit_codes() {
    // Cargo creates these names natively; a shell cannot export the hyphenated
    // names. Neither receiver may use /bin/sh, which would hide this regression.
    let environment = [
        ("CARGO_BIN_EXE_site-pack", "/tmp/site pack\nproducer"),
        ("CARGO_BIN_EXE_site-server", "/tmp/site-server"),
        ("CARGO_BIN_EXE_siteservercontrol", "underscore control"),
        ("CARGO_BIN_EXE_a.b+工具", "punctuation and unicode"),
    ];
    let expected_environment = serde_json::to_value(
        environment
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
    )
    .unwrap();
    let arguments = ["", "with spaces", "quote'\"", "line\nbreak", "--flag=*"];
    for cache_state in ["ready", "failed", "start", "outage"] {
        for exit in [0, 23] {
            let workspace = Workspace::new();
            let output = workspace.run(":", &[]);
            assert!(output.status.success());
            let state = workspace.0.join(".sylphx/lazy-cache");
            if matches!(cache_state, "ready" | "failed") {
                workspace.file(&format!(".sylphx/lazy-cache/{cache_state}"), "");
            }
            workspace.file(
                "bin/compiler-probe",
                r#"#!/usr/bin/python3
import json, os, sys
print(json.dumps({"environment": {k: v for k, v in os.environ.items() if k.startswith("CARGO_BIN_EXE_")}, "arguments": sys.argv[1:]}))
sys.exit(int(os.environ["TEST_COMPILE_STATUS"]))
"#,
            );
            workspace.file(
                "bin/sccache-probe",
                r#"#!/usr/bin/python3
import json, os, sys
with open(os.environ["TEST_WORKSPACE"] + "/cache-entry", "a") as output:
    output.write(json.dumps({"environment": {k: v for k, v in os.environ.items() if k.startswith("CARGO_BIN_EXE_")}, "arguments": sys.argv[1:]}) + "\n")
if sys.argv[1:] == ["--start-server"]:
    sys.exit(int(os.environ["TEST_CACHE_STATUS"]))
assert os.environ["SCCACHE_IGNORE_SERVER_IO_ERROR"] == "1"
os.execv(sys.argv[1], sys.argv[1:])
"#,
            );
            for name in ["compiler-probe", "sccache-probe"] {
                std::fs::set_permissions(
                    workspace.0.join("bin").join(name),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            let compiler = workspace.0.join("bin/compiler-probe");
            let cache = workspace.0.join("bin/sccache-probe");
            let mut command = Command::new(state.join("bin/rustc-wrapper"));
            command
                .arg(&compiler)
                .args(arguments)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .envs(environment)
                .env("TEST_WORKSPACE", &workspace.0)
                .env("TEST_COMPILE_STATUS", exit.to_string())
                .env(
                    "TEST_CACHE_STATUS",
                    if cache_state == "outage" { "2" } else { "0" },
                )
                .env("SYLPHX_BUILD_CACHE_STATE", &state)
                .env("SYLPHX_BUILD_SCCACHE", &cache);
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(exit), "{cache_state}");
            let entry: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(entry["environment"], expected_environment, "{cache_state}");
            assert_eq!(entry["arguments"], serde_json::json!(arguments));
            let entries =
                std::fs::read_to_string(workspace.0.join("cache-entry")).unwrap_or_default();
            let entries: Vec<serde_json::Value> = entries
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                entries.len(),
                match cache_state {
                    "ready" | "outage" => 1,
                    "start" => 2,
                    "failed" => 0,
                    _ => unreachable!(),
                }
            );
            for entry in entries {
                assert_eq!(entry["environment"], expected_environment, "{cache_state}");
                if entry["arguments"] != serde_json::json!(["--start-server"]) {
                    let mut delegated = vec![compiler.to_str().unwrap()];
                    delegated.extend(arguments);
                    assert_eq!(entry["arguments"], serde_json::json!(delegated));
                }
            }
        }
    }
}

#[test]
fn explicit_wrappers_including_empty_always_win() {
    for wrapper in ["mine", ""] {
        let workspace = Workspace::new();
        let output = workspace.run(
            "printf '[%s]' \"$RUSTC_WRAPPER\"",
            &[("RUSTC_WRAPPER", wrapper), ("TEST_CACHE_STATUS", "2")],
        );
        assert!(output.status.success());
        assert_eq!(output.stdout, format!("[{wrapper}]").as_bytes());
        assert_eq!(workspace.calls("cache-calls"), 0);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn incremental_and_ephemeral_no_cache_commands_do_not_install_wrapper() {
    for env in [
        vec![("CARGO_INCREMENTAL", "1")],
        vec![("SCCACHE_WEBDAV_ENDPOINT", "")],
    ] {
        let workspace = Workspace::new();
        let output = workspace.run("printf '[%s]' \"${RUSTC_WRAPPER-unset}\"", &env);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"[unset]");
        assert_eq!(workspace.calls("cache-calls"), 0);
    }
}

#[test]
fn cargo_version_and_non_rust_package_manager_leave_mirror_unprobed() {
    for command in [
        "cargo --version",
        "cargo -V",
        "cargo +stable --version",
        "cargo version",
        "cargo version --verbose",
        "cargo help check",
        "cargo --help",
        "cargo --list",
        "cargo",
        "printf '%s' \"$NPM_CONFIG_REGISTRY\"",
    ] {
        let workspace = Workspace::new();
        let output = workspace.run(
            command,
            &[
                (
                    "SYLPHX_CRATES_MIRROR",
                    "sparse+https://cache.test/crates/index/",
                ),
                ("NPM_CONFIG_REGISTRY", "https://npm.test/"),
            ],
        );
        assert!(output.status.success());
        assert_eq!(workspace.calls("mirror-calls"), 0);
        if command.starts_with("printf") {
            assert_eq!(output.stdout, b"https://npm.test/");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cargo_native_alias_routes_dependency_resolution_through_the_mirror() {
    assert_native_cargo_mirror("cargo ci").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cargo_native_global_option_value_routes_dependency_resolution_through_the_mirror() {
    assert_native_cargo_mirror("cargo --config net.retry=0 check").await;
}

async fn assert_native_cargo_mirror(command: &str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let workspace = Workspace::new();
    std::fs::create_dir_all(workspace.0.join("tree/.cargo")).unwrap();
    std::fs::create_dir_all(workspace.0.join("tree/src")).unwrap();
    workspace.file("tree/.cargo/config.toml", "[alias]\nci = \"check\"\n");
    workspace.file(
        "tree/Cargo.toml",
        "[package]\nname = \"mirror-consumer\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nsylphx-mirror-probe = \"=0.0.1\"\n",
    );
    workspace.file("tree/src/lib.rs", "");
    workspace.executable("cargo", "exec \"$TEST_REAL_CARGO\" \"$@\"\n");

    // An empty sparse registry proves that Cargo itself selects the mirror:
    // resolution must reach its package index, not just inspect generated TOML.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mirror = format!("sparse+http://{address}/");
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let length = stream.read(&mut buffer).await.unwrap();
                if length == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..length]);
            }
            let request = String::from_utf8(request).unwrap();
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            let (status, body) = if path == "/config.json" {
                ("200 OK", format!(r#"{{"dl":"http://{address}/unused"}}"#))
            } else {
                ("404 Not Found", String::new())
            };
            requests.send(path).unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len(),
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });

    let cargo = PathBuf::from(env!("CARGO"));
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".rustup"));
    let output = workspace
        .command(RUN, command)
        .env("TEST_REAL_CARGO", &cargo)
        .env("RUSTC", cargo.parent().unwrap().join("rustc"))
        .env("HOME", home)
        .env("RUSTUP_HOME", rustup_home)
        .env("RUSTC_WRAPPER", "")
        .env("SYLPHX_CRATES_MIRROR", mirror)
        .env("CARGO_HTTP_TIMEOUT", "5")
        .env("CARGO_NET_RETRY", "0")
        .output()
        .unwrap();
    server.abort();
    let mut paths = Vec::new();
    while let Ok(path) = received.try_recv() {
        paths.push(path);
    }
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no matching package named"), "{stderr}");
    assert!(stderr.contains("sylphx-mirror-probe"), "{stderr}");
    assert!(paths.iter().any(|path| path == "/config.json"), "{paths:?}");
    assert!(
        paths
            .iter()
            .any(|path| path == "/sy/lp/sylphx-mirror-probe"),
        "{paths:?}",
    );
    assert_eq!(workspace.calls("cache-calls"), 0);
}

#[test]
fn toolchain_selection_keeps_the_template_workspace_and_explicit_choices() {
    let workspace = Workspace::new();
    let output = workspace.run("printf '%s' \"$RUSTUP_HOME\"", &[]);
    assert_eq!(output.stdout, b"/opt/rustup");
    let output = workspace.run(
        "printf '%s' \"$RUSTUP_HOME\"",
        &[("TEST_RUSTUP_HOME", "unavailable")],
    );
    assert_eq!(
        output.stdout,
        workspace.0.join("rustup").to_str().unwrap().as_bytes()
    );
    let output = workspace.run(
        "printf '%s' \"$RUSTUP_HOME\"",
        &[("RUSTUP_HOME", "/custom")],
    );
    assert_eq!(output.stdout, b"/custom");
}

#[test]
fn durable_admission_exists_before_command_and_not_after_setup_failure() {
    for (setup, admitted) in [(":", true), ("exit 3", false)] {
        let workspace = Workspace::new();
        let state = workspace.0.join("admission");
        let output = workspace
            .command(
                &start_script(setup, false),
                "test -s \"$SYLPHX_BUILD_ACTION_STATE\" && exit 125",
            )
            .env("SYLPHX_BUILD_ACTION_STATE", &state)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125));
        assert_eq!(state.exists(), admitted);
        if admitted {
            let uptime: f64 = std::fs::read_to_string(&state)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert!(uptime >= 0.0 && uptime.is_finite());
        }
        let mut startup = Startup::default();
        startup.push(&output.stderr);
        assert_eq!(startup.started(), admitted);
    }
}

#[test]
fn standalone_run_preserves_command_exit_codes_without_protocol_marker() {
    let workspace = Workspace::new();
    for code in [0, 1, 23, 101, 125] {
        let output = workspace.run(&format!("exit {code}"), &[]);
        assert_eq!(output.status.code(), Some(code));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn absent_mirror_removes_stale_config_without_a_probe() {
    let workspace = Workspace::new();
    let output = workspace.run("cargo build", &[]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"direct\n");
    assert_eq!(workspace.calls("mirror-calls"), 0);
}

#[test]
fn ordinary_stderr_is_borrowed_before_and_after_start() {
    let mut startup = Startup::default();
    assert!(matches!(
        startup.push(b"ordinary stderr"),
        std::borrow::Cow::Borrowed(_)
    ));
    startup.push(COMMAND_STARTED.as_bytes());
    assert!(matches!(
        startup.push(b"command stderr"),
        std::borrow::Cow::Borrowed(_)
    ));
}
