//! Exercise the guest prelude with Cargo, a real sparse registry and a locked
//! crate: the paths a registry is asked for say which configured source Cargo
//! used.
use std::io::{BufRead, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

/// The command every test runs: the real Cargo fetches the locked dependency
/// through whatever source the guest configured, with the machine's toolchain.
const FETCH: &str = "RUSTUP_HOME=\"$REAL_RUSTUP_HOME\" \"$REAL_CARGO\" fetch --locked";

struct Scratch(PathBuf);

impl Scratch {
    /// A fresh scratch root, named for the test that owns it.
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "build-registry-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `cached-fixture` crate a registry serves: its `.crate` archive and the
/// checksum both its index entry and the workspace's lock carry.
fn fixture(root: &Path) -> (String, Vec<u8>) {
    let package = root.join("cached-fixture-0.1.0");
    std::fs::create_dir_all(package.join("src")).unwrap();
    std::fs::write(
        package.join("Cargo.toml"),
        "[package]\nname='cached-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(package.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").unwrap();
    let archive = root.join("fixture.crate");
    assert!(Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .arg("-C")
        .arg(root)
        .arg("cached-fixture-0.1.0")
        .status()
        .unwrap()
        .success());
    let sum = Command::new("sha256sum").arg(&archive).output().unwrap();
    assert!(sum.status.success());
    let checksum = String::from_utf8(sum.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    (checksum, std::fs::read(archive).unwrap())
}

/// The consumer workspace at `w`: one dependency on the fixture, locked to the
/// crates.io source a replacement redirects. Answers the lock's text.
fn consumer(w: &Path, checksum: &str) -> String {
    std::fs::create_dir_all(w.join("tree/src")).unwrap();
    std::fs::create_dir_all(w.join(".sylphx")).unwrap();
    std::fs::write(
        w.join("tree/Cargo.toml"),
        "[package]\nname='consumer'\nversion='0.1.0'\nedition='2021'\n[dependencies]\ncached-fixture='=0.1.0'\n[workspace]\nresolver='2'\n",
    )
    .unwrap();
    std::fs::write(
        w.join("tree/src/lib.rs"),
        "pub use cached_fixture::answer;\n",
    )
    .unwrap();
    let lock = format!(
        "version = 3\n\n[[package]]\nname = \"cached-fixture\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n\n[[package]]\nname = \"consumer\"\nversion = \"0.1.0\"\ndependencies = [\"cached-fixture\"]\n"
    );
    std::fs::write(w.join("tree/Cargo.lock"), &lock).unwrap();
    lock
}

/// A local sparse registry. It serves the fixture under **any** path prefix, so
/// the paths it is asked for say which configured source Cargo used, and
/// records every request; anything else is answered 404, which fails the fetch
/// it was serving.
struct Mirror {
    base: String,
    stop: Arc<AtomicBool>,
    server: std::thread::JoinHandle<Vec<String>>,
}

impl Mirror {
    fn start(checksum: &str, archive: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let index = serde_json::json!({"name":"cached-fixture", "vers":"0.1.0", "deps":[], "cksum":checksum, "features":{}, "yanked":false}).to_string() + "\n";
        let config =
            serde_json::json!({"dl": format!("{base}/files/{{crate}}/{{version}}/download")})
                .to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let end = stop.clone();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(40);
            while !end.load(Ordering::Relaxed) && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut header = String::new();
                loop {
                    header.clear();
                    assert_ne!(reader.read_line(&mut header).unwrap(), 0);
                    if header == "\r\n" {
                        break;
                    }
                }
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                requests.push(path.clone());
                let body: Option<&[u8]> = if path.ends_with("/config.json") {
                    Some(config.as_bytes())
                } else if path.ends_with("/ca/ch/cached-fixture") {
                    Some(index.as_bytes())
                } else if path.ends_with("/files/cached-fixture/0.1.0/download") {
                    Some(&archive)
                } else {
                    None
                };
                match body {
                    Some(body) => {
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .unwrap();
                        stream.write_all(body).unwrap();
                    }
                    None => write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap(),
                }
            }
            requests
        });
        Self { base, stop, server }
    }

    /// Stops serving and answers every path requested, in order.
    fn requests(self) -> Vec<String> {
        self.stop.store(true, Ordering::Relaxed);
        self.server.join().unwrap()
    }
}

