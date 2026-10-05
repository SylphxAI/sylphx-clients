//! `sylphx build run [PATH] -- <command…>`: runs a command on a remote copy
//! of this git work tree and behaves like the local command (the remote
//! build execution ADR, section 1). The output streams back, the exit code is
//! the command's own, and named outputs are copied back.
//!
//! A run takes a warm workspace (a Sandboxes Volume from a pool per project
//! and repository), leases a `build-<size>` machine with it mounted at
//! [`WS`], sends only the files that differ from the workspace's manifest
//! ([`sync`]), runs the command next to the warm `target/`, and always
//! releases the lease. A preempted or lost run, or one whose output stream
//! broke, is retried once after a short backoff. Anything that fails before
//! the command starts (the machine, the sync, the toolchain install) is a
//! platform failure, 125; once it has started, its own status is the exit code.
//!
//! `--region` runs in that region's Cell, with that region's warm workspaces
//! (a Volume lives in one Cell); no `--region` is the project's home region.
//! A region whose Cell offers no Volumes refuses the workspace at once
//! (`SHAPE_NOT_OFFERED`); the run then leases the machine with no Volume and
//! builds on its own disk: a cold workspace that ends with the lease, with
//! no local sccache (only a remote build cache the machine is given).
//! `--queue-timeout` bounds the wait for a machine: past it the run answers
//! 125, retryable, so a caller can try another region.
//!
//! Exit codes: the command's own; 2 usage; 124 `--timeout`; 125 a platform
//! failure (with `retryable` in the `result` event); 130 interrupted.

mod guest;
mod sync;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json, Value};
use sylphx::sandboxes as sbx;
use sylphx::{Client, HttpRequest};

use crate::build_cache;
use crate::devices::{duration, parent_env, release_request, state, why, wire};
use crate::Failure;
use guest::{Event, Fault, Guest};

pub const EXIT_TIMEOUT: u8 = 124;
pub const EXIT_PLATFORM: u8 = 125;
pub const EXIT_INTERRUPTED: u8 = 130;

/// The warm workspace's mount point; the tree is `WS/tree`.
const WS: &str = "/workspace";
const USER: &str = "user";
const TEMPLATE: &str = "template:build";
const SIZES: [&str; 3] = ["standard", "large", "xlarge"];
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// Sync, toolchain install and copy-back on top of `--timeout`.
const TTL_SLACK: Duration = Duration::from_secs(30 * 60);
const IDLE_TIMEOUT: &str = "600s";
/// How long a run waits for a machine before it answers 125 (retryable),
/// unless `--queue-timeout` says otherwise.
const DEFAULT_QUEUE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A Volume reaches available within this, or the run gives up on it.
const VOLUME_WAIT: Duration = Duration::from_secs(300);
/// A warm workspace pins the run to its node; past this the run takes a
/// fresh workspace on any node instead (a cold build, never a failed one).
const PIN_WAIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(2);
const POOL_POLL: Duration = Duration::from_secs(10);
/// A data-plane call this often keeps a silent command from idling out.
const KEEPALIVE: Duration = Duration::from_secs(60);
/// Lease tokens live an hour at most; renew well before.
const TOKEN_TTL: Duration = Duration::from_secs(3600);
const TOKEN_RENEW: Duration = Duration::from_secs(45 * 60);
/// Warm workspaces per (project, repository): the host's concurrent builds.
const MAX_WARM: usize = 10;
const VOLUME_GIB: i32 = 200;
const VOLUME_CLASS: &str = "local";
const POOL_PURPOSE: &str = "build-workspace";
/// The pool label naming a workspace's region; home-region workspaces have
/// none, so the pool without `--region` is what it always was.
const POOL_REGION: &str = "build-region";
/// The `build-packages` egress preset: package and toolchain hosts, and the
/// build-cache gateway's public name (a lease on the public network reaches
/// the cache there with its run token; in-cluster leases use the Service).
const BUILD_PACKAGES: [&str; 8] = [
    "index.crates.io",
    "static.crates.io",
    "static.rust-lang.org",
    "registry.npmjs.org",
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "build-cache.sylphx.net",
];

/// The lease's egress allow-list: the preset, then each `--allow-host`.
fn allowed_domains(extra: &[String]) -> Vec<String> {
    BUILD_PACKAGES
        .iter()
        .map(|h| h.to_string())
        .chain(extra.iter().cloned())
        .collect()
}

/// Prepares the workspace as root: the mount root belongs to the guest user,
/// and the manifest is offered compressed for the client to read.
const BOOTSTRAP: &str = r#"set -eu
W=$1
mkdir -p "$W/.sylphx"
chown user:user "$W" "$W/.sylphx" 2>/dev/null || true
rm -f "$W/.sylphx/manifest.gz"
if [ -f "$W/.sylphx/manifest" ]; then
  gzip -c "$W/.sylphx/manifest" > "$W/.sylphx/manifest.gz"
  chown user:user "$W/.sylphx/manifest.gz" 2>/dev/null || true
fi
"#;

/// The directories both guest scripts share: the build caches live on the
/// workspace (`$W`) so a warm machine reuses them.
macro_rules! guest_dirs {
    () => {
        r#"export CARGO_TARGET_DIR="$W/target" CARGO_HOME="$W/cargo" SCCACHE_DIR="$W/sccache" RUSTUP_HOME="$W/rustup"
export PATH="$W/cargo/bin:$PATH"
"#
    };
}

/// Everything that must be ready before the user's command starts: the tree
/// directory and, for a Rust tree, the toolchain named by its
/// `rust-toolchain.toml` (installed on first use onto the workspace). It runs
/// as its own process so a failure here is told apart from the command's own
/// exit status: any non-zero status is a platform failure (125), never the
/// command's. The install's log goes to `$W/.sylphx/toolchain.log` and its
/// tail to stderr on failure. A tree without Rust files (looked for up to the tree root) does not need the
/// toolchain, so a failed install does not stop its command.
const PROVISION: &str = concat!(
    r#"W=$1 R=$2
"#,
    guest_dirs!(),
    r#"cd "$W/tree/$R" || { echo "the work tree is missing on the machine" >&2; exit 3; }
if command -v rustup >/dev/null 2>&1 && ! rustup which rustc >/dev/null 2>&1; then
  { rustup toolchain install || rustup default stable; } > "$W/.sylphx/toolchain.log" 2>&1 || true
  if ! rustup which rustc >> "$W/.sylphx/toolchain.log" 2>&1; then
    d=$PWD
    while :; do
      if [ -e "$d/rust-toolchain.toml" ] || [ -e "$d/rust-toolchain" ] || [ -e "$d/Cargo.toml" ]; then
        echo "the Rust toolchain could not be installed:" >&2
        tail -n 6 "$W/.sylphx/toolchain.log" >&2
        exit 4
      fi
      [ "$d" = "$W/tree" ] && break
      d=$(dirname "$d")
    done
  fi
fi
exit 0
"#
);

/// Runs the command (`$4…`) in `$W/tree/$R` with the build caches on the
/// workspace: `target/`, the Cargo home, sccache, and rustup's toolchains
/// ([`PROVISION`] has installed them). `$3` is 1 on a machine without a
/// Volume: its disk ends with the lease, so sccache runs only against a remote
/// cache the machine is given, never a local directory that would double
/// `target/` on the same disk. The command's exit status is the script's.
const RUN: &str = concat!(
    r#"W=$1 R=$2 E=$3
shift 3
"#,
    guest_dirs!(),
    r#"export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-20G}"
if [ -z "${RUSTC_WRAPPER:-}" ] && command -v sccache >/dev/null 2>&1; then
  if [ "$E" != 1 ] || [ -n "${SCCACHE_WEBDAV_ENDPOINT:-}" ]; then export RUSTC_WRAPPER=sccache; fi
fi
cd "$W/tree/$R" || exit 125
exec "$@"
"#
);

/// Lists the files matching the globs (`$3…`, relative to `$W/tree/$R`) with
/// their SHA-256, for the copy-back.
const ARTIFACTS: &str = r#"W=$1 R=$2
shift 2
cd "$W/tree/$R" || exit 0
IFS=
for g in "$@"; do
  for f in $g; do
    [ -f "$f" ] && sha256sum -- "$f"
  done
done
exit 0
"#;

