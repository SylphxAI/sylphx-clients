//! `sylphx build image [PATH]`: builds an OCI image from this git work tree
//! on a build lease, with the template's rootless BuildKit
//! (`sylphx-image-build`). The lease, workspace, sync, output stream and exit
//! codes are `build run`'s own ([`super`]); this file holds what is specific
//! to an image: the flags, the guest command line and environment, the
//! egress list, the registry credentials' handling, and the metadata copied
//! back.
//!
//! Exit codes are `build run`'s: the script's (2 usage, 125 before BuildKit
//! ran, else BuildKit's) once it has started; 124, 125 and 130 as there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as Git;

use base64::Engine;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::Value;

use super::{BUILD_CACHE_HOST, SIZES, WS};

/// The guest command.
pub const GUEST_BIN: &str = "/usr/local/bin/sylphx-image-build";
/// BuildKit's state: on the warm workspace, so layers stay warm.
pub fn state_dir() -> String {
    format!("{WS}/buildkit")
}
/// The script's metadata file, copied back after the build.
pub fn metadata_path() -> String {
    format!("{WS}/.sylphx/image.json")
}
/// A private directory (0700) for what must not be readable by others: on
/// the machine's own disk, which ends with the lease, never on the warm
/// workspace Volume, so an interrupted build leaves no credential behind.
pub fn private_dir() -> String {
    "/tmp/sylphx-private".to_string()
}

/// The cache variables an image build receives: mirror addresses only.
pub fn forwarded(key: &str) -> bool {
    matches!(
        key,
        "SYLPHX_CRATES_MIRROR"
            | "SYLPHX_OCI_MIRROR"
            | "SYLPHX_APT_MIRROR"
            | "PIP_INDEX_URL"
            | "PIP_TRUSTED_HOST"
            | "UV_INDEX_URL"
            | "GOPROXY"
            | "GOSUMDB"
            | "NPM_CONFIG_REGISTRY"
            | "BUN_CONFIG_REGISTRY"
            | "RUSTUP_DIST_SERVER"
            | "RUSTUP_UPDATE_ROOT"
    ) || key.starts_with("MISE_")
}

/// The build cache's OCI read-through (`/v2/`, no token), for registries a
/// build pulls from, when the cache's env names none.
pub fn default_oci_mirror() -> String {
    format!("https://{BUILD_CACHE_HOST}")
}
pub fn auth_path() -> String {
    format!("{}/registry-auth.json", private_dir())
}
/// Where `--oci-out` is written on the machine before it is copied back.
pub fn oci_path() -> String {
    format!("{WS}/.sylphx/image.oci.tar")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub dockerfile: Option<String>,
    pub push: Option<String>,
    /// The local credentials file; its content goes to the machine alone.
    pub registry_auth: Option<PathBuf>,
    /// Where the image archive lands on this machine.
    pub oci_out: Option<PathBuf>,
    /// The local HEAD's commit time and id, when the tree is clean at HEAD.
    pub source: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub epoch: u64,
    pub commit: String,
}