/// Runs [`sylphx_build_lease::scripts::RUN`] over the workspace `w` with
/// `cargo_home` holding the only Cargo configuration and `mirror` as
/// `SYLPHX_CRATES_MIRROR`, and answers the result. `curl` on `PATH` always
/// fails: routing that still needed a network preflight would be seen.
fn run(
    w: &Path,
    root: &Path,
    cargo_home: &Path,
    mirror: &str,
    index: Option<&str>,
) -> std::process::Output {
    run_command(w, root, cargo_home, mirror, index, FETCH)
}

fn run_command(
    w: &Path,
    root: &Path,
    cargo_home: &Path,
    mirror: &str,
    index: Option<&str>,
    script: &str,
) -> std::process::Output {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let tool = bin.join("curl");
    std::fs::write(&tool, "#!/bin/sh\nexit 7\n").unwrap();
    std::fs::set_permissions(tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new("timeout");
    command
        .args([
            "30",
            "sh",
            "-c",
            sylphx_build_lease::scripts::RUN,
            "sylphx-run",
        ])
        .arg(w)
        .current_dir(root)
        .args([".", "1", "sh", "-c", script])
        .env_clear()
        .env("HOME", root.join("home"))
        .env("PATH", path)
        .env("REAL_CARGO", cargo)
        .env(
            "REAL_RUSTUP_HOME",
            std::env::var("RUSTUP_HOME")
                .unwrap_or_else(|_| format!("{}/.rustup", std::env::var("HOME").unwrap())),
        )
        .env("SYLPHX_CRATES_MIRROR", mirror)
        .env("CARGO_HOME", cargo_home)
        .env("EXTERNAL_PACKAGE", root.join("outside/tree"))
        .env("CARGO_NET_RETRY", "0")
        .env("CARGO_HTTP_TIMEOUT", "5")
        .env("CARGO_INCREMENTAL", "1")
        .env_remove("RUSTC_WRAPPER");
    if let Some(index) = index {
        command.env("CARGO_REGISTRIES_CRATES_IO_INDEX", index);
    }
    command.output().unwrap()
}

#[test]
fn locked_cargo_fetch_stays_on_the_mirror_without_a_preflight() {
    let root = Scratch::new("preflight");
    let w = root.0.join("ws");
    let (checksum, archive) = fixture(&root.0);
    let lock = consumer(&w, &checksum);
    let cargo_home = root.0.join("custom-cargo");
    let mirror = Mirror::start(&checksum, archive);
    let out = run(
        &w,
        &root.0,
        &cargo_home,
        &format!("sparse+{}/index/", mirror.base),
        None,
    );
    let requests = mirror.requests();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(w.join("tree/Cargo.lock")).unwrap(),
        lock
    );
    assert!(
        requests.iter().any(|p| p == "/index/ca/ch/cached-fixture"),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|p| p == "/files/cached-fixture/0.1.0/download"),
        "{requests:?}"
    );
    assert!(cargo_home.join("registry/cache").exists());
    assert!(!w.join("cargo/registry").exists());
}

