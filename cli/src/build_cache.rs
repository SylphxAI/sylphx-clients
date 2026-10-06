//! The shared build cache (ADR remote-build-execution, section 4): a token
//! minted by the `build-cache` gateway and the environment that points
//! sccache and Turbo at it.
//!
//! * `sylphx build run` mints a read-write token for the run and merges the
//!   returned environment into the remote command ([`mint`], fail-open).
//! * `sylphx build cache env` mints a read-only token for local builds and
//!   prints it as shell `export` lines.
//!
//! The gateway's `env` is an opaque map of strings: this module neither
//! knows nor interprets the variable names. The token it carries is a
//! credential: it is never logged, never put in an event or in an error
//! text, and [`Minted`] does not print its values.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use clap::{Arg, ArgMatches, Command};
use serde_json::{json, Value};

use crate::context::{self, Resolved};
use crate::output::{self, Format};
use crate::{Defaults, Failure};

/// Where the gateway is, unless `SYLPHX_BUILD_CACHE_URL` says otherwise.
pub const DEFAULT_URL: &str = "https://build-cache.sylphx.net";
const URL_ENV: &str = "SYLPHX_BUILD_CACHE_URL";
/// A cache that does not answer this fast only slows the build: give up.
const MINT_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest token the gateway mints (a 6 h run and 15 minutes of slack).
const MAX_TTL_SECONDS: u64 = 22_500;
const RUN_SLACK_SECONDS: u64 = 900;
/// A local read-only token lives half a day.
const LOCAL_TTL_SECONDS: u64 = 43_200;
/// More than this is not a cache environment.
const MAX_ENV_ENTRIES: usize = 64;
const MAX_REPLY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// The run's machine inside the platform.
    Cluster,
    /// A developer machine.
    Public,
}

/// What a token is minted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub project: String,
    pub access: Access,
    pub network: Network,
    pub ttl_seconds: u64,
}

impl Request {
    fn body(&self) -> Value {
        json!({
            "project": self.project,
            "access": match self.access { Access::Read => "read", Access::Write => "write" },
            "ttl_seconds": self.ttl_seconds,
            "network": match self.network { Network::Cluster => "cluster", Network::Public => "public" },
        })
    }
}

/// The token's lifetime for a run of `timeout`: the run and slack, capped.
pub fn run_ttl_seconds(timeout: Duration) -> u64 {
    (timeout.as_secs() + RUN_SLACK_SECONDS).min(MAX_TTL_SECONDS)
}

/// The request for a local, read-only environment.
pub fn local_request(project: String) -> Request {
    Request {
        project,
        access: Access::Read,
        network: Network::Public,
        ttl_seconds: LOCAL_TTL_SECONDS,
    }
}

/// The request for a run of `timeout` on a build machine.
pub fn run_request(project: String, timeout: Duration) -> Request {
    Request {
        project,
        access: Access::Write,
        network: Network::Cluster,
        ttl_seconds: run_ttl_seconds(timeout),
    }
}

/// A minted environment. It holds the token, so it has no `Debug` output of
/// its values.
#[derive(Clone, PartialEq, Eq)]
pub struct Minted {
    pub env: BTreeMap<String, String>,
    /// The token itself, when the reply names it (the build store's bearer).
    pub token: Option<String>,
    /// The cache the token is for, when the reply names it.
    pub cache: Option<String>,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minted")
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .field("cache", &self.cache)
            .finish()
    }
}

impl Minted {
    /// Values worth scrubbing from any text that leaves the process.
    pub fn secrets(&self) -> Vec<String> {
        self.env
            .values()
            .chain(&self.token)
            .filter(|v| v.len() >= 8)
            .cloned()
            .collect()
    }
}

/// Why a token was not minted. [`MintError::short`] is safe for a one-line
/// warning; [`MintError::long`] adds what the gateway said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    /// No project to mint for.
    NoProject(String),
    /// `SYLPHX_BUILD_CACHE_URL` is not usable.
    BadUrl(String),
    TimedOut,
    Unreachable,
    Status {
        status: u16,
        code: Option<String>,
        message: String,
    },
    BadReply,
}

impl MintError {
    pub fn short(&self) -> String {
        match self {
            MintError::NoProject(_) => "no project".into(),
            MintError::BadUrl(_) => format!("{URL_ENV} is not usable"),
            MintError::TimedOut => "timed out".into(),
            MintError::Unreachable => "unreachable".into(),
            MintError::Status {
                status,
                code: Some(c),
                ..
            } => format!("HTTP {status} {c}"),
            MintError::Status { status, .. } => format!("HTTP {status}"),
            MintError::BadReply => "unreadable reply".into(),
        }
    }