pub fn command() -> Command {
    Command::new("image")
        .about("Build an OCI image from this work tree on a remote build machine; the digest comes back")
        .long_about("Builds an OCI image from the enclosing git work tree (PATH inside it is the build context) with rootless BuildKit on a build machine: the Dockerfile, or with none the zero-config frontend (Railpack). Only files that differ from the warm workspace are sent; BuildKit's layer cache stays warm on it. Output streams back unchanged apart from at most three `sylphx:` lines on stderr; the digest, frontend and process sandbox are printed after it.\n\nThe image is reproducible when the tree is clean at HEAD: SOURCE_DATE_EPOCH is HEAD's commit time and the commit is recorded. A tree with local changes records no commit and builds with SOURCE_DATE_EPOCH 0 (still reproducible for the same tree), and says so on one `sylphx:` line.\n\nThe machine reaches no internet package host: only the project's build cache, the registry of --push, and each --allow-host. Package installs resolve through the build cache's mirrors; add --allow-host for any other source. --push REF needs --registry-auth FILE (a Docker config.json); the file is sent to the machine as a private file and removed after the build, and is never logged. A REF with no registry host in it (Docker Hub) needs --allow-host for that registry.\n\nExit codes: sylphx-image-build's (2 usage; 125 before BuildKit ran; otherwise BuildKit's); 124 --timeout reached; 125 platform failure (not signed in, no capacity, no machine within --queue-timeout, sync failed, machine lost); 130 interrupted. -o json adds the whole metadata object to the `result` event as `image`.\n\nExample: sylphx build image --push registry.sylphx.net/acme/app:1 --registry-auth ~/.docker/config.json")
        .arg(Arg::new("path").value_name("PATH").index(1)
            .help("Directory to build (default \".\"); the synced root is its git work tree"))
        .arg(Arg::new("dockerfile").long("dockerfile").value_name("PATH")
            .help("Dockerfile, relative to PATH (default: PATH/Dockerfile, else the zero-config frontend)"))
        .arg(Arg::new("push").long("push").value_name("REF")
            .help("Push the image to this reference; needs --registry-auth"))
        .arg(Arg::new("registry-auth").long("registry-auth").value_name("FILE")
            .help("Registry credentials (a Docker config.json), sent to the machine privately and removed after the build"))
        .arg(Arg::new("oci-out").long("oci-out").value_name("FILE")
            .help("Copy the image back as an OCI archive to FILE"))
        .arg(Arg::new("size").long("size").value_name("SIZE").default_value("large")
            .value_parser(SIZES)
            .help("Machine size: standard (8 vCPU), large (16), xlarge (32)"))
        .arg(Arg::new("timeout").long("timeout").value_name("DURATION")
            .help("Wall-clock limit for the build (default 60m, at most 6h)"))
        .arg(Arg::new("region").long("region").value_name("REGION")
            .help("Region to build in (default: the project's home region); each region keeps its own warm workspaces"))
        .arg(Arg::new("queue-timeout").long("queue-timeout").value_name("DURATION")
            .help("Longest wait for a build machine before exiting 125, retryable (default 30m, at most 6h)"))
        .arg(Arg::new("allow-host").long("allow-host").value_name("HOST").action(ArgAction::Append)
            .help("Extra egress host beyond the build cache and the --push registry (repeatable)"))
        .arg(Arg::new("quiet").long("quiet").short('q').action(ArgAction::SetTrue)
            .help("No `sylphx:` progress lines on stderr"))
}

