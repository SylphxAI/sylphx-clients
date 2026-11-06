//! `sylphx build image [PATH]`: builds an OCI image from this git work tree
//! on a build lease, with the template's rootless BuildKit
//! (`sylphx-image-build`). The lease, workspace, sync, output stream and exit
//! codes are `build run`'s own ([`super`]); this file holds what is specific
//! to an image: the flags, the guest command line and environment, the
//! egress list, the registry credentials' handling, and the metadata copied
//! back.
//!
//! With neither `--push` nor `--registry-auth` the build is a managed one:
//! the platform mints a unique upload reference on the project's registry and
//! a short-lived credential just before the build runs ([`Publish`]), the
//! image is registered in Kernel Artifacts when it succeeds, and the
//! deployable `repo@digest` reference is what the caller deploys with. An
//! explicit `--push REF --registry-auth FILE` pair publishes to another
//! registry and registers nothing.
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

mod registry;
pub use registry::registry_descriptor;

/// The guest command.
pub const GUEST_BIN: &str = "/usr/local/bin/sylphx-image-build";
/// The registry the managed publication pushes to: the deployed data-plane
/// host, not customer configuration. The lease's egress allow-list is fixed
/// before the machine is acquired, while the upload reference is only minted
/// after it is ready, so the host is named here and the minted reference is
/// held to it: a managed build must be able to reach its registry from its
/// first byte.
pub const MANAGED_REGISTRY_HOST: &str = "registry.sylphx.com";
/// BuildKit's per-lease state on the cold owned workspace.
pub fn state_dir() -> String {
    format!("{WS}/buildkit")
}
/// Removes the tree's `target` link to the workspace's `target/` before the
/// image build, then runs it. The sync links `tree/target` to `../target`
/// for `build run`; in an image's context that link dangles (`COPY . /app`
/// gives `/app/target` pointing nowhere), and tools that walk the source fail
/// on it (`next build`: `ENOENT, stat '/app/target'`). Only that exact link
/// goes; a `target` of the source's own stays.
pub const UNLINK_TARGET: &str = r#"t=$1/target
shift
if [ -L "$t" ] && [ "$(readlink "$t")" = ../target ]; then rm -f "$t"; fi
exec "$@"
"#;
/// The script's metadata file, copied back after the build.
pub fn metadata_path() -> String {
    format!("{WS}/.sylphx/image.json")
}
/// A private directory (0700) for credentials on the machine's root disk,
/// outside the synced workspace. The machine ends with the lease, so an
/// interrupted build leaves no credential behind.
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
            | "SYLPHX_UPSTREAM_MIRROR"
            | "SYLPHX_NPM_MIRROR"
            | "SYLPHX_PYPI_MIRROR"
            | "SYLPHX_GOPROXY"
            | "SYLPHX_PIP_TRUSTED_HOST"
            | "CARGO_REGISTRIES_CRATES_IO_INDEX"
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
    /// The reference an explicit `--push` names; `None` on the managed path.
    pub push: Option<String>,
    /// The local credentials file; its content goes to the machine alone.
    pub registry_auth: Option<PathBuf>,
    /// Where the image archive lands on this machine.
    pub oci_out: Option<PathBuf>,
    /// The local HEAD's commit time and id, when the tree is clean at HEAD.
    pub source: Option<Source>,
    /// No `--push` and no `--registry-auth`: the image is published through
    /// the platform's managed path. The reference and the credentials are
    /// minted just before the build runs ([`Publish`]).
    pub managed: bool,
}

/// Where the guest pushes the image and how it authenticates, resolved just
/// before the build starts: an explicit `--push`/`--registry-auth` pair, or
/// the reference and credentials the platform minted for this build.
///
/// It holds no credential: the registry credentials live only on the machine
/// (a private file) and among the run's scrubbed secrets. The reference is a
/// non-secret upload name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Publish {
    /// The reference the guest pushes to (`--push`).
    pub reference: Option<String>,
    /// The guest has a private credentials file at [`auth_path`].
    pub auth: bool,
    /// Minted by the platform: the run registers the pushed image as an
    /// Artifact when the build succeeds.
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub epoch: u64,
    pub commit: String,
}