pub fn command() -> Command {
    Command::new("run")
        .about("Run a command on a remote build machine against this work tree; output, exit code and --artifact files come back")
        .long_about("Runs <command> on a remote copy of the enclosing git work tree, in PATH inside it. Only files that differ from the warm workspace are sent; target/, the Cargo registry, sccache and toolchains stay warm on it. In a region that offers no workspace Volumes the run builds cold on the machine's own disk. Output streams back unchanged apart from at most three `sylphx:` lines on stderr.\n\nThe run gets the project's shared build cache (sccache, Turbo) through its environment; if the cache cannot be reached the run builds without it and says so on one `sylphx: warning:` line. --no-cache skips it.\n\nExit codes: the command's own; 2 usage; 124 --timeout reached; 125 platform failure (not signed in, no capacity, no machine within --queue-timeout, region not offered, sync failed, the toolchain could not be installed, machine lost; anything before the command starts); 130 interrupted.\n\nExample: sylphx build run --region gra --queue-timeout 120s -- cargo test -p sylphx-cli")
        .arg(Arg::new("path").value_name("PATH").index(1)
            .help("Directory to run in (default \".\"); the synced root is its git work tree"))
        .arg(Arg::new("command").value_name("COMMAND").index(2).num_args(1..).last(true).required(true)
            .help("The command and its arguments, after --"))
        .arg(Arg::new("size").long("size").value_name("SIZE").default_value("large")
            .value_parser(SIZES)
            .help("Machine size: standard (8 vCPU), large (16), xlarge (32)"))
        .arg(Arg::new("timeout").long("timeout").value_name("DURATION")
            .help("Wall-clock limit for the command (default 60m, at most 6h)"))
        .arg(Arg::new("region").long("region").value_name("REGION")
            .help("Region to run in (default: the project's home region); each region keeps its own warm workspaces"))
        .arg(Arg::new("queue-timeout").long("queue-timeout").value_name("DURATION")
            .help("Longest wait for a build machine before exiting 125, retryable (default 30m, at most 6h)"))
        .arg(Arg::new("artifact").long("artifact").value_name("GLOB").action(ArgAction::Append)
            .help("Copy matching files back after the run (repeatable; relative to PATH on the remote side)"))
        .arg(Arg::new("out").long("out").value_name("DIR")
            .help("Where artifacts land (default .sylphx/out/<run id>/)"))
        .arg(Arg::new("env").long("env").value_name("NAME=VALUE").action(ArgAction::Append)
            .help("Non-secret environment for the command (repeatable)"))
        .arg(Arg::new("no-cache").long("no-cache").action(ArgAction::SetTrue)
            .help("Do not use the shared build cache (no token is minted)"))
        .arg(Arg::new("allow-host").long("allow-host").value_name("HOST").action(ArgAction::Append)
            .help("Extra egress host beyond the package hosts (repeatable)"))
        .arg(Arg::new("fresh").long("fresh").action(ArgAction::SetTrue)
            .help("Start from an empty workspace: no synced tree, no target/ or sccache"))
        .arg(Arg::new("dry-run").long("dry-run").action(ArgAction::SetTrue)
            .help("Print what a cold run would send (files, bytes) and exit 0"))
        .arg(Arg::new("quiet").long("quiet").short('q').action(ArgAction::SetTrue)
            .help("No `sylphx:` progress lines on stderr"))
}

#[derive(Debug)]
pub struct Opts {
    root: PathBuf,
    rel: String,
    command: Vec<String>,
    size: String,
    timeout: Duration,
    region: Option<String>,
    queue_timeout: Duration,
    artifacts: Vec<String>,
    out: Option<PathBuf>,
    env: BTreeMap<String, String>,
    allow_hosts: Vec<String>,
    fresh: bool,
    no_cache: bool,
    dry_run: bool,
    quiet: bool,
    json: bool,
}

impl Opts {
    pub fn parse(m: &ArgMatches) -> Result<Self, String> {
        let dir = PathBuf::from(
            m.get_one::<String>("path")
                .map(String::as_str)
                .unwrap_or("."),
        );
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        let (root, rel) = sync::work_tree(&dir)?;
        let command: Vec<String> = m
            .get_many::<String>("command")
            .map(|v| v.cloned().collect())
            .unwrap_or_default();
        if command.is_empty() || command[0].is_empty() {
            return Err("give the command after --: sylphx build run -- cargo test".into());
        }
        let timeout = match m.get_one::<String>("timeout") {
            Some(v) => duration(v, "--timeout")?,
            None => DEFAULT_TIMEOUT,
        };
        if timeout.is_zero() || timeout > MAX_TIMEOUT {
            return Err("--timeout is more than 0s and at most 6h".into());
        }
        let queue_timeout = match m.get_one::<String>("queue-timeout") {
            Some(v) => duration(v, "--queue-timeout")?,
            None => DEFAULT_QUEUE_TIMEOUT,
        };
        if queue_timeout.is_zero() || queue_timeout > MAX_TIMEOUT {
            return Err("--queue-timeout is more than 0s and at most 6h".into());
        }
        let region = m.get_one::<String>("region").cloned();
        if let Some(r) = &region {
            let ok = !r.is_empty()
                && r.len() <= 32
                && r.starts_with(|c: char| c.is_ascii_lowercase())
                && r.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !ok {
                return Err(format!(
                    "--region {r}: give a region name such as gra (lower-case letters, digits, -)"
                ));
            }
        }
        let mut env = BTreeMap::new();
        for kv in m.get_many::<String>("env").into_iter().flatten() {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| format!("--env {kv}: give NAME=VALUE"))?;
            let ok = !k.is_empty()
                && !k.starts_with(|c: char| c.is_ascii_digit())
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(format!("--env {kv}: `{k}` is not a variable name"));
            }
            env.insert(k.to_string(), v.to_string());
        }
        let mut allow_hosts = Vec::new();
        for h in m.get_many::<String>("allow-host").into_iter().flatten() {
            let ok = !h.is_empty()
                && h.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '*');
            if !ok {
                return Err(format!(
                    "--allow-host {h}: give a host name such as api.example.com"
                ));
            }
            allow_hosts.push(h.to_ascii_lowercase());
        }
        Ok(Self {
            root,
            rel,
            command,
            size: m
                .get_one::<String>("size")
                .cloned()
                .unwrap_or_else(|| "large".into()),
            timeout,
            region,
            queue_timeout,
            artifacts: m
                .get_many::<String>("artifact")
                .map(|v| v.cloned().collect())
                .unwrap_or_default(),
            out: m.get_one::<String>("out").map(PathBuf::from),
            env,
            allow_hosts,
            fresh: m.get_flag("fresh"),
            no_cache: m.get_flag("no-cache"),
            dry_run: m.get_flag("dry-run"),
            quiet: m.get_flag("quiet"),
            json: m.get_one::<String>("output").map(String::as_str) == Some("json"),
        })
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The command ran to its end with this exit status.
    Ran(i32),
    TimedOut,
    Interrupted,
    /// The platform could not run it; `retryable` says whether trying again
    /// may succeed.
    Platform {
        reason: String,
        retryable: bool,
    },
}

impl Outcome {
    pub fn code(&self) -> u8 {
        match self {
            // A local shell reports a status the same way: the low byte.
            Outcome::Ran(c) => (*c & 0xff) as u8,
            Outcome::TimedOut => EXIT_TIMEOUT,
            Outcome::Interrupted => EXIT_INTERRUPTED,
            Outcome::Platform { .. } => EXIT_PLATFORM,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Outcome::Ran(0) => "succeeded",
            Outcome::Ran(_) => "failed",
            Outcome::TimedOut => "timed_out",
            Outcome::Interrupted => "interrupted",
            Outcome::Platform { .. } => "platform_error",
        }
    }

    fn platform(reason: impl Into<String>, retryable: bool) -> Self {
        Outcome::Platform {
            reason: reason.into(),
            retryable,
        }
    }
}

/// Where progress, output and events go: raw output and `sylphx:` lines, or
/// NDJSON events on stdout with `-o json`.
struct Out {
    json: bool,
    quiet: bool,
}

impl Out {
    fn progress(&self, text: &str) {
        if !self.json && !self.quiet {
            eprintln!("sylphx: {text}");
        }
    }

    fn event(&self, v: Value) {
        if self.json {
            let mut o = std::io::stdout().lock();
            let _ = writeln!(o, "{v}");
            let _ = o.flush();
        }
    }

    fn stdout(&self, b: &[u8]) {
        if self.json {
            self.event(json!({"type": "stdout", "data": String::from_utf8_lossy(b)}));
        } else {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(b);
            let _ = o.flush();
        }
    }

    fn stderr(&self, b: &[u8]) {
        if self.json {
            self.event(json!({"type": "stderr", "data": String::from_utf8_lossy(b)}));
        } else {
            let mut e = std::io::stderr().lock();
            let _ = e.write_all(b);
            let _ = e.flush();
        }
    }
}

/// What the way out needs even when the run itself was dropped (Ctrl-C).
#[derive(Default)]
struct Held(Mutex<HeldState>);

#[derive(Default)]
struct HeldState {
    lease: Option<String>,
    proc: Option<(Guest, u64)>,
    /// Cache token values: removed from any text that leaves the process.
    secrets: Vec<String>,
}

impl Held {
    fn with<R>(&self, f: impl FnOnce(&mut HeldState) -> R) -> R {
        f(&mut self.0.lock().expect("not poisoned"))
    }
}

/// The counters of the `result` event.
#[derive(Default)]
struct Stats {
    warm: bool,
    bytes_up: u64,
    bytes_down: u64,
    artifacts: usize,
}