impl Image {
    pub fn parse(m: &ArgMatches) -> Result<Self, String> {
        let get = |k: &str| m.get_one::<String>(k).cloned();
        let nonempty = |k: &str| -> Result<Option<String>, String> {
            match get(k) {
                Some(v) if v.trim().is_empty() => Err(format!("--{k} is empty")),
                v => Ok(v),
            }
        };
        let dockerfile = nonempty("dockerfile")?;
        if let Some(d) = &dockerfile {
            let p = Path::new(d);
            if p.is_absolute() || p.components().any(|c| c.as_os_str() == "..") {
                return Err("--dockerfile is a path inside PATH".into());
            }
        }
        let push = nonempty("push")?;
        if let Some(r) = &push {
            if r.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("--push {r}: give an image reference"));
            }
        }
        let registry_auth = nonempty("registry-auth")?.map(PathBuf::from);
        match (&push, &registry_auth) {
            (Some(_), None) => return Err("--push needs --registry-auth FILE".into()),
            (None, Some(_)) => return Err("--registry-auth is for --push".into()),
            _ => {}
        }
        if let Some(f) = &registry_auth {
            if !f.is_file() {
                return Err(format!("--registry-auth {}: not a file", f.display()));
            }
        }
        Ok(Self {
            dockerfile,
            push,
            registry_auth,
            oci_out: nonempty("oci-out")?.map(PathBuf::from),
            source: None,
        })
    }

    /// The guest command line; the build context is the tree directory.
    pub fn argv(&self, rel: &str) -> Vec<String> {
        let ctx = if rel.is_empty() || rel == "." {
            format!("{WS}/tree")
        } else {
            format!("{WS}/tree/{rel}")
        };
        let mut a = vec![
            GUEST_BIN.to_string(),
            "--context".into(),
            ctx,
            "--state".into(),
            state_dir(),
            "--metadata".into(),
            metadata_path(),
        ];
        if let Some(d) = &self.dockerfile {
            a.extend(["--dockerfile".into(), d.clone()]);
        }
        if let Some(r) = &self.push {
            a.extend(["--push".into(), r.clone()]);
        }
        if self.registry_auth.is_some() {
            a.extend(["--registry-auth".into(), auth_path()]);
        }
        if self.oci_out.is_some() {
            a.extend(["--oci-out".into(), oci_path()]);
        }
        a
    }

    /// The build's own environment, on top of the cache's.
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut e = BTreeMap::new();
        if let Some(s) = &self.source {
            e.insert("SOURCE_DATE_EPOCH".into(), s.epoch.to_string());
            e.insert("SYLPHX_SOURCE_COMMIT".into(), s.commit.clone());
        }
        e.insert("SYLPHX_BUILDKIT_STATE".into(), state_dir());
        e
    }
}