/// The published manifest descriptor confirmed by the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageDescriptor {
    pub media_type: String,
    pub digest: String,
    pub size_bytes: i64,
}

pub fn command() -> Command {
    Command::new("image")
        .about("Build an OCI image from this work tree on a remote build machine; the digest comes back")
        .long_about("Builds an OCI image from the enclosing git work tree (PATH inside it is the build context) with rootless BuildKit on a build machine: the Dockerfile, or with none the zero-config frontend (Railpack). Each build gets a cold lease-owned workspace after admission, reclaimed at lease end; --region pins placement without creating or attaching a persistent workspace Volume. BuildKit's local layer state ends with the lease. Output streams back unchanged apart from at most three `sylphx:` lines on stderr; the digest, frontend and process sandbox are printed after it.\n\nThe image is reproducible when the tree is clean at HEAD: SOURCE_DATE_EPOCH is HEAD's commit time and the commit is recorded. A tree with local changes records no commit and builds with SOURCE_DATE_EPOCH 0 (still reproducible for the same tree), and says so on one `sylphx:` line.\n\nBy default the image is published to the project's managed registry (registry.sylphx.com/<org>/<project>-image) under a unique upload reference, with credentials the platform mints for this build alone: nothing to pass, and no credential on this machine. When the build succeeds its digest is registered in Sylphx Artifacts and the deployable reference (repo@digest) is printed. --push REF --registry-auth FILE publishes to another registry instead: the file (a Docker config.json) is sent to the machine as a private file, removed after the build, and never logged.\n\nThe machine reaches no internet package host: only the project's scoped build cache, the managed registry (or the registry of --push), and each --allow-host. Package installs resolve through the build cache's mirrors; add --allow-host for any other source. An explicit REF with no registry host in it (Docker Hub) needs --allow-host for that registry.\n\nExit codes: sylphx-image-build's (2 usage; 125 before BuildKit ran; otherwise BuildKit's); 124 --timeout reached; 125 platform failure (not signed in, no capacity, no machine within --queue-timeout, sync failed, machine lost); 130 interrupted. -o json adds the whole metadata object to the `result` event as `image`, plus `artifact` and `source_ref` for a managed publication.\n\nExample: sylphx build image")
        .arg(Arg::new("path").value_name("PATH").index(1)
            .help("Directory to build (default \".\"); the synced root is its git work tree"))
        .arg(Arg::new("dockerfile").long("dockerfile").value_name("PATH")
            .help("Dockerfile, relative to PATH (default: PATH/Dockerfile, else the zero-config frontend)"))
        .arg(Arg::new("push").long("push").value_name("REF")
            .help("Publish to this reference instead of the managed registry; needs --registry-auth"))
        .arg(Arg::new("registry-auth").long("registry-auth").value_name("FILE")
            .help("Registry credentials for --push (a Docker config.json), sent to the machine privately and removed after the build"))
        .arg(Arg::new("oci-out").long("oci-out").value_name("FILE")
            .help("Copy the image back as an OCI archive to FILE"))
        .arg(Arg::new("size").long("size").value_name("SIZE").default_value("large")
            .value_parser(SIZES)
            .help("Machine size: standard (8 vCPU), large (16), xlarge (32)"))
        .arg(Arg::new("timeout").long("timeout").value_name("DURATION")
            .help("Wall-clock limit for the build (default 60m, at most 6h)"))
        .arg(Arg::new("region").long("region").value_name("REGION")
            .help("Region to pin the build to (default: automatic Build placement); workspace lifetime always follows the lease"))
        .arg(Arg::new("queue-timeout").long("queue-timeout").value_name("DURATION")
            .help("Longest wait for a build machine before exiting 125, retryable (default 30m, at most 6h)"))
        .arg(Arg::new("allow-host").long("allow-host").value_name("HOST").action(ArgAction::Append)
            .help("Extra egress host beyond the build cache and the publication registry (repeatable)"))
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
        // Neither flag names an external registry: the platform mints the
        // publication (and its credentials) just before the build runs.
        let managed = push.is_none() && registry_auth.is_none();
        Ok(Self {
            dockerfile,
            push,
            registry_auth,
            oci_out: nonempty("oci-out")?.map(PathBuf::from),
            source: None,
            managed,
        })
    }

    /// The publication the caller named on the command line: an explicit
    /// `--push`/`--registry-auth` pair, or none — a managed build resolves it
    /// at [`Publish`] time.
    pub fn explicit_publish(&self) -> Publish {
        Publish {
            reference: self.push.clone(),
            auth: self.registry_auth.is_some(),
            managed: self.managed,
        }
    }

    /// The guest command line: [`UNLINK_TARGET`], then the image build of
    /// [`Self::guest_argv`].
    pub fn argv(&self, rel: &str, publish: &Publish) -> Vec<String> {
        let mut a = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            UNLINK_TARGET.into(),
            "sylphx-image".into(),
            format!("{WS}/tree"),
        ];
        a.extend(self.guest_argv(rel, publish));
        a
    }

    /// `sylphx-image-build`'s command line; the build context is the tree
    /// directory. `publish` carries only what the guest is told — the upload
    /// reference, and whether a private credentials file is at [`auth_path`]
    /// — never a credential.
    pub fn guest_argv(&self, rel: &str, publish: &Publish) -> Vec<String> {
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
        if let Some(r) = &publish.reference {
            a.extend(["--push".into(), r.clone()]);
        }
        if publish.auth {
            a.extend(["--registry-auth".into(), auth_path()]);
        }
        // Retain an archive only when the caller asks to download it.
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

/// The egress allow-list of an image build: the build cache and the registry
/// the image is published to — the managed one by default, `--push`'s host
/// otherwise — then each `--allow-host`. No internet package host: the
/// build's installs resolve through the cache's mirrors.
pub fn allowed_domains(image: &Image, extra: &[String]) -> Vec<String> {
    let mut v = vec![BUILD_CACHE_HOST.to_string()];
    if image.managed {
        v.push(MANAGED_REGISTRY_HOST.to_string());
    } else if let Some(h) = image.push.as_deref().and_then(registry_host) {
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

/// The project a resource name below `orgs/{org}/projects/{project}/…` hangs
/// from (`orgs/{org}/projects/{project}`): the parent an Artifact and a
/// managed upload are scoped to. The environment of the run's own parent
/// never names the repository a caller chooses.
pub fn project_parent(parent: &str) -> Option<String> {
    let mut segs = parent.split('/');
    let (mut org, mut project) = (None, None);
    while let Some(seg) = segs.next() {
        match seg {
            "orgs" => org = segs.next(),
            "projects" => project = segs.next(),
            _ => {}
        }
    }
    match (
        org.filter(|s| !s.is_empty()),
        project.filter(|s| !s.is_empty()),
    ) {
        (Some(org), Some(project)) => Some(format!("orgs/{org}/projects/{project}")),
        _ => None,
    }
}

/// The canonical digest accepted by Kernel Artifacts and Hosting.
pub fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The Artifact's name: `{parent}/artifacts/{digest}` with `-` for `:`, as
/// the server derives it from the digest.
pub fn artifact_name(parent: &str, digest: &str) -> Option<String> {
    is_digest(digest).then(|| format!("{parent}/artifacts/sha256-{}", &digest[7..]))
}

/// The deployable reference of a pushed image: the registry repository of
/// `reference` with its tag replaced by the digest the registry stored.
pub fn source_ref(reference: &str, digest: &str) -> Option<String> {
    if !is_digest(digest) {
        return None;
    }
    let repo = match reference.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => repo,
        _ => reference,
    };
    (!repo.is_empty()).then(|| format!("{repo}@{digest}"))
}

/// The image digest the build's metadata records, when it is one.
pub fn image_digest(meta: &Value) -> Option<&str> {
    let digest = meta.get("digest")?.as_str()?;
    is_digest(digest).then_some(digest)
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

    /// No `--push` and no `--registry-auth`: the build is a managed one. Its
    /// upload reference and credentials do not exist yet, so the command as
    /// parsed carries neither. No archive is retained without --oci-out.
    #[test]
    fn defaults_render_the_managed_command() {
        let i = parse(&["sylphx", "image"]).unwrap();
        assert!(i.managed && i.push.is_none() && i.registry_auth.is_none());
        assert_eq!(
            i.guest_argv("", &i.explicit_publish()),
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
        let sub = i.guest_argv("services/api", &i.explicit_publish());
        assert_eq!(sub[2], "/workspace/tree/services/api");
        // The guest runs the build behind the unlink of the tree's target link.
        let a = i.argv("services/api", &i.explicit_publish());
        assert_eq!(
            a[..5],
            [
                "/bin/sh",
                "-c",
                UNLINK_TARGET,
                "sylphx-image",
                "/workspace/tree"
            ]
        );
        assert_eq!(a[5..], sub[..]);
    }

    /// The minted publication reaches the guest as a reference and a path —
    /// never as a credential: the Docker config stays a private file on the
    /// machine, and no argument or environment value carries it.
    #[test]
    fn the_minted_publication_never_puts_a_credential_in_the_command() {
        let i = parse(&["sylphx", "image"]).unwrap();
        let auth = r#"{"auths":{"registry.sylphx.com":{"identitytoken":"mint-token-value"}}}"#;
        let publish = Publish {
            reference: Some(format!("{MANAGED_REGISTRY_HOST}/org_1/prj_1-image:up1")),
            auth: true,
            managed: true,
        };
        let a = i.argv(".", &publish);
        let at = |flag: &str| a[a.iter().position(|x| x == flag).unwrap() + 1].clone();
        assert_eq!(
            at("--push"),
            format!("{MANAGED_REGISTRY_HOST}/org_1/prj_1-image:up1")
        );
        assert_eq!(at("--registry-auth"), auth_path());
        assert!(!a.iter().any(|x| x.contains("mint-token-value")), "{a:?}");
        assert!(!i.env().values().any(|v| v.contains("mint-token-value")));
        // The credential's values are scrubbed from anything that leaves.
        let secrets = auth_secrets(auth);
        assert!(crate::build_cache::scrub(auth, &secrets).contains("[redacted]"));
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
        // An explicit publication is not a managed one.
        assert!(!i.managed);
        let a = i.argv(".", &i.explicit_publish());
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

    /// Only the sync's own `target -> ../target` link leaves the tree; a
    /// source's own `target` (a directory or another link) stays, and the
    /// build runs either way.
    #[test]
    fn the_image_context_drops_only_the_workspace_target_link() {
        let dir = std::env::temp_dir().join(format!("sylphx-image-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let run = |tree: &Path| {
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", UNLINK_TARGET, "sylphx-image"])
                .arg(tree)
                .args(["echo", "built"])
                .output()
                .unwrap();
            assert!(out.status.success());
            assert_eq!(String::from_utf8_lossy(&out.stdout), "built\n");
        };
        let ours = dir.join("ours");
        std::fs::create_dir_all(&ours).unwrap();
        std::os::unix::fs::symlink("../target", ours.join("target")).unwrap();
        run(&ours);
        assert!(std::fs::symlink_metadata(ours.join("target")).is_err());
        let theirs = dir.join("theirs");
        std::fs::create_dir_all(theirs.join("target")).unwrap();
        run(&theirs);
        assert!(theirs.join("target").is_dir());
        let link = dir.join("link");
        std::fs::create_dir_all(&link).unwrap();
        std::os::unix::fs::symlink("build/out", link.join("target")).unwrap();
        run(&link);
        assert!(std::fs::symlink_metadata(link.join("target")).is_ok());
        run(&dir.join("none"));
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A managed build is allowed the project's registry from the lease's
    /// first byte: the upload reference is minted only after the machine is
    /// ready, so nothing else can put the host on the list.
    #[test]
    fn a_managed_build_leases_with_the_managed_registry() {
        let managed = Image {
            managed: true,
            ..Image::default()
        };
        assert_eq!(
            allowed_domains(&managed, &[]),
            [BUILD_CACHE_HOST, MANAGED_REGISTRY_HOST]
        );
        assert!(
            parse(&["sylphx", "image"]).unwrap().managed,
            "no flags name the managed publication"
        );
        for h in sylphx_build_lease::BUILD_PACKAGES {
            if h != BUILD_CACHE_HOST {
                assert!(
                    !allowed_domains(&managed, &[]).iter().any(|x| x == h),
                    "{h}"
                );
            }
        }
    }

    /// The parent an Artifact and a managed upload are scoped to: the run's
    /// canonical parent, never a repository a caller names.
    #[test]
    fn the_project_parent_drops_the_environment() {
        assert_eq!(
            project_parent("orgs/org_1/projects/prj_1/envs/env_1").as_deref(),
            Some("orgs/org_1/projects/prj_1")
        );
        assert_eq!(
            project_parent("orgs/org_1/projects/prj_1").as_deref(),
            Some("orgs/org_1/projects/prj_1")
        );
        assert_eq!(project_parent("orgs/org_1"), None);
        assert_eq!(project_parent(""), None);
    }

    /// The deployable reference the build prints: the pushed repository, the
    /// digest the registry stored — not the upload tag.
    #[test]
    fn the_source_ref_replaces_the_upload_tag_with_the_digest() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            source_ref(
                &format!("{MANAGED_REGISTRY_HOST}/org_1/prj_1-image:up1"),
                &digest
            )
            .as_deref(),
            Some(format!("{MANAGED_REGISTRY_HOST}/org_1/prj_1-image@{digest}").as_str())
        );
        assert_eq!(
            source_ref("registry.sylphx.com:443/acme/app:1", &digest).as_deref(),
            Some(format!("registry.sylphx.com:443/acme/app@{digest}").as_str())
        );
        assert_eq!(
            source_ref("localhost:5000/acme/app", &digest).as_deref(),
            Some(format!("localhost:5000/acme/app@{digest}").as_str())
        );
        assert_eq!(source_ref("acme/app:1", "not-a-digest"), None);
    }

    #[test]
    fn artifact_names_and_digests_follow_the_resource() {
        let digest = format!("sha256:{}", "b".repeat(64));
        assert_eq!(
            artifact_name("orgs/o/projects/p", &digest).as_deref(),
            Some(format!("orgs/o/projects/p/artifacts/sha256-{}", "b".repeat(64)).as_str())
        );
        assert_eq!(artifact_name("orgs/o/projects/p", "sha256"), None);
        assert_eq!(
            image_digest(&serde_json::json!({"digest": digest.clone()})),
            Some(digest.as_str())
        );
        assert_eq!(
            image_digest(&serde_json::json!({"digest": "sha256:"})),
            None
        );
        assert_eq!(image_digest(&serde_json::json!({})), None);
        assert!(is_digest(&digest));
        for invalid in [
            "sha256:ab",
            "sha256:",
            "ab",
            &format!("sha256:{}", "A".repeat(64)),
            &format!("sha512:{}", "a".repeat(64)),
        ] {
            assert!(!is_digest(invalid), "{invalid}");
        }
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
            "SYLPHX_UPSTREAM_MIRROR",
            "PIP_INDEX_URL",
            "PIP_TRUSTED_HOST",
            "GOPROXY",
            "MISE_URL_REPLACEMENTS",
            "CARGO_REGISTRIES_CRATES_IO_INDEX",
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
            "credentials stay outside the synced workspace"
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