/// `sylphx build run`. `client` is the signed-in client, or why there is none.
pub async fn run(
    client: Result<Client, Failure>,
    key: Option<String>,
    m: &ArgMatches,
) -> Result<(), Failure> {
    let opts = Opts::parse(m).map_err(Failure::Usage)?;
    let out = Out {
        json: opts.json,
        quiet: opts.quiet,
    };
    let t0 = Instant::now();
    if opts.dry_run {
        return dry_run(&opts, &out);
    }
    let client = match client {
        Ok(c) => c,
        Err(f) => {
            let reason = match f {
                Failure::Usage(s) | Failure::Refused(s) => s,
                Failure::Api(e) => why(&e),
                Failure::Exit(_) => "not signed in".into(),
            };
            return finish(
                &out,
                &Outcome::platform(reason, false),
                &Stats::default(),
                t0,
            );
        }
    };
    let held = Held::default();
    let stats = Mutex::new(Stats::default());
    let outcome = tokio::select! {
        o = execute(&client, key.as_deref(), &opts, &out, &held, &stats) => o,
        _ = interrupted() => Outcome::Interrupted,
    };
    // Stop the command first, so nothing more is written to the workspace,
    // then end the machine. Either is bounded: an unreachable API must not
    // hold the caller.
    let (lease, proc) = held.with(|h| (h.lease.take(), h.proc.take()));
    let outcome = scrub(outcome, &held.with(|h| h.secrets.clone()));
    if matches!(outcome, Outcome::Interrupted) {
        if let Some((g, pid)) = proc {
            let _ = tokio::time::timeout(Duration::from_secs(5), g.signal(pid, "SIGKILL")).await;
        }
    }
    if let Some(name) = lease {
        release(&client, &name).await;
    }
    let stats = stats.into_inner().unwrap_or_default();
    finish(&out, &outcome, &stats, t0)
}

async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

async fn release(client: &Client, name: &str) {
    let r = tokio::time::timeout(
        Duration::from_secs(30),
        client.sandboxes().leases().release(release_request(name)),
    )
    .await;
    if !matches!(r, Ok(Ok(_))) {
        eprintln!("sylphx: warning: {name} was not released; its idle timeout ends it");
    }
}

/// The final `-o json` event.
fn result_event(outcome: &Outcome, s: &Stats, ms: u64) -> Value {
    let mut result = json!({
        "type": "result",
        "exit_code": outcome.code(),
        "outcome": outcome.name(),
        "retryable": matches!(outcome, Outcome::Platform { retryable: true, .. }),
        "duration_ms": ms,
        "workspace": if s.warm { "warm" } else { "cold" },
        "bytes_up": s.bytes_up,
        "bytes_down": s.bytes_down,
    });
    if let Outcome::Platform { reason, .. } = outcome {
        result["error"] = json!(reason);
    }
    result
}

/// The `result` event and summary line, and the exit status.
fn finish(out: &Out, outcome: &Outcome, s: &Stats, t0: Instant) -> Result<(), Failure> {
    let ms = t0.elapsed().as_millis() as u64;
    if let Outcome::Platform { reason, retryable } = outcome {
        eprintln!(
            "sylphx: error: {reason}{}",
            if *retryable { " [retryable]" } else { "" }
        );
    }
    out.event(result_event(outcome, s, ms));
    let what = match outcome {
        Outcome::Ran(c) => format!("exit {c}"),
        Outcome::TimedOut => "timed out".into(),
        Outcome::Interrupted => "interrupted".into(),
        Outcome::Platform { .. } => "platform failure".into(),
    };
    let arts = match s.artifacts {
        0 => String::new(),
        1 => "; 1 artifact".into(),
        n => format!("; {n} artifacts"),
    };
    out.progress(&format!("{what} in {:.1} s{arts}", ms as f64 / 1000.0));
    match outcome.code() {
        0 => Ok(()),
        c => Err(Failure::Exit(c)),
    }
}

fn dry_run(o: &Opts, out: &Out) -> Result<(), Failure> {
    let files = sync::file_set(&o.root).map_err(Failure::Usage)?;
    let mut cache = StatCache::open(&o.root);
    let tree = sync::scan(&o.root, &files, &mut cache.cache).map_err(Failure::Usage)?;
    cache.save();
    let plan = sync::plan(&tree, None);
    let bytes = plan.upload_bytes(&tree);
    out.event(json!({"type": "sync", "dry_run": true, "full": true, "files": plan.upload.len(), "bytes": bytes, "removed": 0}));
    if !o.json {
        println!(
            "{} files, {} to send to a cold workspace; a warm one gets only the changed files",
            plan.upload.len(),
            human(bytes)
        );
    }
    Ok(())
}

struct StatCache {
    path: Option<PathBuf>,
    cache: sync::StatCache,
}

impl StatCache {
    fn open(root: &Path) -> Self {
        let path = sync::cache_path(root);
        let cache = path
            .as_deref()
            .map(sync::StatCache::load)
            .unwrap_or_default();
        Self { path, cache }
    }

    fn save(&self) {
        if let Some(p) = &self.path {
            let _ = self.cache.save(p);
        }
    }
}