/// The egress allow-list of an image build: the build cache and the push
/// registry's host, then each `--allow-host`. No internet package host: the
/// build's installs resolve through the cache's mirrors.
pub fn allowed_domains(image: &Image, extra: &[String]) -> Vec<String> {
    let mut v = vec![BUILD_CACHE_HOST.to_string()];
    if let Some(h) = image.push.as_deref().and_then(registry_host) {
        v.push(h);
    }
    v.extend(extra.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    v.retain(|h| seen.insert(h.clone()));
    v
}

/// The registry host of an image reference, when it names one (a first
/// segment with a dot or colon, or `localhost`); Docker Hub's short names
/// name none.
pub fn registry_host(reference: &str) -> Option<String> {
    let first = reference.split_once('/')?.0;
    if !(first.contains('.') || first.contains(':') || first == "localhost") {
        return None;
    }
    let host = first.rsplit_once(':').map_or(first, |(h, _)| h);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Commit time and id of the local HEAD, when the work tree is clean at HEAD
/// (nothing modified, staged or untracked). `None` otherwise.
pub fn source_of(root: &Path) -> Option<Source> {
    let git = |args: &[&str]| -> Option<String> {
        let o = Git::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if !git(&["status", "--porcelain"])?.is_empty() {
        return None;
    }
    let epoch = git(&["log", "-1", "--format=%ct", "HEAD"])?.parse().ok()?;
    let commit = git(&["rev-parse", "HEAD"])?;
    (commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(Source { epoch, commit })
}

/// Values of a credentials file that must not leave this process: the file,
/// every string in it, and the user and password inside any base64 `user:password`.
pub fn auth_secrets(content: &str) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                out.push(s.clone());
                if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(s.trim()) {
                    if let Ok(t) = String::from_utf8(b) {
                        if let Some((_, pw)) = t.split_once(':') {
                            out.push(t.clone());
                            out.push(pw.to_string());
                        }
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = vec![content.trim().to_string()];
    if let Ok(v) = serde_json::from_str::<Value>(content) {
        walk(&v, &mut out);
    }
    // Short values (`true`, a host) would blot out ordinary text.
    out.retain(|s| s.len() >= 8);
    out.sort();
    out.dedup();
    // Longest first, so a value inside another is not replaced out of it.
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out
}

/// The three lines printed after the build, from the script's metadata.
pub fn summary(meta: &Value) -> Vec<String> {
    ["digest", "frontend", "process_sandbox"]
        .iter()
        .map(|k| {
            let v = match meta.get(*k) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => "-".into(),
                Some(o) => o.to_string(),
            };
            format!("{k}: {v}")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Image, String> {
        let root = Command::new("sylphx").subcommand(command());
        let m = root.try_get_matches_from(args).map_err(|e| e.to_string())?;
        let (_, r) = m.subcommand().unwrap();
        Image::parse(r)
    }

    #[test]
    fn defaults_render_the_minimal_command() {
        let i = parse(&["sylphx", "image"]).unwrap();
        assert_eq!(
            i.argv(""),
            [
                GUEST_BIN,
                "--context",
                "/workspace/tree",
                "--state",
                "/workspace/buildkit",
                "--metadata",
                "/workspace/.sylphx/image.json"
            ]
        );
        let sub = i.argv("services/api");
        assert_eq!(sub[2], "/workspace/tree/services/api");
    }

    #[test]
    fn every_flag_reaches_the_guest_command() {
        let dir = std::env::temp_dir().join(format!("sylphx-image-auth-{}", std::process::id()));
        std::fs::write(&dir, "{}").unwrap();
        let f = dir.to_str().unwrap();
        let i = parse(&[
            "sylphx",
            "image",
            "--dockerfile",
            "docker/App.Dockerfile",
            "--push",
            "registry.sylphx.net/acme/app:1",
            "--registry-auth",
            f,
            "--oci-out",
            "out/app.tar",
        ])
        .unwrap();
        let a = i.argv(".");
        let at = |flag: &str| a[a.iter().position(|x| x == flag).unwrap() + 1].clone();
        assert_eq!(at("--dockerfile"), "docker/App.Dockerfile");
        assert_eq!(at("--push"), "registry.sylphx.net/acme/app:1");
        // The machine sees its own private copy, never the local path.
        assert_eq!(
            at("--registry-auth"),
            "/tmp/sylphx-private/registry-auth.json"
        );
        assert_eq!(at("--oci-out"), "/workspace/.sylphx/image.oci.tar");
        assert!(!a.iter().any(|x| x.contains("sylphx-image-auth")));
        assert_eq!(i.oci_out.as_deref(), Some(Path::new("out/app.tar")));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn usage_errors() {
        assert!(parse(&["sylphx", "image", "--push", "r.io/a:1"])
            .unwrap_err()
            .contains("--registry-auth"));
        assert!(
            parse(&["sylphx", "image", "--registry-auth", "/etc/hostname"])
                .unwrap_err()
                .contains("for --push")
        );
        assert!(parse(&[
            "sylphx",
            "image",
            "--registry-auth",
            "/nonexistent/x",
            "--push",
            "r.io/a"
        ])
        .unwrap_err()
        .contains("not a file"));
        assert!(parse(&["sylphx", "image", "--dockerfile", "../Dockerfile"]).is_err());
        assert!(parse(&["sylphx", "image", "--dockerfile", "/etc/passwd"]).is_err());
        assert!(parse(&["sylphx", "image", "--oci-out", " "]).is_err());
        assert!(parse(&["sylphx", "image", "--size", "huge"]).is_err());
    }

    #[test]
    fn egress_has_no_internet_package_host() {
        let none = allowed_domains(&Image::default(), &[]);
        assert_eq!(none, [BUILD_CACHE_HOST]);
        for h in sylphx_build_lease::BUILD_PACKAGES {
            if h != BUILD_CACHE_HOST {
                assert!(!none.iter().any(|x| x == h), "{h}");
            }
        }
        let img = Image {
            push: Some("registry.sylphx.net:443/acme/app:1".into()),
            ..Image::default()
        };
        assert_eq!(
            allowed_domains(&img, &["api.example.com".into(), BUILD_CACHE_HOST.into()]),
            [BUILD_CACHE_HOST, "registry.sylphx.net", "api.example.com"]
        );
        // Docker Hub short names name no host: --allow-host does.
        assert_eq!(registry_host("acme/app:1"), None);
        assert_eq!(registry_host("localhost/app"), Some("localhost".into()));
    }

    #[test]
    fn env_carries_the_source_only_when_known() {
        let mut i = Image::default();
        let e = i.env();
        assert!(!e.contains_key("SOURCE_DATE_EPOCH") && !e.contains_key("SYLPHX_SOURCE_COMMIT"));
        assert_eq!(e["SYLPHX_BUILDKIT_STATE"], "/workspace/buildkit");
        i.source = Some(Source {
            epoch: 1760000000,
            commit: "a".repeat(40),
        });
        let e = i.env();
        assert_eq!(e["SOURCE_DATE_EPOCH"], "1760000000");
        assert_eq!(e["SYLPHX_SOURCE_COMMIT"], "a".repeat(40));
    }

    #[test]
    fn an_image_build_gets_mirrors_never_cache_tokens() {
        for k in [
            "SYLPHX_CRATES_MIRROR",
            "SYLPHX_OCI_MIRROR",
            "SYLPHX_APT_MIRROR",
            "PIP_INDEX_URL",
            "PIP_TRUSTED_HOST",
            "GOPROXY",
            "MISE_URL_REPLACEMENTS",
        ] {
            assert!(forwarded(k), "{k}");
        }
        for k in [
            "SCCACHE_WEBDAV_TOKEN",
            "SCCACHE_WEBDAV_ENDPOINT",
            "TURBO_TOKEN",
            "TURBO_API",
            "PATH",
        ] {
            assert!(!forwarded(k), "{k}");
        }
        assert_eq!(default_oci_mirror(), format!("https://{BUILD_CACHE_HOST}"));
        assert!(
            auth_path().starts_with("/tmp/"),
            "credentials stay off the warm Volume"
        );
    }

    #[test]
    fn the_auth_file_content_is_scrubbed() {
        let pw = "placeholder-pass";
        let b64 = base64::engine::general_purpose::STANDARD.encode(format!("bot:{pw}"));
        let file = format!(
            r#"{{"auths":{{"registry.sylphx.net":{{"auth":"{b64}","identitytoken":"tok-placeholder-value"}}}}}}"#
        );
        let secrets = auth_secrets(&file);
        let text = format!(
            "push failed: {file} / {b64} / bot:{pw} / {pw} / tok-placeholder-value / registry.sylphx.net"
        );
        let clean = crate::build_cache::scrub(&text, &secrets);
        for leaked in [pw, &b64, "tok-placeholder-value"] {
            assert!(!clean.contains(leaked), "{leaked} in {clean}");
        }
        // Ordinary text survives.
        assert!(clean.contains("push failed"));
        assert!(clean.contains("registry.sylphx.net"));
    }

    #[test]
    fn summary_names_the_three_facts() {
        let m = serde_json::json!({"digest": "sha256:ab", "frontend": "dockerfile", "process_sandbox": true});
        assert_eq!(
            summary(&m),
            [
                "digest: sha256:ab",
                "frontend: dockerfile",
                "process_sandbox: true"
            ]
        );
        assert_eq!(summary(&serde_json::json!({}))[0], "digest: -");
    }

    #[test]
    fn a_clean_tree_gives_its_commit_time_and_a_dirty_one_none() {
        let d = std::env::temp_dir().join(format!("sylphx-image-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let g = |args: &[&str]| {
            let o = Git::new("git")
                .arg("-C")
                .arg(&d)
                .args(args)
                .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        };
        g(&["init", "-q"]);
        std::fs::write(d.join("a"), "x").unwrap();
        g(&["add", "a"]);
        g(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "m",
        ]);
        let s = source_of(&d).expect("clean");
        assert_eq!(s.epoch, 1700000000);
        assert_eq!(s.commit.len(), 40);
        std::fs::write(d.join("b"), "y").unwrap();
        assert_eq!(source_of(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