    pub fn long(&self) -> String {
        match self {
            MintError::NoProject(why) | MintError::BadUrl(why) => why.clone(),
            MintError::Status { message, .. } if !message.is_empty() => {
                format!("the build cache answered {}: {message}", self.short())
            }
            other => format!("the build cache is unavailable ({})", other.short()),
        }
    }

    /// A refused argument is the caller's to fix; the rest is the platform's.
    pub fn is_usage(&self) -> bool {
        matches!(
            self,
            MintError::NoProject(_) | MintError::BadUrl(_) | MintError::Status { status: 400, .. }
        )
    }
}

/// The gateway's base URL.
pub fn base_url() -> String {
    std::env::var(URL_ENV)
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// The key is only ever sent over TLS; plain HTTP is for a gateway on this
/// machine (tests, a local gateway).
fn check_url(base: &str) -> Result<(), MintError> {
    let rest = if let Some(r) = base.strip_prefix("https://") {
        r
    } else if let Some(r) = base.strip_prefix("http://") {
        let host = r.split(['/', ':']).next().unwrap_or_default();
        let host = if r.starts_with('[') {
            r.split(']')
                .next()
                .unwrap_or_default()
                .trim_start_matches('[')
        } else {
            host
        };
        if !matches!(host, "localhost" | "127.0.0.1" | "::1") {
            return Err(MintError::BadUrl(format!(
                "{URL_ENV} must be https (plain http is allowed only for localhost)"
            )));
        }
        r
    } else {
        return Err(MintError::BadUrl(format!("{URL_ENV} must be an https URL")));
    };
    if rest.is_empty() {
        return Err(MintError::BadUrl(format!("{URL_ENV} has no host")));
    }
    Ok(())
}

/// `POST {base}/v1/tokens` with the caller's key. Bounded to 5 s, no
/// redirects (the key must not follow one), and every error is classified
/// without echoing the reply.
pub async fn mint(base: &str, key: &str, req: &Request) -> Result<Minted, MintError> {
    check_url(base)?;
    let http = reqwest::Client::builder()
        .timeout(MINT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| MintError::Unreachable)?;
    let resp = http
        .post(format!("{base}/v1/tokens"))
        .bearer_auth(key)
        .json(&req.body())
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                MintError::TimedOut
            } else {
                MintError::Unreachable
            }
        })?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| {
        if e.is_timeout() {
            MintError::TimedOut
        } else {
            MintError::BadReply
        }
    })?;
    if bytes.len() > MAX_REPLY_BYTES {
        return Err(MintError::BadReply);
    }
    let v: Option<Value> = serde_json::from_slice(&bytes).ok();
    if !status.is_success() {
        let err = v.as_ref().map(|v| &v["error"]);
        let text = |k: &str| {
            err.and_then(|e| e[k].as_str())
                .map(|s| clean(s, key))
                .filter(|s| !s.is_empty())
        };
        return Err(MintError::Status {
            status: status.as_u16(),
            code: text("code").filter(|c| c.chars().all(|c| c.is_ascii_uppercase() || c == '_')),
            message: text("message").unwrap_or_default(),
        });
    }
    parse_env(v.as_ref().ok_or(MintError::BadReply)?)
}