fn human(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / 1048576.0)
    } else if b >= 1 << 10 {
        format!("{:.0} KiB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

async fn execute(
    client: &Client,
    key: Option<&str>,
    o: &Opts,
    out: &Out,
    held: &Held,
    stats: &Mutex<Stats>,
) -> Outcome {
    let parent = match parent_env(client, None).await {
        Ok(p) => p,
        Err(e) => return Outcome::platform(e, false),
    };
    // The tree is hashed once; a retry after a preemption diffs it again
    // against whatever the workspace holds then.
    let files = match sync::file_set(&o.root) {
        Ok(f) => f,
        Err(e) => return Outcome::platform(format!("listing the work tree: {e}"), false),
    };
    let mut cache = StatCache::open(&o.root);
    let tree = match sync::scan(&o.root, &files, &mut cache.cache) {
        Ok(t) => t,
        Err(e) => return Outcome::platform(format!("hashing the work tree: {e}"), false),
    };
    cache.save();
    let repo = sync::repo_key(&o.root);
    let mut run = Run {
        client,
        o,
        out,
        held,
        stats,
        parent,
        repo,
        tree,
        key,
        cache_url: build_cache::base_url(),
        cache_warned: AtomicBool::new(false),
        no_volumes: false,
    };
    let mut retries = Retries::default();
    loop {
        match retries.next(run.attempt().await) {
            Step::Done(outcome) => return outcome,
            Step::Again { reason, backoff } => {
                out.progress(&format!("{reason}; retrying once"));
                out.event(json!({"type": "queued", "retry": true, "reason": reason}));
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Waited before the one retry, so a gateway that dropped a stream is not hit
/// again in the same instant.
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// The one retry a run gets, whatever lost it: a machine preempted or lost,
/// or a process stream that broke.
#[derive(Default)]
struct Retries {
    used: bool,
}

enum Step {
    Done(Outcome),
    Again { reason: String, backoff: Duration },
}

impl Retries {
    fn next(&mut self, a: Attempt) -> Step {
        match a {
            Attempt::Done(o) => Step::Done(o),
            Attempt::Lost(reason) if !self.used => {
                self.used = true;
                Step::Again {
                    reason,
                    backoff: RETRY_BACKOFF,
                }
            }
            Attempt::Lost(reason) => {
                Step::Done(Outcome::platform(format!("{reason}, twice"), true))
            }
        }
    }
}

enum Attempt {
    Done(Outcome),
    /// The machine was preempted or lost: worth one more try.
    Lost(String),
}

struct Run<'a> {
    client: &'a Client,
    o: &'a Opts,
    out: &'a Out,
    held: &'a Held,
    stats: &'a Mutex<Stats>,
    parent: String,
    repo: String,
    tree: sync::Tree,
    /// The caller's key, for minting the cache token.
    key: Option<&'a str>,
    cache_url: String,
    /// The one `build cache unavailable` warning is given once per run.
    cache_warned: AtomicBool,
    /// The region refused a workspace Volume (`SHAPE_NOT_OFFERED`): every
    /// attempt of this run leases a machine without one.
    no_volumes: bool,
}

/// A ready machine with its workspace.
struct Machine {
    lease: sbx::Lease,
    warm_volume: bool,
    /// Whether a Volume is mounted at [`WS`] (else the machine's own disk).
    volume: bool,
}

impl Run<'_> {
    /// The region asked for, or "" for the home region.
    fn region(&self) -> &str {
        self.o.region.as_deref().unwrap_or("")
    }

    /// " in region `<r>`" when a region was asked for.
    fn where_(&self) -> String {
        match &self.o.region {
            Some(r) => format!(" in region {r}"),
            None => String::new(),
        }
    }

    fn add(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.stats.lock().expect("not poisoned"));
    }

    async fn attempt(&mut self) -> Attempt {
        let m = match self.acquire().await {
            Ok(m) => m,
            Err(a) => return a,
        };
        let name = m.lease.name.clone();
        let mut guest = match self.guest(&m.lease).await {
            Ok(g) => g,
            Err(e) => return self.lost_or(&name, e).await,
        };
        let mut minted = Instant::now();
        let warm = match self.sync(&guest, &m).await {
            Ok(w) => w,
            Err(e) => return self.lost_or(&name, e).await,
        };
        if let Err(a) = self.provision(&guest, &name).await {
            return a;
        }
        let shape = format!("build-{}", self.o.size);
        let (cache, warned) = self.cache().await;
        // At most three `sylphx:` lines: a warning takes the place of this one.
        if !warned {
            self.out.progress(&format!(
                "running on {shape}, {} workspace{}",
                if warm { "warm" } else { "cold" },
                if m.volume {
                    String::new()
                } else {
                    format!(" on its own disk (no Volumes{})", self.where_())
                }
            ));
        }
        self.out.event(
            json!({"type": "running", "lease": name, "size": self.o.size, "region": self.region(), "workspace": if warm { "warm" } else { "cold" }, "volume": m.volume, "cache": !cache.is_empty()}),
        );
        let code = match self
            .exec(&mut guest, &mut minted, &name, m.volume, cache)
            .await
        {
            Ok(Some(c)) => c,
            Ok(None) => return Attempt::Done(Outcome::TimedOut),
            Err(e) => return self.lost_or(&name, e).await,
        };
        if !self.o.artifacts.is_empty() {
            if let Err(e) = self.collect(&guest, &name).await {
                return Attempt::Done(Outcome::platform(
                    format!("copying artifacts back: {e}"),
                    true,
                ));
            }
        }
        Attempt::Done(Outcome::Ran(code))
    }

    /// The cache environment for the command, minted just before it starts so
    /// the token covers the run. Empty with `--no-cache`, and when no token
    /// can be had (a cache that is missing only slows the build); the bool
    /// says the warning line was written.
    async fn cache(&self) -> (BTreeMap<String, String>, bool) {
        if self.o.no_cache {
            return (BTreeMap::new(), false);
        }
        let minted = match (self.key, build_cache::project_id(&self.parent)) {
            (Some(key), Some(project)) => {
                let req = build_cache::run_request(project, self.o.timeout);
                build_cache::mint(&self.cache_url, key, &req).await
            }
            (None, _) => Err(build_cache::MintError::NoProject("not signed in".into())),
            (_, None) => Err(build_cache::MintError::NoProject(
                "the environment names no project".into(),
            )),
        };
        match minted {
            Ok(m) => {
                self.held.with(|h| h.secrets.extend(m.secrets()));
                (m.env, false)
            }
            Err(e) => {
                let first = !self.cache_warned.swap(true, Ordering::Relaxed);
                if first {
                    eprintln!(
                        "sylphx: warning: build cache unavailable ({}); building without it",
                        e.short()
                    );
                }
                (BTreeMap::new(), first)
            }
        }
    }

    /// Installs what the command needs before it starts. Any failure here is
    /// a platform failure (125, retryable), never the command's own status.
    async fn provision(&self, g: &Guest, name: &str) -> Result<(), Attempt> {
        let args = argv(&[
            "/bin/sh",
            "-c",
            PROVISION,
            "sylphx-provision",
            WS,
            self.rel(),
        ]);
        let ran =
            tokio::time::timeout(PROVISION_TIMEOUT, g.run(USER, &args, &BTreeMap::new())).await;
        match ran {
            Err(_) => Err(Attempt::Done(Outcome::platform(
                format!(
                    "preparing the build machine took longer than {}s",
                    PROVISION_TIMEOUT.as_secs()
                ),
                true,
            ))),
            Ok(Ok((0, _, _))) => Ok(()),
            Ok(Ok((code, _, err))) => Err(Attempt::Done(Outcome::platform(
                provision_failure(code, &err),
                true,
            ))),
            Ok(Err(f)) => Err(self.lost_or(name, f).await),
        }
    }

    /// The tree directory the command runs in, relative to the tree root.
    fn rel(&self) -> &str {
        if self.o.rel.is_empty() {
            "."
        } else {
            &self.o.rel
        }
    }

    /// A guest fault means the machine may be gone: if the lease ended
    /// preempted or lost, or the process stream broke, the run is worth one
    /// retry; otherwise it is a platform failure.
    async fn lost_or(&self, name: &str, f: Fault) -> Attempt {
        let mut get = sbx::GetLeaseRequest::default();
        get.name = name.to_string();
        let lease = self.client.sandboxes().leases().get(get).await.ok();
        let reason = lease
            .as_ref()
            .and_then(|l| l.status.as_ref())
            .and_then(|s| s.end_reason.as_ref())
            .map(|r| r.as_str().to_string());
        let ended = lease
            .as_ref()
            .map(|l| matches!(state(l), sbx::LeaseState::Ending | sbx::LeaseState::Ended))
            .unwrap_or(false);
        let verdict = judge(reason.as_deref(), &f);
        // A retry takes a new machine: end this one first, or it idles on.
        let release_now = !ended && matches!(verdict, Verdict::Retry(_));
        self.held.with(|h| {
            h.proc = None;
            if ended || release_now {
                h.lease = None;
            }
        });
        if release_now {
            release(self.client, name).await;
        }
        match verdict {
            Verdict::Retry(why) => Attempt::Lost(why),
            Verdict::Fail { reason, retryable } => {
                Attempt::Done(Outcome::platform(reason, retryable))
            }
        }
    }

    /// A ready lease of `build-<size>` with a workspace from the pool, or,
    /// where the region offers no Volumes, on the machine's own disk.
    async fn acquire(&mut self) -> Result<Machine, Attempt> {
        let deadline = Instant::now() + self.o.queue_timeout;
        let mut queued = false;
        loop {
            if self.no_volumes {
                return self.acquire_without_volume(deadline).await;
            }
            let pool = self.pool().await.map_err(Attempt::Done)?;
            let live = pool
                .iter()
                .filter(|v| {
                    !matches!(
                        vstate(v),
                        sbx::VolumeState::Failed | sbx::VolumeState::Deleting
                    )
                })
                .count();
            let mut free: Vec<&sbx::Volume> = pool
                .iter()
                .filter(|v| vstate(v) == sbx::VolumeState::Available)
                .collect();
            free.sort_by(|a, b| a.name.cmp(&b.name));
            let mut leased = None;
            for v in free {
                match self.lease(Some(&v.name)).await {
                    Ok(l) => {
                        leased = Some((l, true));
                        break;
                    }
                    Err(LeaseErr::InUse) => continue,
                    Err(LeaseErr::Fatal(o)) => return Err(Attempt::Done(o)),
                }
            }
            if leased.is_none() && live < MAX_WARM {
                let Some(vol) = self.new_volume(deadline).await.map_err(Attempt::Done)? else {
                    self.no_volumes = true;
                    continue;
                };
                match self.lease(Some(&vol)).await {
                    Ok(l) => leased = Some((l, false)),
                    Err(LeaseErr::InUse) => {}
                    Err(LeaseErr::Fatal(o)) => return Err(Attempt::Done(o)),
                }
            }
            let Some((lease, warm_volume)) = leased else {
                if Instant::now() >= deadline {
                    return Err(Attempt::Done(Outcome::platform(
                        format!(
                            "every warm workspace{} stayed busy past --queue-timeout",
                            self.where_()
                        ),
                        true,
                    )));
                }
                if !queued {
                    queued = true;
                    self.out.progress("waiting for a free workspace");
                }
                tokio::time::sleep(POOL_POLL).await;
                continue;
            };
            self.out
                .event(json!({"type": "queued", "lease": lease.name, "size": self.o.size, "region": self.region()}));
            // A warm workspace pins the run to its node; fall back to a fresh
            // workspace elsewhere only while the pool has room for one.
            let pin = (warm_volume && live < MAX_WARM).then_some(PIN_WAIT);
            match self.ready(lease, pin, deadline).await? {
                Some(l) => {
                    return Ok(Machine {
                        lease: l,
                        warm_volume,
                        volume: true,
                    })
                }
                None => {
                    self.out
                        .progress("the warm workspace's node is busy; using a fresh workspace");
                    let Some(vol) = self.new_volume(deadline).await.map_err(Attempt::Done)? else {
                        self.no_volumes = true;
                        continue;
                    };
                    let l = match self.lease(Some(&vol)).await {
                        Ok(l) => l,
                        Err(LeaseErr::InUse) => continue,
                        Err(LeaseErr::Fatal(o)) => return Err(Attempt::Done(o)),
                    };
                    if let Some(l) = self.ready(l, None, deadline).await? {
                        return Ok(Machine {
                            lease: l,
                            warm_volume: false,
                            volume: true,
                        });
                    }
                }
            }
        }
    }

    /// A ready lease with no Volume: the region's Cell offers none, so the
    /// workspace is the machine's own disk, created with it and gone with
    /// it. Nothing is created that could outlive the run.
    async fn acquire_without_volume(&mut self, deadline: Instant) -> Result<Machine, Attempt> {
        let lease = match self.lease(None).await {
            Ok(l) => l,
            Err(LeaseErr::InUse) => {
                return Err(Attempt::Done(Outcome::platform(
                    "the build machine was refused: in use",
                    true,
                )))
            }
            Err(LeaseErr::Fatal(o)) => return Err(Attempt::Done(o)),
        };
        self.out.event(
            json!({"type": "queued", "lease": lease.name, "size": self.o.size, "region": self.region(), "volume": false}),
        );
        match self.ready(lease, None, deadline).await? {
            Some(l) => Ok(Machine {
                lease: l,
                warm_volume: false,
                volume: false,
            }),
            // Without a pin, `ready` answers a lease or an outcome.
            None => Err(Attempt::Done(Outcome::platform(
                format!(
                    "no build machine{} within --queue-timeout {}s",
                    self.where_(),
                    self.o.queue_timeout.as_secs()
                ),
                true,
            ))),
        }
    }

    /// The pool: this project's (environment's) workspaces of this repository.
    async fn pool(&self) -> Result<Vec<sbx::Volume>, Outcome> {
        let mut req = sbx::ListVolumesRequest::default();
        req.parent = self.parent.clone();
        let all = self
            .client
            .sandboxes()
            .volumes()
            .list_all(req)
            .await
            .map_err(|e| api("listing workspaces", &e))?;
        Ok(all
            .into_iter()
            .filter(|v| in_pool(v, &self.repo, self.o.region.as_deref()))
            .collect())
    }

    /// A new pool workspace, once it is available (by `deadline` at the
    /// latest, the run's wait for a machine). `None` when the region offers
    /// no Volumes: the API refuses with `SHAPE_NOT_OFFERED` before creating
    /// anything, so no workspace is left behind.
    async fn new_volume(&self, deadline: Instant) -> Result<Option<String>, Outcome> {
        let mut body = json!({
            "meta": {"labels": {"purpose": POOL_PURPOSE, "build-repo": self.repo}},
            "spec": {"sizeGib": VOLUME_GIB, "storageClass": VOLUME_CLASS},
        });
        if let Some(r) = &self.o.region {
            body["meta"]["labels"][POOL_REGION] = json!(r);
            body["spec"]["region"] = json!(r);
        }
        let v: sbx::Volume = match self
            .client
            .call(HttpRequest {
                method: "POST",
                path: format!("/v1/{}/volumes", self.parent),
                query: vec![],
                body: Some(body),
                mutation: true,
                origin: None,
                effect_ids: false,
            })
            .await
        {
            Ok(v) => v,
            Err(sylphx::Error::Api { code, .. }) if code.as_str() == "SHAPE_NOT_OFFERED" => {
                return Ok(None)
            }
            Err(e) => return Err(api("creating a workspace", &e)),
        };
        let deadline = deadline.min(Instant::now() + VOLUME_WAIT);
        let mut v = v;
        loop {
            match vstate(&v) {
                sbx::VolumeState::Available => return Ok(Some(v.name)),
                sbx::VolumeState::Failed => {
                    return Err(Outcome::platform(
                        format!("the workspace {} failed to provision", v.name),
                        true,
                    ))
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(Outcome::platform(
                    format!(
                        "the workspace {}{} was not ready in time",
                        v.name,
                        self.where_()
                    ),
                    true,
                ));
            }
            tokio::time::sleep(POLL).await;
            let mut get = sbx::GetVolumeRequest::default();
            get.name = v.name.clone();
            v = self
                .client
                .sandboxes()
                .volumes()
                .get(get)
                .await
                .map_err(|e| api("reading the workspace", &e))?;
        }
    }

    /// Creates the build lease, with `volume` at [`WS`] or, without one, on
    /// the machine's own disk.
    async fn lease(&self, volume: Option<&str>) -> Result<sbx::Lease, LeaseErr> {
        let mut spec = sbx::LeaseSpec::default();
        spec.shape = format!("build-{}", self.o.size);
        spec.image = TEMPLATE.into();
        spec.region = self.region().to_string();
        spec.kind = Some(sbx::LeaseKind::General);
        spec.ttl = wire((self.o.timeout + TTL_SLACK).min(Duration::from_secs(24 * 3600)));
        spec.idle_timeout = IDLE_TIMEOUT.into();
        let mut net = sbx::LeaseNetwork::default();
        net.egress = Some(sbx::EgressPolicy::Allowlist);
        net.allowed_domains = allowed_domains(&self.o.allow_hosts);
        spec.network = Some(net);
        if let Some(volume) = volume {
            let mut mount = sbx::VolumeMount::default();
            mount.volume = volume.to_string();
            mount.mount_path = WS.into();
            spec.volumes = vec![mount];
        }
        let mut meta = sylphx::common::ResourceMeta::default();
        meta.labels.insert("purpose".into(), "build-run".into());
        let mut lease = sbx::Lease::default();
        lease.meta = Some(meta);
        lease.spec = Some(spec);
        let mut req = sbx::CreateLeaseRequest::default();
        req.parent = self.parent.clone();
        req.lease = Some(lease);
        match self.client.sandboxes().leases().create(req).await {
            Ok(l) => {
                self.held.with(|h| h.lease = Some(l.name.clone()));
                Ok(l)
            }
            Err(sylphx::Error::Api { code, .. }) if code.as_str().contains("IN_USE") => {
                Err(LeaseErr::InUse)
            }
            Err(e) => Err(LeaseErr::Fatal(api("creating the build machine", &e))),
        }
    }

    /// Waits for READY. With `pin`, gives up after that long (and releases
    /// the lease) so the caller can take a fresh workspace.
    async fn ready(
        &self,
        mut lease: sbx::Lease,
        pin: Option<Duration>,
        deadline: Instant,
    ) -> Result<Option<sbx::Lease>, Attempt> {
        let t = Instant::now();
        let leases = self.client.sandboxes().leases();
        loop {
            match state(&lease) {
                sbx::LeaseState::Ready => return Ok(Some(lease)),
                sbx::LeaseState::Refused => {
                    self.held.with(|h| h.lease = None);
                    let why = lease
                        .status
                        .as_ref()
                        .and_then(|s| s.refusal.as_ref())
                        .map(|r| r.as_str().to_string())
                        .unwrap_or_default();
                    let retryable = why == "no_capacity";
                    return Err(Attempt::Done(Outcome::platform(
                        format!("the build machine was refused: {why}"),
                        retryable,
                    )));
                }
                sbx::LeaseState::Ending | sbx::LeaseState::Ended => {
                    self.held.with(|h| h.lease = None);
                    let r = lease
                        .status
                        .as_ref()
                        .and_then(|s| s.end_reason.as_ref())
                        .map(|r| r.as_str().to_string())
                        .unwrap_or_default();
                    if r == "preempted" || r == "machine_lost" {
                        return Err(Attempt::Lost(format!(
                            "the build machine was {} before it started",
                            r.replace('_', " ")
                        )));
                    }
                    return Err(Attempt::Done(Outcome::platform(
                        format!("the build machine ended before it was ready: {r}"),
                        r == "boot_failed",
                    )));
                }
                _ => {}
            }
            if pin.is_some_and(|p| t.elapsed() >= p) || Instant::now() >= deadline {
                release(self.client, &lease.name).await;
                self.held.with(|h| h.lease = None);
                if pin.is_some() && Instant::now() < deadline {
                    return Ok(None);
                }
                return Err(Attempt::Done(Outcome::platform(
                    format!(
                        "no build machine{} within --queue-timeout {}s",
                        self.where_(),
                        self.o.queue_timeout.as_secs()
                    ),
                    true,
                )));
            }
            tokio::time::sleep(POLL).await;
            let mut get = sbx::GetLeaseRequest::default();
            get.name = lease.name.clone();
            lease = leases
                .get(get)
                .await
                .map_err(|e| Attempt::Done(api("reading the build machine", &e)))?;
        }
    }

    async fn token(&self, lease: &str) -> Result<String, Fault> {
        let mut req = sbx::MintLeaseTokenRequest::default();
        req.name = lease.to_string();
        req.scopes = vec![sbx::TokenScope::Guest];
        req.ttl = wire(TOKEN_TTL);
        self.client
            .sandboxes()
            .leases()
            .mint_token(req)
            .await
            .map(|r| r.token)
            .map_err(|e| Fault::Other(format!("minting a guest token: {}", why(&e))))
    }

    async fn guest(&self, lease: &sbx::Lease) -> Result<Guest, Fault> {
        let ep = lease
            .status
            .as_ref()
            .and_then(|s| s.endpoints.clone())
            .unwrap_or_default();
        if ep.guest_uri.is_empty() || ep.e2b_sandbox_id.is_empty() {
            return Err(Fault::Other(
                "the build machine has no guest endpoint".into(),
            ));
        }
        let token = self.token(&lease.name).await?;
        Guest::new(&ep.guest_uri, &ep.e2b_sandbox_id, &token).map_err(Fault::Other)
    }

    /// Brings the workspace's tree to this tree; answers whether it was warm
    /// (a trusted manifest, so only the diff was sent).
    async fn sync(&self, g: &Guest, m: &Machine) -> Result<bool, Fault> {
        let t = Instant::now();
        let none = BTreeMap::new();
        let (code, _, err) = g
            .run(
                "root",
                &argv(&["/bin/sh", "-c", BOOTSTRAP, "bootstrap", WS]),
                &none,
            )
            .await?;
        if code != 0 {
            return Err(Fault::Other(format!(
                "preparing the workspace failed: {}",
                String::from_utf8_lossy(&err).trim()
            )));
        }
        let remote = if self.o.fresh {
            None
        } else {
            match g
                .download(&format!("{WS}/.sylphx/manifest.gz"), USER)
                .await?
            {
                Some(gz) => {
                    self.add(|s| s.bytes_down += gz.len() as u64);
                    gunzip(&gz).and_then(|t| sync::parse(&t))
                }
                None => None,
            }
        };
        let warm = remote.is_some();
        let plan = sync::plan(&self.tree, remote.as_ref());
        let bytes = plan.upload_bytes(&self.tree);
        let tars = sync::tarballs(&self.o.root, &plan.upload, &self.tree).map_err(Fault::Other)?;
        let lists = sync::lists(&self.tree, &plan);
        let s = format!("{WS}/.sylphx");
        let mut up = 0u64;
        for (i, t) in tars.iter().enumerate() {
            g.upload(&format!("{s}/in-{i:04}.tar.gz"), USER, t).await?;
            up += t.len() as u64;
        }
        for (name, body) in [
            ("drop", lists.drop.as_bytes()),
            ("add", lists.add.as_bytes()),
            ("remove", &lists.remove[..]),
        ] {
            if !body.is_empty() {
                g.upload(&format!("{s}/{name}"), USER, body).await?;
                up += body.len() as u64;
            }
        }
        self.add(|st| {
            st.bytes_up += up;
            st.warm = warm;
        });
        let full = match (self.o.fresh, plan.full) {
            (true, _) => "2",
            (false, true) => "1",
            (false, false) => "0",
        };
        let (code, _, err) = g
            .run(
                USER,
                &argv(&["/bin/sh", "-c", sync::APPLY, "apply", WS, full]),
                &none,
            )
            .await?;
        if code != 0 {
            return Err(Fault::Other(format!(
                "applying the sync failed: {}",
                String::from_utf8_lossy(&err).trim()
            )));
        }
        let secs = t.elapsed().as_secs_f64();
        self.out.progress(&format!(
            "synced {} files, {}, in {secs:.1} s{}",
            plan.upload.len(),
            human(up),
            if plan.remove.is_empty() {
                String::new()
            } else {
                format!("; {} removed", plan.remove.len())
            }
        ));
        self.out.event(json!({
            "type": "sync",
            "full": plan.full,
            "files": plan.upload.len(),
            "removed": plan.remove.len(),
            "bytes": bytes,
            "bytes_up": up,
            "duration_ms": t.elapsed().as_millis() as u64,
            "workspace": if warm { "warm" } else { "cold" },
            "new_volume": m.volume && !m.warm_volume,
            "volume": m.volume,
        }));
        Ok(warm)
    }

    /// Runs the command, streaming its output; `None` when `--timeout` hit.
    async fn exec(
        &self,
        g: &mut Guest,
        minted: &mut Instant,
        lease: &str,
        volume: bool,
        cache: BTreeMap<String, String>,
    ) -> Result<Option<i32>, Fault> {
        let mut args = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            RUN.into(),
            "sylphx-run".into(),
            WS.into(),
            self.rel().to_string(),
            if volume { "0" } else { "1" }.into(),
        ];
        args.extend(self.o.command.iter().cloned());
        // The command's own `--env` wins over the cache's.
        let mut env = cache;
        env.extend(self.o.env.clone());
        let mut p = g.start(USER, &args, "", &env).await?;
        let deadline = tokio::time::Instant::now() + self.o.timeout;
        let mut tick = tokio::time::interval(KEEPALIVE);
        tick.tick().await;
        let result = loop {
            tokio::select! {
                ev = p.next() => match ev? {
                    Some(Event::Start(pid)) => {
                        let g2 = g.clone();
                        self.held.with(|h| h.proc = Some((g2, pid)));
                    }
                    Some(Event::Stdout(b)) => self.out.stdout(&b),
                    Some(Event::Stderr(b)) => self.out.stderr(&b),
                    Some(Event::End(c)) => break Some(c),
                    Some(Event::Error(m)) => return Err(Fault::Other(format!("the command could not run: {m}"))),
                    None => return Err(Fault::Stream("the output stream closed before the command ended".into())),
                },
                _ = tick.tick() => {
                    if minted.elapsed() >= TOKEN_RENEW {
                        if let Ok(t) = self.token(lease).await {
                            g.set_token(&t);
                            *minted = Instant::now();
                        }
                    }
                    let _ = g.touch(WS).await;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let pid = self.held.with(|h| h.proc.as_ref().map(|(_, p)| *p));
                    if let Some(pid) = pid {
                        let _ = g.signal(pid, "SIGKILL").await;
                    }
                    break None;
                }
            }
        };
        self.add(|s| s.bytes_down += p.bytes);
        self.held.with(|h| h.proc = None);
        Ok(result)
    }

    /// Copies the `--artifact` matches back, checking each digest.
    async fn collect(&self, g: &Guest, lease: &str) -> Result<(), String> {
        let rel = if self.o.rel.is_empty() {
            "."
        } else {
            &self.o.rel
        };
        let mut args = argv(&[
            "/bin/bash",
            "-O",
            "globstar",
            "-O",
            "nullglob",
            "-c",
            ARTIFACTS,
            "artifacts",
            WS,
            rel,
        ]);
        args.extend(self.o.artifacts.iter().cloned());
        let (_, listing, _) = g
            .run(USER, &args, &BTreeMap::new())
            .await
            .map_err(|f| f.text().to_string())?;
        let dir = self.o.out.clone().unwrap_or_else(|| {
            PathBuf::from(".sylphx/out").join(lease.rsplit('/').next().unwrap_or("run"))
        });
        let mut n = 0usize;
        for line in String::from_utf8_lossy(&listing).lines() {
            let Some((digest, path)) = parse_sum(line) else {
                continue;
            };
            let Some(local) = artifact_path(&dir, path) else {
                continue;
            };
            let remote = format!("{WS}/tree/{rel}/{path}");
            let bytes = g
                .download(&remote, USER)
                .await
                .map_err(|f| f.text().to_string())?
                .ok_or_else(|| format!("{path} disappeared before it was copied"))?;
            if sync::sha256_hex(&bytes) != digest {
                return Err(format!("{path} changed while it was copied"));
            }
            if let Some(parent) = local.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            std::fs::write(&local, &bytes).map_err(|e| format!("{}: {e}", local.display()))?;
            self.add(|s| {
                s.bytes_down += bytes.len() as u64;
                s.artifacts += 1;
            });
            self.out.event(json!({
                "type": "artifact",
                "path": local.to_string_lossy(),
                "digest": format!("sha256:{digest}"),
                "bytes": bytes.len(),
            }));
            n += 1;
        }
        if n == 0 {
            self.out.progress("no file matched --artifact");
        }
        Ok(())
    }
}

enum LeaseErr {
    /// The workspace is attached to another lease.
    InUse,
    Fatal(Outcome),
}

/// How long preparing the machine (the toolchain install) may take.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// What a guest fault means for the run.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Worth one more try on a new machine.
    Retry(String),
    Fail {
        reason: String,
        retryable: bool,
    },
}

/// `end_reason` is why the lease ended, when it has.
fn judge(end_reason: Option<&str>, f: &Fault) -> Verdict {
    match end_reason {
        Some(r @ ("preempted" | "machine_lost")) => {
            Verdict::Retry(format!("the build machine was {}", r.replace('_', " ")))
        }
        Some(r) => Verdict::Fail {
            reason: format!("the build machine ended: {r}"),
            retryable: false,
        },
        None => match f {
            Fault::Stream(t) => Verdict::Retry(t.clone()),
            other => Verdict::Fail {
                reason: other.text().to_string(),
                retryable: true,
            },
        },
    }
}

/// The reason for a non-zero [`PROVISION`] status, with the last lines it
/// wrote to stderr.
fn provision_failure(code: i32, stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let tail: String = text
        .trim()
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        format!("preparing the build machine failed (status {code})")
    } else {
        format!("preparing the build machine failed (status {code}): {tail}")
    }
}

/// A refused API call as a platform failure; retryable as the API says, and
/// always when the API could not be reached.
fn api(what: &str, e: &sylphx::Error) -> Outcome {
    let retryable = match e {
        sylphx::Error::Api { retryable, .. } => *retryable,
        sylphx::Error::Transport(_) => true,
        _ => false,
    };
    Outcome::platform(format!("{what}: {}", why(e)), retryable)
}

/// An outcome's text with the cache token removed.
fn scrub(outcome: Outcome, secrets: &[String]) -> Outcome {
    match outcome {
        Outcome::Platform { reason, retryable } => Outcome::Platform {
            reason: build_cache::scrub(&reason, secrets),
            retryable,
        },
        other => other,
    }
}

/// Whether a Volume is one of this pool's workspaces: this repository's, in
/// this region (`None`, the home region, is the pool without a region label).
fn in_pool(v: &sbx::Volume, repo: &str, region: Option<&str>) -> bool {
    let l = v.meta.as_ref().map(|m| &m.labels);
    let label = |k: &str| l.and_then(|l| l.get(k)).map(String::as_str);
    label("purpose") == Some(POOL_PURPOSE)
        && label("build-repo") == Some(repo)
        && label(POOL_REGION) == region
}

fn vstate(v: &sbx::Volume) -> sbx::VolumeState {
    v.status
        .as_ref()
        .and_then(|s| s.state.clone())
        .unwrap_or(sbx::VolumeState::Unknown(String::new()))
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn gunzip(b: &[u8]) -> Option<String> {
    let mut s = String::new();
    flate2::read::GzDecoder::new(b)
        .read_to_string(&mut s)
        .ok()?;
    Some(s)
}

/// One `sha256sum` line: the digest and the path. Escaped lines (a path with
/// a backslash or newline) are skipped.
fn parse_sum(line: &str) -> Option<(&str, &str)> {
    if line.starts_with('\\') {
        return None;
    }
    let (digest, rest) = line.split_once("  ")?;
    (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then_some((digest, rest))
}

/// Where an artifact lands: its remote path under `dir`, with `.` and `..`
/// dropped so nothing lands outside `dir`.
fn artifact_path(dir: &Path, remote: &str) -> Option<PathBuf> {
    let mut p = dir.to_path_buf();
    let mut any = false;
    for c in Path::new(remote).components() {
        if let Component::Normal(s) = c {
            p.push(s);
            any = true;
        }
    }
    any.then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Opts, String> {
        let mut root = Command::new("sylphx").arg(
            Arg::new("output")
                .long("output")
                .short('o')
                .global(true)
                .default_value("table"),
        );
        root = root.subcommand(Command::new("build").subcommand(command()));
        let m = root.try_get_matches_from(args).map_err(|e| e.to_string())?;
        let (_, b) = m.subcommand().unwrap();
        let (_, r) = b.subcommand().unwrap();
        Opts::parse(r)
    }

    #[test]
    fn lease_egress_allows_the_package_hosts_the_cache_gateway_and_allow_hosts() {
        let d = allowed_domains(&["proxy.golang.org".to_string()]);
        assert_eq!(d.len(), BUILD_PACKAGES.len() + 1);
        assert!(d.contains(&"build-cache.sylphx.net".to_string()));
        assert!(d.contains(&"index.crates.io".to_string()));
        assert_eq!(d.last().map(String::as_str), Some("proxy.golang.org"));
        assert_eq!(allowed_domains(&[]).len(), BUILD_PACKAGES.len());
    }

    // The guest scripts run for real under /bin/sh in a scratch workspace;
    // rustup is a fake placed where the script puts the workspace's cargo/bin.
    struct Ws(PathBuf);

    impl Ws {
        fn new(tag: &str) -> Ws {
            let d = std::env::temp_dir().join(format!(
                "sylphx-build-run-{tag}-{}-{}",
                std::process::id(),
                guest::rand_u64()
            ));
            std::fs::create_dir_all(d.join("tree")).unwrap();
            std::fs::create_dir_all(d.join(".sylphx")).unwrap();
            std::fs::create_dir_all(d.join("cargo/bin")).unwrap();
            Ws(d)
        }

        fn file(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }

        /// A fake rustup: `which rustc` succeeds once `installed` exists;
        /// `toolchain install` creates it unless `fail_install`.
        fn rustup(&self, fail_install: bool) {
            let w = self.0.display();
            let install = if fail_install {
                "exit 1".to_string()
            } else {
                format!("touch '{w}/installed'")
            };
            let body = format!(
                "#!/bin/sh\ncase \"$1\" in\n which) [ -e '{w}/installed' ] ;;\n toolchain) echo 'error: could not download channel-rust-1.99.0.toml.sha256 (Connection timed out)' >&2; {install} ;;\n default) echo 'error: no network' >&2; exit 1 ;;\nesac\n"
            );
            self.file("cargo/bin/rustup", &body);
            use std::os::unix::fs::PermissionsExt;
            let p = self.0.join("cargo/bin/rustup");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn sh(&self, script: &str, rel: &str, extra: &[&str]) -> (i32, String) {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .arg("sylphx-test")
                .arg(&self.0)
                .arg(rel)
                .args(extra)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
    }

    impl Drop for Ws {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_failed_toolchain_install_stops_a_rust_tree_before_its_command() {
        let w = Ws::new("tc-fail");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(true);
        let (code, err) = w.sh(PROVISION, ".", &[]);
        assert_eq!(code, 4, "{err}");
        assert!(err.contains("could not be installed"), "{err}");
        assert!(
            err.contains("Connection timed out"),
            "the log's tail is shown: {err}"
        );
        // Whatever the status, a failed preparation is a platform failure.
        let o = Outcome::platform(provision_failure(code, err.as_bytes()), true);
        assert_eq!(o.code(), 125);
        assert!(
            matches!(&o, Outcome::Platform { reason, .. } if reason.contains("Connection timed out"))
        );
    }

    #[test]
    fn a_toolchain_file_above_the_run_directory_counts_as_a_rust_tree() {
        let w = Ws::new("tc-sub");
        w.file(
            "tree/rust-toolchain.toml",
            "[toolchain]\nchannel = \"1.99.0\"\n",
        );
        w.file("tree/sub/dir/x.txt", "x");
        w.rustup(true);
        assert_eq!(w.sh(PROVISION, "sub/dir", &[]).0, 4);
    }

    #[test]
    fn a_failed_install_does_not_stop_a_tree_without_rust() {
        let w = Ws::new("tc-none");
        w.file("tree/package.json", "{}");
        w.rustup(true);
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
    }

    #[test]
    fn a_successful_install_provisions() {
        let w = Ws::new("tc-ok");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        let (code, err) = w.sh(PROVISION, ".", &[]);
        assert_eq!(code, 0, "{err}");
        assert!(w.0.join("installed").exists());
        // Already installed: nothing to do.
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
    }

    #[test]
    fn a_missing_tree_fails_provisioning() {
        let w = Ws::new("no-tree");
        let (code, err) = w.sh(PROVISION, "gone", &[]);
        assert_eq!(code, 3, "{err}");
    }

    #[test]
    fn the_commands_own_failure_keeps_its_exit_code() {
        let w = Ws::new("run");
        w.file("tree/x", "");
        for want in [0, 1, 2, 7, 101] {
            let (code, _) = w.sh(RUN, ".", &["0", "/bin/sh", "-c", &format!("exit {want}")]);
            assert_eq!(code, want);
            assert_eq!(Outcome::Ran(code).code() as i32, want);
        }
        // Not a platform failure: the code is the command's, not 125.
        assert_ne!(Outcome::Ran(101).code(), EXIT_PLATFORM);
        // The tree missing is a failure before the command starts.
        assert_eq!(w.sh(RUN, "gone", &["0", "/bin/sh", "-c", "exit 0"]).0, 125);
    }

    #[test]
    fn a_stream_error_is_retried_once_with_a_backoff_then_is_125() {
        let mut r = Retries::default();
        let stream =
            || Attempt::Lost("the process stream broke: error decoding response body".into());
        match r.next(stream()) {
            Step::Again { reason, backoff } => {
                assert!(reason.contains("process stream broke"));
                assert!(backoff >= Duration::from_secs(1), "{backoff:?}");
            }
            Step::Done(o) => panic!("the first loss is retried: {o:?}"),
        }
        match r.next(stream()) {
            Step::Done(o) => {
                assert_eq!(o.code(), 125);
                assert!(
                    matches!(&o, Outcome::Platform { reason, retryable: true } if reason.ends_with(", twice"))
                );
            }
            Step::Again { .. } => panic!("only one retry"),
        }
    }

    #[test]
    fn a_retry_that_succeeds_keeps_the_commands_exit_code() {
        let mut r = Retries::default();
        assert!(matches!(
            r.next(Attempt::Lost("x".into())),
            Step::Again { .. }
        ));
        match r.next(Attempt::Done(Outcome::Ran(101))) {
            Step::Done(o) => assert_eq!((o.code(), o), (101, Outcome::Ran(101))),
            Step::Again { .. } => panic!("done is done"),
        }
        // A command that failed on its own is never retried.
        match Retries::default().next(Attempt::Done(Outcome::Ran(1))) {
            Step::Done(Outcome::Ran(1)) => {}
            _ => panic!("a command's own failure is not retried"),
        }
    }

    #[test]
    fn faults_are_judged_retry_or_fail() {
        let stream = Fault::Stream("the process stream broke".into());
        assert_eq!(
            judge(None, &stream),
            Verdict::Retry("the process stream broke".into())
        );
        for r in ["preempted", "machine_lost"] {
            assert!(matches!(
                judge(Some(r), &Fault::Gone("x".into())),
                Verdict::Retry(_)
            ));
        }
        // The machine ended for another reason: no retry, even on a broken stream.
        assert!(matches!(
            judge(Some("idle"), &stream),
            Verdict::Fail {
                retryable: false,
                ..
            }
        ));
        for f in [
            Fault::Gone("guest unreachable".into()),
            Fault::Other("denied".into()),
        ] {
            assert!(matches!(
                judge(None, &f),
                Verdict::Fail {
                    retryable: true,
                    ..
                }
            ));
        }
    }

    #[test]
    fn exit_codes_follow_docker_run_and_timeout() {
        assert_eq!(Outcome::Ran(0).code(), 0);
        assert_eq!(
            Outcome::Ran(101).code(),
            101,
            "the command's own status passes through"
        );
        assert_eq!(Outcome::Ran(2).code(), 2);
        assert_eq!(Outcome::Ran(137).code(), 137);
        assert_eq!(Outcome::Ran(256 + 3).code(), 3);
        assert_eq!(Outcome::TimedOut.code(), 124);
        assert_eq!(Outcome::platform("no capacity", true).code(), 125);
        assert_eq!(Outcome::Interrupted.code(), 130);
        assert_eq!(Outcome::Ran(0).name(), "succeeded");
        assert_eq!(Outcome::Ran(1).name(), "failed");
    }

    #[test]
    fn the_command_follows_the_double_dash() {
        let o = parse(&["sylphx", "build", "run", "--", "cargo", "test", "-p", "x"]).unwrap();
        assert_eq!(o.command, ["cargo", "test", "-p", "x"]);
        assert_eq!(o.size, "large");
        assert_eq!(o.timeout, DEFAULT_TIMEOUT);
        assert_eq!(o.region, None, "no --region: the home region");
        assert_eq!(o.queue_timeout, DEFAULT_QUEUE_TIMEOUT);
        assert!(!o.json);
        let o = parse(&[
            "sylphx",
            "-o",
            "json",
            "build",
            "run",
            ".",
            "--size",
            "xlarge",
            "--timeout",
            "90m",
            "--env",
            "RUST_LOG=debug",
            "--allow-host",
            "Proxy.Golang.org",
            "--artifact",
            "a/*",
            "--",
            "make",
        ])
        .unwrap();
        assert!(o.json);
        assert_eq!(o.size, "xlarge");
        assert_eq!(o.timeout, Duration::from_secs(5400));
        assert_eq!(o.env.get("RUST_LOG").map(String::as_str), Some("debug"));
        assert_eq!(o.allow_hosts, ["proxy.golang.org"]);
        assert_eq!(o.artifacts, ["a/*"]);
    }

    /// A caller that prefers one region: a region and a short wait for a machine.
    #[test]
    fn region_and_queue_timeout_parse() {
        let o = parse(&[
            "sylphx",
            "build",
            "run",
            "--size",
            "standard",
            "--region",
            "gra",
            "--queue-timeout",
            "120s",
            "--timeout",
            "60m",
            "--",
            "cargo",
            "check",
        ])
        .unwrap();
        assert_eq!(o.region.as_deref(), Some("gra"));
        assert_eq!(o.queue_timeout, Duration::from_secs(120));
        assert_eq!(o.timeout, Duration::from_secs(3600));
    }

    /// A Volume lives in one region's Cell: a run uses only its region's
    /// workspaces, and the home pool is exactly the pool before regions.
    #[test]
    fn the_pool_is_per_region() {
        fn vol(labels: &[(&str, &str)]) -> sbx::Volume {
            let mut meta = sylphx::common::ResourceMeta::default();
            for (k, v) in labels {
                meta.labels.insert(k.to_string(), v.to_string());
            }
            let mut v = sbx::Volume::default();
            v.meta = Some(meta);
            v
        }
        let home = vol(&[("purpose", POOL_PURPOSE), ("build-repo", "r")]);
        let gra = vol(&[
            ("purpose", POOL_PURPOSE),
            ("build-repo", "r"),
            (POOL_REGION, "gra"),
        ]);
        let other = vol(&[("purpose", POOL_PURPOSE), ("build-repo", "s")]);
        assert!(in_pool(&home, "r", None));
        assert!(!in_pool(&gra, "r", None));
        assert!(in_pool(&gra, "r", Some("gra")));
        assert!(!in_pool(&home, "r", Some("gra")));
        assert!(!in_pool(&gra, "r", Some("fra")));
        assert!(!in_pool(&other, "r", None));
    }

    /// A caller sends --region only when `build run --help` lists both
    /// flags, so the help text is part of the contract.
    #[test]
    fn help_lists_region_and_queue_timeout() {
        let help = command().render_long_help().to_string();
        assert!(help.contains("--region"), "{help}");
        assert!(help.contains("--queue-timeout"), "{help}");
    }

    #[test]
    fn usage_errors_are_caught_before_anything_is_created() {
        assert!(parse(&["sylphx", "build", "run"]).is_err(), "no command");
        assert!(
            parse(&["sylphx", "build", "run", "cargo"]).is_err(),
            "no --"
        );
        for bad in [
            &["--timeout", "7h"][..],
            &["--timeout", "0s"],
            &["--timeout", "soon"],
            &["--queue-timeout", "0s"],
            &["--queue-timeout", "7h"],
            &["--queue-timeout", "later"],
            &["--region", "GRA"],
            &["--region", "gra west"],
            &["--region", ""],
            &["--region", "-gra"],
            &["--size", "huge"],
            &["--env", "NOVALUE"],
            &["--env", "1X=y"],
            &["--allow-host", "a b"],
            &["/nonexistent-dir-for-test"],
        ] {
            let mut args = vec!["sylphx", "build", "run"];
            args.extend_from_slice(bad);
            args.extend_from_slice(&["--", "true"]);
            assert!(parse(&args).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn artifacts_never_land_outside_the_out_dir() {
        let d = Path::new("/out");
        assert_eq!(
            artifact_path(d, "target/release/app"),
            Some(PathBuf::from("/out/target/release/app"))
        );
        assert_eq!(
            artifact_path(d, "../../target/x"),
            Some(PathBuf::from("/out/target/x"))
        );
        assert_eq!(
            artifact_path(d, "/etc/passwd"),
            Some(PathBuf::from("/out/etc/passwd"))
        );
        assert_eq!(artifact_path(d, ".."), None);
    }

    #[test]
    fn sha256sum_lines_parse() {
        let h = "a".repeat(64);
        assert_eq!(
            parse_sum(&format!("{h}  dir/file name")),
            Some((h.as_str(), "dir/file name"))
        );
        assert_eq!(parse_sum(&format!("\\{h}  odd\\nname")), None);
        assert_eq!(parse_sum("short  x"), None);
    }

    /// Every `-o json` event has a `type` the schema knows and that type's
    /// required fields, with the right JSON kinds.
    #[test]
    fn json_events_match_the_schema() {
        /// A required field and the JSON kind it must have.
        type Field = (&'static str, fn(&Value) -> bool);
        fn check(v: &Value) {
            let t = v["type"].as_str().expect("type");
            let need: &[Field] = match t {
                "sync" => &[
                    ("files", Value::is_u64),
                    ("bytes", Value::is_u64),
                    ("full", Value::is_boolean),
                ],
                "queued" => &[],
                "running" => &[("workspace", Value::is_string)],
                "stdout" | "stderr" => &[("data", Value::is_string)],
                "artifact" => &[
                    ("path", Value::is_string),
                    ("digest", Value::is_string),
                    ("bytes", Value::is_u64),
                ],
                "result" => &[
                    ("exit_code", Value::is_u64),
                    ("outcome", Value::is_string),
                    ("retryable", Value::is_boolean),
                    ("duration_ms", Value::is_u64),
                    ("workspace", Value::is_string),
                    ("bytes_up", Value::is_u64),
                    ("bytes_down", Value::is_u64),
                ],
                other => panic!("unknown event type {other}"),
            };
            for (k, ok) in need {
                assert!(ok(&v[*k]), "{t}.{k} in {v}");
            }
        }
        // The events as the code builds them.
        let stats = Stats {
            warm: true,
            bytes_up: 512,
            bytes_down: 2048,
            artifacts: 0,
        };
        let captured = Mutex::new(Vec::new());
        let sink = |v: Value| captured.lock().unwrap().push(v);
        for o in [
            Outcome::Ran(0),
            Outcome::Ran(101),
            Outcome::TimedOut,
            Outcome::Interrupted,
            Outcome::platform("x", true),
        ] {
            let r = result_event(&o, &stats, 1);
            assert_eq!(r["exit_code"], json!(o.code()));
            assert_eq!(r["retryable"], json!(o == Outcome::platform("x", true)));
            sink(r);
        }
        sink(json!({"type": "stdout", "data": String::from_utf8_lossy(b"ok\n")}));
        sink(
            json!({"type": "sync", "full": false, "files": 1, "removed": 0, "bytes": 10, "bytes_up": 300, "duration_ms": 5, "workspace": "warm", "new_volume": false}),
        );
        sink(json!({"type": "running", "lease": "l", "size": "large", "workspace": "warm"}));
        sink(json!({"type": "artifact", "path": "p", "digest": "sha256:aa", "bytes": 3}));
        for v in captured.lock().unwrap().iter() {
            check(v);
        }
    }
}
