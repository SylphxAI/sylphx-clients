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
    let lock = format!("version = 3\n\n[[package]]\nname = \"cached-fixture\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n\n[[package]]\nname = \"consumer\"\nversion = \"0.1.0\"\ndependencies = [\"cached-fixture\"]\n");
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
        .args([".", "1", "sh", "-c", FETCH])
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
/// mirror from routing the fetch, and must stay exactly as the caller left it:
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
                requests.iter().any(|p| p == "/managed/ca/ch/cached-fixture"),
                "{case}: {requests:?}"
            );
            assert!(
                requests
                    .iter()
                    .any(|p| p == "/files/cached-fixture/0.1.0/download"),
                "{case}: {requests:?}"
            );
            assert_eq!(
                std::fs::read_to_string(cargo_home.join(name)).unwrap(),
                config,
                "{case}"
            );
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