/// A Cargo home configuration that does not replace crates-io must not stop the
/// mirror from routing the fetch; its existing settings remain unchanged:
/// unrelated settings, a `[registries.crates-io]` protocol, another source
/// whose URL names crates-io, or a source merely prefixed with `crates-io` are
/// not replacements. Cargo's preference for a
/// legacy `config` over `config.toml` are exercised.
#[test]
fn an_unrelated_cargo_home_config_keeps_the_mirror_routing() {
    for (label, name, config) in [
        (
            "unrelated-settings",
            "config.toml",
            "[build]\nrustflags = [\"--cfg\", \"custom\"]\n\n[registries.crates-io]\nprotocol = \"sparse\"\n",
        ),
        ("legacy-file", "config", "[build]\njobs = 2\n"),
        (
            "source-mentioning-crates-io",
            "config.toml",
            "source = { sylphx-mirror = { registry = \"sparse+http://127.0.0.1:9/crates-io/index/\" } }\n\n[build]\nrustflags = [\"--cfg\", \"custom\"]\n",
        ),
        (
            "source-prefixed-with-crates-io",
            "config.toml",
            "[build]\nrustflags = [\"--cfg\", \"custom\"]\n\n[source.crates-io-vendor]\nregistry = \"sparse+http://127.0.0.1:9/crates-io/index/\"\n",
        ),
    ] {
        let root = Scratch::new("unrelated");
        let w = root.0.join("ws");
        let (checksum, archive) = fixture(&root.0);
        let lock = consumer(&w, &checksum);
        let cargo_home = root.0.join("custom-cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        std::fs::write(cargo_home.join(name), config).unwrap();
        let mirror = Mirror::start(&checksum, archive);
        let out = run(
            &w,
            &root.0,
            &cargo_home,
            &format!("sparse+{}/managed/", mirror.base),
            None,
        );
        let requests = mirror.requests();
        let case = label;
        assert!(
            out.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(w.join("tree/Cargo.lock")).unwrap(),
            lock,
            "{case}"
        );
        assert!(
            requests
                .iter()
                .any(|p| p == "/managed/ca/ch/cached-fixture"),
            "{case}: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|p| p == "/files/cached-fixture/0.1.0/download"),
            "{case}: {requests:?}"
        );
        let preserved = Command::new("python3")
                .args(["-c", "import sys,tomllib; old=tomllib.loads(sys.argv[1]); new=tomllib.load(open(sys.argv[2],'rb')); assert all(new[k] == v if k != 'source' else all(new[k][s] == x for s,x in v.items()) for k,v in old.items())"])
                .arg(config)
                .arg(cargo_home.join(name))
                .status().unwrap();
        assert!(preserved.success(), "{case}: caller settings changed");
    }
}

/// A Cargo home configuration that really replaces crates-io is the caller's
/// own source and wins: the prelude must not write — or leave — a workspace
/// replacement that shadows it, whichever file Cargo reads, however the
/// crates-io key is spelled.
#[test]
fn a_caller_source_replacement_in_the_cargo_home_is_kept() {
    for (label, name, replacement) in [
        (
            "modern-file",
            "config.toml",
            "[source.crates-io]\nreplace-with = \"customer\"\n\n[source.customer]\nregistry = \"sparse+{base}/customer/\"\n",
        ),
        (
            "legacy-file",
            "config",
            "[source.crates-io]\nreplace-with = \"customer\"\n\n[source.customer]\nregistry = \"sparse+{base}/customer/\"\n",
        ),
        (
            "quoted-key",
            "config.toml",
            "[source.\"crates-io\"]\nreplace-with = \"customer\"\n\n[source.customer]\nregistry = \"sparse+{base}/customer/\"\n",
        ),
        (
            "registry-definition",
            "config.toml",
            "[source.crates-io]\nregistry = \"sparse+{base}/customer/\"\n",
        ),
    ] {
        let root = Scratch::new("override");
        let w = root.0.join("ws");
        let (checksum, archive) = fixture(&root.0);
        consumer(&w, &checksum);
        let cargo_home = root.0.join("custom-cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        let mirror = Mirror::start(&checksum, archive);
        std::fs::write(
            cargo_home.join(name),
            replacement.replace("{base}", &mirror.base),
        )
        .unwrap();
        // The replacement an earlier run left on a warm workspace must not
        // keep shadowing the caller's source either.
        std::fs::create_dir_all(w.join(".cargo")).unwrap();
        std::fs::write(
                w.join(".cargo/config.toml"),
                format!(
                    "[source.crates-io]\nreplace-with = \"sylphx-mirror\"\n\n[source.sylphx-mirror]\nregistry = \"sparse+{}/stale/\"\n",
                    mirror.base
                ),
            )
            .unwrap();
        let out = run(
            &w,
            &root.0,
            &cargo_home,
            &format!("sparse+{}/managed/", mirror.base),
            None,
        );
        assert_eq!(
            std::fs::read_to_string(cargo_home.join(name)).unwrap(),
            replacement.replace("{base}", &mirror.base),
            "{label}: caller source config was overwritten"
        );
        let requests = mirror.requests();
        let case = label;
        assert!(
            out.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            requests
                .iter()
                .any(|p| p == "/customer/ca/ch/cached-fixture"),
            "{case}: {requests:?}"
        );
        assert!(
            !requests.iter().any(|p| p.starts_with("/managed/")),
            "{case}: {requests:?}"
        );
        assert!(
            !requests.iter().any(|p| p.starts_with("/stale/")),
            "{case}: {requests:?}"
        );
    }
}

#[test]
fn native_registry_env_keeps_native_locked_fetch_behavior() {
    for index in ["", "sparse+http://127.0.0.1:9/caller/"] {
        let root = Scratch::new("native-index");
        let w = root.0.join("ws");
        let (checksum, archive) = fixture(&root.0);
        let lock = consumer(&w, &checksum);
        let cargo_home = root.0.join("custom-cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        let config = "[build]\njobs=2\n";
        std::fs::write(cargo_home.join("config.toml"), config).unwrap();
        let mirror = Mirror::start(&checksum, archive);
        let baseline = run(&w, &root.0, &cargo_home, "", Some(index));
        let registry = cargo_home.join("registry");
        if registry.exists() {
            std::fs::remove_dir_all(registry).unwrap();
        }
        let managed = run(
            &w,
            &root.0,
            &cargo_home,
            &format!("sparse+{}/managed/", mirror.base),
            Some(index),
        );
        assert_eq!(
            managed.status.code(),
            baseline.status.code(),
            "native index={index:?}: {}",
            String::from_utf8_lossy(&managed.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(cargo_home.join("config.toml")).unwrap(),
            config
        );
        assert_eq!(
            std::fs::read_to_string(w.join("tree/Cargo.lock")).unwrap(),
            lock
        );
        assert!(!w.join(".cargo/config.toml").exists());
        assert!(mirror.requests().is_empty());
    }
}

#[test]
fn cargo_outside_the_workspace_uses_the_mirror_cache_offline() {
    let root = Scratch::new("outside");
    let w = root.0.join("ws");
    let (checksum, archive) = fixture(&root.0);
    consumer(&w, &checksum);
    let outside = root.0.join("outside");
    consumer(&outside, &checksum);
    std::fs::write(
        outside.join("tree/src/main.rs"),
        "fn main() { assert_eq!(cached_fixture::answer(), 42); }\n",
    )
    .unwrap();
    let cargo_home = root.0.join("custom-cargo");
    let mirror = Mirror::start(&checksum, archive);
    let script = format!(
        "{FETCH} && cd \"$EXTERNAL_PACKAGE\" && RUSTUP_HOME=\"$REAL_RUSTUP_HOME\" \"$REAL_CARGO\" run --offline --locked --quiet"
    );
    let out = run_command(
        &w,
        &root.0,
        &cargo_home,
        &format!("sparse+{}/managed/", mirror.base),
        None,
        &script,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(mirror
        .requests()
        .iter()
        .any(|p| p == "/managed/ca/ch/cached-fixture"));
    assert!(!w.join(".cargo/config.toml").exists());
}

#[test]
fn a_warm_cargo_home_updates_and_removes_only_the_managed_source() {
    for name in ["config.toml", "config"] {
        let root = Scratch::new("warm");
        let w = root.0.join("ws");
        let (checksum, archive) = fixture(&root.0);
        consumer(&w, &checksum);
        let cargo_home = root.0.join("custom-cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        let caller = "[build]\njobs=2\n";
        std::fs::write(cargo_home.join(name), caller).unwrap();
        let mirror = Mirror::start(&checksum, archive);
        for prefix in ["first", "changed"] {
            let out = run(
                &w,
                &root.0,
                &cargo_home,
                &format!("sparse+{}/{prefix}/", mirror.base),
                None,
            );
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // Both explicit native-index opt-out (including empty) and removing
        // the managed mirror must restore the original caller configuration.
        for index in [Some(""), Some("sparse+http://127.0.0.1:9/native/"), None] {
            let active = format!("sparse+{}/changed/", mirror.base);
            let configured = if index.is_some() { active.as_str() } else { "" };
            let out = run_command(&w, &root.0, &cargo_home, configured, index, "true");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(cargo_home.join(name)).unwrap(),
                caller
            );
            assert!(!cargo_home.join(".sylphx-mirror.json").exists());
            let out = run_command(
                &w,
                &root.0,
                &cargo_home,
                &format!("sparse+{}/changed/", mirror.base),
                None,
                "true",
            );
            assert!(out.status.success());
        }
        // Once a caller replaces the generated source, later managed runs
        // preserve that configuration byte-for-byte, including on opt-out.
        let replacement = format!(
            "[source.crates-io]\nreplace-with='customer'\n[source.customer]\nregistry='sparse+{}/customer/'\n",
            mirror.base
        );
        std::fs::write(cargo_home.join(name), &replacement).unwrap();
        let out = run(
            &w,
            &root.0,
            &cargo_home,
            &format!("sparse+{}/managed/", mirror.base),
            None,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = run_command(&w, &root.0, &cargo_home, "", Some(""), "true");
        assert!(out.status.success());
        assert_eq!(
            std::fs::read_to_string(cargo_home.join(name)).unwrap(),
            replacement
        );
        let requests = mirror.requests();
        for prefix in ["first", "changed", "customer"] {
            assert!(
                requests
                    .iter()
                    .any(|p| p == &format!("/{prefix}/ca/ch/cached-fixture")),
                "{requests:?}"
            );
        }
        assert!(!requests.iter().any(|p| p.starts_with("/managed/")));
    }
}

#[test]
fn a_registry_adopted_by_a_caller_is_reused_and_preserved() {
    for name in ["config.toml", "config"] {
        let root = Scratch::new("adopted");
        let w = root.0.join("ws");
        let (checksum, archive) = fixture(&root.0);
        consumer(&w, &checksum);
        let cargo_home = root.0.join("custom-cargo");
        std::fs::create_dir_all(&cargo_home).unwrap();
        let path = cargo_home.join(name);
        std::fs::write(&path, "[build]\njobs=2\n").unwrap();
        let mirror = Mirror::start(&checksum, archive);
        let first = format!("sparse+{}/first/", mirror.base);
        let out = run(&w, &root.0, &cargo_home, &first, None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // This caller source now depends on the previously managed registry.
        let caller_url = format!("sparse+{}/caller/", mirror.base);
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "\n[source.caller]\nregistry={caller_url:?}\nreplace-with='sylphx-mirror'"
        )
        .unwrap();
        for prefix in ["first", "first", "", "first", "changed", ""] {
            let active = if prefix.is_empty() {
                String::new()
            } else {
                format!("sparse+{}/{prefix}/", mirror.base)
            };
            let out = run_command(
                &w,
                &root.0,
                &cargo_home,
                &active,
                None,
                if prefix.is_empty() { "true" } else { FETCH },
            );
            assert!(
                out.status.success(),
                "{name} {prefix:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let parsed = Command::new("python3")
                .args(["-c", "import json,sys,tomllib; print(json.dumps(tomllib.load(open(sys.argv[1],'rb'))))"])
                .arg(&path)
                .output().unwrap();
            assert!(parsed.status.success());
            let config: serde_json::Value = serde_json::from_slice(&parsed.stdout).unwrap();
            let sources = config["source"].as_object().unwrap();
            assert_eq!(config["build"]["jobs"], 2);
            assert_eq!(
                sources["caller"],
                serde_json::json!({
                    "registry": caller_url, "replace-with": "sylphx-mirror"
                })
            );
            assert_eq!(
                sources["sylphx-mirror"],
                serde_json::json!({"registry": first})
            );
            assert_eq!(
                sources.values().filter(|s| s["registry"] == first).count(),
                1
            );
            if prefix.is_empty() {
                assert!(!sources.contains_key("crates-io"));
                assert_eq!(sources.len(), 2);
            } else {
                let target = sources["crates-io"]["replace-with"].as_str().unwrap();
                assert_eq!(sources[target]["registry"], active);
                if prefix == "first" {
                    assert_eq!(target, "sylphx-mirror");
                }
            }
        }
        let requests = mirror.requests();
        for prefix in ["first", "changed"] {
            assert!(
                requests
                    .iter()
                    .any(|p| p == &format!("/{prefix}/ca/ch/cached-fixture")),
                "{requests:?}"
            );
        }
    }
}

#[test]
fn a_relative_cargo_home_is_shared_with_temporary_packages() {
    let root = Scratch::new("relative-home");
    let w = root.0.join("ws");
    let (checksum, archive) = fixture(&root.0);
    consumer(&w, &checksum);
    consumer(&root.0.join("outside"), &checksum);
    let mirror = Mirror::start(&checksum, archive);
    let script = format!(
        "{FETCH} && cd \"$EXTERNAL_PACKAGE\" && RUSTUP_HOME=\"$REAL_RUSTUP_HOME\" \"$REAL_CARGO\" fetch --offline --locked"
    );
    let out = run_command(
        &w,
        &root.0,
        Path::new("cargo cache"),
        &format!("sparse+{}/managed/", mirror.base),
        None,
        &script,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(w.join("tree/cargo cache/config.toml").exists());
    assert!(w.join("tree/cargo cache/registry/cache").exists());
    assert!(!root.0.join("cargo cache").exists());
    assert!(mirror
        .requests()
        .iter()
        .any(|p| p == "/managed/ca/ch/cached-fixture"));
}

#[test]
fn unicode_caller_keys_survive_source_merge_and_cleanup() {
    for reuse in [false, true] {
        let root = Scratch::new("unicode-keys");
        let w = root.0.join("ws");
        let (checksum, archive) = fixture(&root.0);
        consumer(&w, &checksum);
        let home = root.0.join("cargo");
        std::fs::create_dir_all(&home).unwrap();
        let mirror = Mirror::start(&checksum, archive);
        let url = format!("sparse+{}/managed/", mirror.base);
        let source = if reuse {
            format!("{{ \"🧪\" = {{ registry = {url:?} }} }}")
        } else {
            "{ other = { registry = \"sparse+http://127.0.0.1:9/other/\" } }".into()
        };
        let original = format!(
            "\"\\u007f\" = \"control\"\n\"🧪\" = {{ \"🧪\" = \"preserved\" }}\nsource = {source}\n[alias]\n\"🧪\" = \"check\"\n"
        );
        let config = home.join("config.toml");
        std::fs::write(&config, &original).unwrap();
        for active in [&url[..], "", &url[..], ""] {
            let out = run_command(
                &w,
                &root.0,
                &home,
                active,
                None,
                if active.is_empty() { "true" } else { FETCH },
            );
            assert!(
                out.status.success(),
                "reuse={reuse} mirror={active:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let preserved = Command::new("python3")
                .args(["-c", "import sys,tomllib; old=tomllib.loads(sys.argv[1]); new=tomllib.load(open(sys.argv[2],'rb')); assert all(new[k] == v if k != 'source' else all(new[k][s] == x for s,x in v.items()) for k,v in old.items()); assert sys.argv[3] or new == old"])
                .arg(&original).arg(&config).arg(active).status().unwrap();
            assert!(preserved.success());
        }
        assert!(mirror
            .requests()
            .iter()
            .any(|p| p == "/managed/ca/ch/cached-fixture"));
    }
}