/// Gateway text for a human: no control characters, no key, short.
fn clean(s: &str, key: &str) -> String {
    let s = if key.is_empty() {
        s.to_string()
    } else {
        s.replace(key, "[redacted]")
    };
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The reply's `env`: a bounded map of valid variable names to strings.
fn parse_env(v: &Value) -> Result<Minted, MintError> {
    let obj = v["env"].as_object().ok_or(MintError::BadReply)?;
    if obj.len() > MAX_ENV_ENTRIES {
        return Err(MintError::BadReply);
    }
    let mut env = BTreeMap::new();
    for (k, val) in obj {
        let val = val.as_str().ok_or(MintError::BadReply)?;
        if !valid_name(k) || val.contains('\0') {
            return Err(MintError::BadReply);
        }
        env.insert(k.clone(), val.to_string());
    }
    let text = |k: &str| {
        v[k].as_str()
            .filter(|s| !s.is_empty() && !s.contains('\0'))
            .map(str::to_string)
    };
    Ok(Minted {
        env,
        token: text("token"),
        cache: text("cache"),
    })
}

fn valid_name(k: &str) -> bool {
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `text` with every secret replaced.
pub fn scrub(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .fold(text.to_string(), |t, s| t.replace(s.as_str(), "[redacted]"))
}

/// The project id of a resource name below `…/projects/prj_…`.
pub fn project_id(name: &str) -> Option<String> {
    let mut segs = name.split('/');
    while let Some(s) = segs.next() {
        if s == "projects" {
            return segs
                .next()
                .filter(|id| id.starts_with("prj_"))
                .map(str::to_string);
        }
    }
    None
}

// ---- `sylphx build cache env` -------------------------------------------

pub fn command() -> Command {
    Command::new("cache")
        .about("The shared build cache: settings for local builds")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("env")
                .about("Print the sccache and Turbo settings for this project's build cache as shell export lines")
                .long_about("Mints a read-only cache token (12 h) and prints `export NAME='value'` lines for the cache's environment, to be evaluated by a POSIX shell: eval \"$(sylphx build cache env)\". `export RUSTC_WRAPPER=sccache` is added when sccache is on PATH and RUSTC_WRAPPER is not set. -o json prints the environment as a JSON object.\n\nThe output holds a credential: do not log it.\n\nExit codes: 0; 2 usage (no project, refused argument); 125 platform failure (not signed in, the cache refused or is unreachable).")
                .arg(
                    Arg::new("project")
                        .long("project")
                        .value_name("ID|SLUG|NAME")
                        .help("The project (prj_… id, slug, or orgs/…/projects/…); default: the linked project, else the key's"),
                ),
        )
}

/// `sylphx build cache env`.
pub async fn run(creds: Option<Resolved>, m: &ArgMatches, format: Format) -> Result<(), Failure> {
    let (_, em) = m.subcommand().expect("subcommand_required");
    let Some(creds) = creds else {
        return fail(125, "not signed in: run `sylphx login`, or give an Access key with SYLPHX_API_KEY=sylphx_sk_…");
    };
    let client = context::client_for(&creds).map_err(Failure::Usage)?;
    let project = match project_of(&client, em.get_one::<String>("project")).await {
        Ok(p) => p,
        Err(Failure::Usage(e)) => return Err(Failure::Usage(e)),
        Err(Failure::Api(e)) => {
            return fail(
                125,
                &format!("reading the project: {}", crate::devices::why(&e)),
            )
        }
        Err(Failure::Refused(e)) => return fail(125, &e),
        Err(f) => return Err(f),
    };
    let minted = match mint(&base_url(), &creds.key, &local_request(project)).await {
        Ok(m) => m,
        Err(e) => {
            return if e.is_usage() {
                Err(Failure::Usage(e.long()))
            } else {
                fail(125, &e.long())
            }
        }
    };
    match format {
        Format::Json | Format::Yaml => {
            println!("{}", output::render(&json!(minted.env), format));
        }
        _ => {
            let wrapper_set = std::env::var_os("RUSTC_WRAPPER").is_some_and(|v| !v.is_empty());
            let path = std::env::var_os("PATH").unwrap_or_default();
            for line in export_lines(&minted.env, &path, wrapper_set) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

fn fail(code: u8, msg: &str) -> Result<(), Failure> {
    eprintln!("error: {msg}");
    Err(Failure::Exit(code))
}

/// `--project` (an id, a slug, or a name), else the linked or the key's.
async fn project_of(client: &sylphx::Client, arg: Option<&String>) -> Result<String, Failure> {
    let mut defaults = Defaults { client, link: None };
    let name = match arg {
        Some(p) if p.starts_with("prj_") => return Ok(p.clone()),
        Some(p) if p.starts_with("orgs/") => p.clone(),
        Some(p) => {
            let org = defaults.at("orgs/{org}").await?;
            format!("{org}/projects/{p}")
        }
        None => defaults
            .at("orgs/{org}/projects/{project}")
            .await
            .map_err(|_| {
                Failure::Usage("no project: pass --project, or run `sylphx link --env …`".into())
            })?,
    };
    let full = crate::names::resolve(client, &name).await?;
    project_id(&full).ok_or_else(|| Failure::Usage(format!("`{name}` is not a project")))
}

/// POSIX `export` lines for the environment, plus the sccache wrapper when
/// it is there to use.
pub fn export_lines(
    env: &BTreeMap<String, String>,
    path: &std::ffi::OsStr,
    wrapper_set: bool,
) -> Vec<String> {
    let mut lines: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("export {k}={}", shell_quote(v)))
        .collect();
    if !wrapper_set && !env.contains_key("RUSTC_WRAPPER") && on_path("sccache", path) {
        lines.push("export RUSTC_WRAPPER=sccache".into());
    }
    lines
}

/// Single-quoted for a POSIX shell.
pub fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', r"'\''"))
}

/// Whether an executable `name` is in a directory of `path`.
pub fn on_path(name: &str, path: &std::ffi::OsStr) -> bool {
    std::env::split_paths(path).any(|d| is_executable(&d.join(name)))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file() || p.with_extension("exe").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_a_shell() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("it's $HOME `x`"), r"'it'\''s $HOME `x`'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn ttl_is_the_run_plus_slack_capped() {
        assert_eq!(run_ttl_seconds(Duration::from_secs(3600)), 4500);
        assert_eq!(run_ttl_seconds(Duration::from_secs(6 * 3600)), 22_500);
        assert_eq!(run_ttl_seconds(Duration::from_secs(7 * 3600)), 22_500);
    }

    #[test]
    fn the_request_is_the_gateways_body() {
        let r = run_request("prj_a".into(), Duration::from_secs(60));
        assert_eq!(
            r.body(),
            json!({"project": "prj_a", "access": "write", "ttl_seconds": 960, "network": "cluster"})
        );
        assert_eq!(
            local_request("prj_a".into()).body(),
            json!({"project": "prj_a", "access": "read", "ttl_seconds": 43200, "network": "public"})
        );
    }

    #[test]
    fn project_ids_come_from_resource_names() {
        assert_eq!(
            project_id("orgs/org_a/projects/prj_b/envs/env_c"),
            Some("prj_b".into())
        );
        assert_eq!(project_id("orgs/org_a/projects/web"), None);
        assert_eq!(project_id("orgs/org_a"), None);
    }

    #[test]
    fn the_env_is_a_bounded_map_of_strings() {
        let ok = parse_env(&json!({"env": {"A_B": "x", "C": "y"}, "token": "t", "cache": "bc_1"}))
            .unwrap();
        assert_eq!(ok.env.len(), 2);
        assert_eq!(ok.token.as_deref(), Some("t"));
        assert_eq!(ok.cache.as_deref(), Some("bc_1"));
        for bad in [
            json!({}),
            json!({"env": []}),
            json!({"env": {"A": 1}}),
            json!({"env": {"1A": "x"}}),
            json!({"env": {"A B": "x"}}),
            json!({"env": {"A": "x\u{0}y"}}),
        ] {
            assert_eq!(parse_env(&bad), Err(MintError::BadReply), "{bad}");
        }
    }

    #[test]
    fn minted_does_not_print_its_values() {
        let m =
            parse_env(&json!({"env": {"TOKEN": "s3cret-value"}, "token": "s3cret-token"})).unwrap();
        assert!(m.secrets().contains(&"s3cret-token".to_string()));
        let shown = format!("{m:?}");
        assert!(
            shown.contains("TOKEN") && !shown.contains("s3cret"),
            "{shown}"
        );
    }

    #[test]
    fn plain_http_only_for_localhost() {
        assert!(check_url("https://build-cache.sylphx.net").is_ok());
        assert!(check_url("http://127.0.0.1:8080").is_ok());
        assert!(check_url("http://localhost").is_ok());
        assert!(check_url("http://[::1]:80").is_ok());
        assert!(check_url("http://cache.example.com").is_err());
        assert!(check_url("ftp://x").is_err());
        assert!(check_url("https://").is_err());
    }

    /// The default gateway is a hostname the build-cache service actually
    /// serves: a default with no route answers 404 and every run builds
    /// without the cache.
    #[test]
    fn default_url_is_a_served_build_cache_domain() {
        let toml = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../services/build/sylphx.toml");
        let Ok(text) = std::fs::read_to_string(&toml) else {
            return; // a packaged crate has no monorepo beside it
        };
        let host = DEFAULT_URL.trim_start_matches("https://");
        assert!(
            text.contains(&format!("hostname = \"{host}\"")),
            "{DEFAULT_URL} is not a [[domains]] hostname in {}",
            toml.display()
        );
    }

    #[test]
    fn scrub_and_clean_remove_secrets() {
        assert_eq!(
            scrub("a tok-12345678 b", &["tok-12345678".into()]),
            "a [redacted] b"
        );
        assert_eq!(
            clean("bad key sk_1 here\n", "sk_1"),
            "bad key [redacted] here"
        );
        assert!(clean(&"x".repeat(500), "k").len() <= 200);
    }

    #[test]
    fn exports_add_the_wrapper_only_when_sccache_is_there() {
        let dir = std::env::temp_dir().join(format!("sylphx-cache-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let env: BTreeMap<_, _> = [("A".to_string(), "x'y".to_string())].into();
        let empty = std::ffi::OsString::from(&dir);
        assert!(!on_path("sccache", &empty));
        let bin = dir.join("sccache");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(on_path("sccache", &empty));
        }
        #[cfg(unix)]
        {
            let lines = export_lines(&env, &empty, false);
            assert_eq!(
                lines,
                [r"export A='x'\''y'", "export RUSTC_WRAPPER=sccache"]
            );
            assert_eq!(export_lines(&env, &empty, true).len(), 1, "already set");
            let mut own = env.clone();
            own.insert("RUSTC_WRAPPER".into(), "x".into());
            assert_eq!(
                export_lines(&own, &empty, false).len(),
                2,
                "the gateway's own wins"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            export_lines(&env, std::ffi::OsStr::new(""), false),
            [r"export A='x'\''y'"]
        );
    }
}
