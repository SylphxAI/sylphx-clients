//! `sylphx build run [PATH] -- <command…>`: runs a command on a remote copy
//! of this git work tree and behaves like the local command (the remote
//! build execution ADR, section 1). The output streams back, the exit code is
//! the command's own, and named outputs are copied back.
//!
//! Every run leases a `build-<size>` machine with a cold workspace at [`WS`].
//! The workspace is created only after admission, belongs to the lease's
//! workload, and is reclaimed when the lease ends. Scoped remote build caches
//! reuse build outputs without retaining a local workspace.
//! Every run releases the lease. Before the guest action launches, a lost
//! machine or broken sync stream is retried once after a short backoff.
//! Known setup failures are platform failures, 125. Once an action launches
//! it is never replayed: a broken stream reattaches to that process, and losing
//! it is 137 because its command may have run while the stream was down.
//! The private guest start marker distinguishes a setup failure from the
//! command's own exit status, including 125.
//!
//! `--region` pins placement to that Cell; otherwise the API selects Build
//! capacity. Neither path creates or reserves a persistent workspace Volume.
//! Once a lease is accepted, every internal retry stays in its returned Cell,
//! including a lease lost before READY.
//! `--queue-timeout` bounds the wait for a machine, API retries before the
//! machine is obtained included: past it the run answers 125, retryable, so
//! a caller can try another region.
//!
//! Exit codes: the command's own; 2 usage; 124 `--timeout`; 125 a platform
//! failure before the command started (with `retryable` in the `result`
//! event); 130 interrupted; 137 the machine was lost after the command
//! started.

mod git_deps;
mod history;
mod image;
mod labels;
mod pack_guest;
mod packs;
mod redact;
mod remote;
mod store;
mod sync;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::{Arg, ArgAction, ArgMatches, Command};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use sylphx::sandboxes as sbx;
use sylphx::Client;

use crate::devices::{duration, parent_env, state, why};
use crate::Failure;
use crate::{build_cache, build_source};
use sylphx_build_lease::guest::{Event, Fault, Guest, Reattach, REATTACH_BUDGET};
use sylphx_build_lease::scripts::{start_script, Startup, ARTIFACTS};
use sylphx_build_lease::{
    allowed_domains, judge, lease_request, LeaseParams, Ready, Verdict, BUILD_CACHE_HOST, WS,
};

/// `sylphx build image`'s arguments.
pub fn image_command() -> Command {
    image::command()
}

pub const EXIT_TIMEOUT: u8 = 124;
pub const EXIT_PLATFORM: u8 = 125;
pub const EXIT_INTERRUPTED: u8 = 130;
/// The command started and its machine was lost: 128 + SIGKILL, as a shell
/// reports a process the kernel killed, never 125 (that is only for anything
/// before the command starts).
pub const EXIT_LOST: u8 = 137;

const USER: &str = "user";

/// Bound network fan-out and retain input order, including errors. Drain every
/// operation before returning so no upload or credential mint outlives setup.
async fn pipeline<I, F, Fut, T>(items: I, work: F) -> Vec<T>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    stream::iter(items).map(work).buffered(4).collect().await
}
const SIZES: [&str; 3] = ["standard", "large", "xlarge"];
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
/// The machine wait includes admission and a cold boot; retain 30m unless
/// the caller overrides it.
const DEFAULT_QUEUE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A data-plane call this often keeps a silent command from idling out.
const KEEPALIVE: Duration = Duration::from_secs(60);
/// Renew the lease token well before it expires.
const TOKEN_RENEW: Duration = Duration::from_secs(45 * 60);
/// The registry issuer caps each managed credential at one hour.
const REGISTRY_TOKEN_RENEW: Duration = Duration::from_secs(30 * 60);

/// The lease's egress allow-list: for an image build [`image::allowed_domains`]
/// (no internet package host), otherwise [`allowed_domains`] (the
/// `build-packages` preset, then each `--allow-host`).
fn lease_domains(o: &Opts) -> Vec<String> {
    match &o.image {
        Some(img) => image::allowed_domains(img, &o.allow_hosts),
        None => allowed_domains(&o.allow_hosts),
    }
}

pub fn command() -> Command {
    Command::new("run")
        .about("Run a command on a remote build machine against this source directory; output, exit code and --artifact files come back")
        .long_about("Runs <command> on a remote copy of PATH, or its enclosing Git work tree when present. No Git initialization or manifest is required. Ordinary directories honor .gitignore; optional .sylphxignore exclusions apply to both source types. Every run gets a cold, lease-owned workspace created only after admission and reclaimed at lease end. Scoped remote caches reuse build outputs; no persistent workspace Volume is created or attached, including with --region. Output streams back unchanged apart from `sylphx:` progress lines on stderr. While admission or cold boot is pending, a line every 30s names the lease, its last public state or admission condition, and elapsed time. --quiet suppresses these lines; -o json keeps only machine events.\n\nThe run gets the project's scoped shared build cache (sccache, Turbo) through its environment; if the cache cannot be reached the run builds without it and says so on one `sylphx: warning:` line. --no-cache skips it. Cargo uses CARGO_INCREMENTAL=0 with sccache and CARGO_PROFILE_DEV_DEBUG=0. --env overrides these defaults. --fresh keeps the empty-tree behavior.\n\nAdmission and cold boot have a default 30m wait budget. --queue-timeout optionally overrides that budget; keep the default unless you need a different wait limit.\n\nExit codes: the command's own; 2 usage; 124 --timeout reached; 125 platform failure before the command starts; 130 interrupted; 137 the machine was lost after the command started (it is never run twice; a broken output stream is reattached to the running command).\n\nExample: sylphx build run --size standard -- cargo test")
        .arg(Arg::new("path").value_name("PATH").index(1)
            .help("Directory to run in (default \".\"); syncs it or its enclosing Git work tree"))
        .arg(Arg::new("command").value_name("COMMAND").index(2).num_args(1..).last(true).required(true)
            .help("The command and its arguments, after --"))
        .arg(Arg::new("size").long("size").value_name("SIZE").default_value("large")
            .value_parser(SIZES)
            .help("Machine size: standard (8 vCPU), large (16), xlarge (32)"))
        .arg(Arg::new("timeout").long("timeout").value_name("DURATION")
            .help("Wall-clock limit for the command (default 60m, at most 6h)"))
        .arg(Arg::new("region").long("region").value_name("REGION")
            .help("Region to pin the run to (default: automatic Build placement); workspace lifetime always follows the lease"))
        .arg(Arg::new("queue-timeout").long("queue-timeout").value_name("DURATION")
            .help("Longest wait for a build machine before exiting 125, retryable (default 30m, at most 6h)"))
        .arg(Arg::new("artifact").long("artifact").value_name("GLOB").action(ArgAction::Append)
            .help("Copy matching files back after the run (repeatable; relative to PATH on the remote side)"))
        .arg(Arg::new("out").long("out").value_name("DIR")
            .help("Where artifacts land (default .sylphx/out/<run id>/)"))
        .arg(Arg::new("env").long("env").value_name("NAME=VALUE").action(ArgAction::Append)
            .help("Non-secret environment for the command (repeatable)"))
        .arg(Arg::new("label").long("label").value_name("KEY=VALUE").action(ArgAction::Append)
            .help("Attribution label on the leased machine, for accounting (repeatable; lower-case letters, digits, `_`, `-`)"))
        .arg(Arg::new("no-cache").long("no-cache").action(ArgAction::SetTrue)
            .help("Do not use the shared build cache (no token is minted)"))
        .arg(Arg::new("allow-host").long("allow-host").value_name("HOST").action(ArgAction::Append)
            .help("Internet host the run may reach (repeatable); without it only the in-cell build cache and its crates and npm mirrors"))
        .arg(Arg::new("source-history").long("source-history").value_name("REF")
            .help("Send HEAD and a local ref or commit SHA's full Git history offline; required for commands such as git merge-base. Keeps synced local changes"))
        .arg(Arg::new("fresh").long("fresh").action(ArgAction::SetTrue)
            .help("Start from an empty workspace: no synced tree, no target/ or sccache"))
        .arg(Arg::new("dry-run").long("dry-run").action(ArgAction::SetTrue)
            .help("Print what a cold run would send (files, bytes) and exit 0"))
        .arg(Arg::new("quiet").long("quiet").short('q').action(ArgAction::SetTrue)
            .help("No `sylphx:` progress lines on stderr"))
        .arg(Arg::new("no-tree").long("no-tree").action(ArgAction::SetTrue)
            .conflicts_with_all(["path", "artifact", "out", "env", "label", "region", "fresh", "no-cache", "dry-run", "source-history"])
            .help("Run on the Build service with no input tree: the run is a Build record, its output streams back from its log"))
        .arg(Arg::new("no-wait").long("no-wait").action(ArgAction::SetTrue).requires("no-tree")
            .help("Create the Build and print it without waiting; read the output later with `sylphx build logs`"))
}

/// `sylphx build logs <build> [--follow]`: the log of a Build on the service.
pub fn logs_command() -> Command {
    Command::new("logs")
        .about("Print a build's log; with --follow, keep printing until the build ends")
        .long_about("Prints the log of a Build created by `sylphx build run --no-tree`, oldest line first: the command's stdout to stdout, its stderr and the platform's `sylphx:` lines to stderr. <BUILD> is a build name (orgs/{org}/projects/{project}/builds/{build}) or the id of a build in the linked project. With --follow the command waits for new lines and returns when the build has ended; without it, it prints what is written now.\n\nExample: sylphx build logs b01j8x --follow")
        .arg(Arg::new("build").value_name("BUILD").required(true).index(1)
            .help("The build name or id"))
        .arg(Arg::new("follow").long("follow").short('f').action(ArgAction::SetTrue)
            .help("Wait for new lines until the build ends"))
}

/// `sylphx build logs`.
pub async fn logs(client: Result<Client, Failure>, m: &ArgMatches) -> Result<(), Failure> {
    let client = client?;
    let arg = m.get_one::<String>("build").cloned().unwrap_or_default();
    let name = if arg.starts_with("orgs/") {
        arg
    } else {
        let project = match remote::project(
            &client,
            m.try_get_one::<String>("org")
                .ok()
                .flatten()
                .map(String::as_str),
        )
        .await
        {
            Ok(p) => p,
            Err(Outcome::Platform { reason, .. }) => return Err(Failure::Usage(reason)),
            Err(_) => return Err(Failure::Exit(EXIT_PLATFORM)),
        };
        format!("{project}/builds/{arg}")
    };
    let out = Out {
        json: false,
        quiet: false,
    };
    tokio::select! {
        r = remote::follow(&client, &name, &out, m.get_flag("follow")) => match r {
            Ok(()) => Ok(()),
            Err(Outcome::Platform { reason, .. }) => Err(Failure::Refused(reason)),
            Err(_) => Err(Failure::Exit(EXIT_PLATFORM)),
        },
        _ = interrupted() => Err(Failure::Exit(EXIT_INTERRUPTED)),
    }
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
    /// The caller's own attribution labels, sorted, carried on the lease's
    /// metadata beside `purpose` (see [`labels::parse`]).
    labels: Vec<(String, String)>,
    org: Option<String>,
    allow_hosts: Vec<String>,
    fresh: bool,
    source_history: Option<String>,
    no_cache: bool,
    dry_run: bool,
    quiet: bool,
    json: bool,
    /// Set for `build image`: `command` is rendered from it.
    image: Option<image::Image>,
    /// Run as a Build on the service, over no tree.
    no_tree: bool,
    no_wait: bool,
}

/// The limits and placement every kind of build takes alike.
struct Shared {
    root: PathBuf,
    rel: String,
    size: String,
    timeout: Duration,
    region: Option<String>,
    queue_timeout: Duration,
    allow_hosts: Vec<String>,
}

impl Shared {
    /// `no_tree`: a run over no tree has no work tree to find.
    fn parse(m: &ArgMatches, no_tree: bool) -> Result<Self, String> {
        let dir = PathBuf::from(
            m.get_one::<String>("path")
                .map(String::as_str)
                .unwrap_or("."),
        );
        if !no_tree && !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        let (root, rel) = if no_tree {
            (PathBuf::new(), String::new())
        } else {
            sync::work_tree(&dir)?
        };
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
            size: m
                .get_one::<String>("size")
                .cloned()
                .unwrap_or_else(|| "large".into()),
            timeout,
            region,
            queue_timeout,
            allow_hosts,
        })
    }
}

impl Opts {
    /// `sylphx build run`.
    pub fn parse(m: &ArgMatches) -> Result<Self, String> {
        let no_tree = m.get_flag("no-tree");
        let sh = Shared::parse(m, no_tree)?;
        let command: Vec<String> = m
            .get_many::<String>("command")
            .map(|v| v.cloned().collect())
            .unwrap_or_default();
        if command.is_empty() || command[0].is_empty() {
            return Err("give the command after --: sylphx build run -- cargo test".into());
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
            if k == crate::context::ENV_VAR {
                return Err(format!(
                    "--env {k}: it names this machine's default environment and is never sent to the run"
                ));
            }
            env.insert(k.to_string(), v.to_string());
        }
        let labels = labels::collect(
            m.get_many::<String>("label")
                .into_iter()
                .flatten()
                .map(|kv| labels::parse(kv))
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        Ok(Self {
            org: m.try_get_one::<String>("org").ok().flatten().cloned(),
            root: sh.root,
            rel: sh.rel,
            command,
            size: sh.size,
            timeout: sh.timeout,
            region: sh.region,
            queue_timeout: sh.queue_timeout,
            artifacts: m
                .get_many::<String>("artifact")
                .map(|v| v.cloned().collect())
                .unwrap_or_default(),
            out: m.get_one::<String>("out").map(PathBuf::from),
            env,
            labels,
            allow_hosts: sh.allow_hosts,
            fresh: m.get_flag("fresh"),
            source_history: m.get_one::<String>("source-history").cloned(),
            no_cache: m.get_flag("no-cache"),
            dry_run: m.get_flag("dry-run"),
            quiet: m.get_flag("quiet"),
            json: m.get_one::<String>("output").map(String::as_str) == Some("json"),
            image: None,
            no_tree,
            no_wait: m.get_flag("no-wait"),
        })
    }

    /// `sylphx build image`: the command is `sylphx-image-build` on the tree.
    pub fn parse_image(m: &ArgMatches) -> Result<Self, String> {
        let sh = Shared::parse(m, false)?;
        let image = image::Image::parse(m)?;
        // The explicit publication is known here; a managed one is resolved
        // at send_auth, and the run rebuilds the command from it.
        let command = image.argv(&sh.rel, &image.explicit_publish());
        Ok(Self {
            org: m.try_get_one::<String>("org").ok().flatten().cloned(),
            command,
            root: sh.root,
            rel: sh.rel,
            size: sh.size,
            timeout: sh.timeout,
            region: sh.region,
            queue_timeout: sh.queue_timeout,
            artifacts: Vec::new(),
            out: None,
            env: BTreeMap::new(),
            labels: Vec::new(),
            allow_hosts: sh.allow_hosts,
            fresh: false,
            source_history: None,
            no_cache: false,
            dry_run: false,
            quiet: m.get_flag("quiet"),
            json: m.get_one::<String>("output").map(String::as_str) == Some("json"),
            image: Some(image),
            no_tree: false,
            no_wait: false,
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
    /// The command started and the run lost it (the machine ended under
    /// it): it is not run again, and the run exits as a killed process does.
    Lost(String),
}

impl Outcome {
    pub fn code(&self) -> u8 {
        match self {
            // A local shell reports a status the same way: the low byte.
            Outcome::Ran(c) => (*c & 0xff) as u8,
            Outcome::TimedOut => EXIT_TIMEOUT,
            Outcome::Interrupted => EXIT_INTERRUPTED,
            Outcome::Platform { .. } => EXIT_PLATFORM,
            Outcome::Lost(_) => EXIT_LOST,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Outcome::Ran(0) => "succeeded",
            Outcome::Ran(_) => "failed",
            Outcome::TimedOut => "timed_out",
            Outcome::Interrupted => "interrupted",
            Outcome::Platform { .. } => "platform_error",
            Outcome::Lost(_) => "lost",
        }
    }

    fn platform(reason: impl Into<String>, retryable: bool) -> Self {
        Outcome::Platform {
            reason: reason.into(),
            retryable,
        }
    }
}

/// One bounded, control-safe line from public status; unknown admission reasons
/// remain visible without making a new server enum a CLI compatibility break.
fn queue_progress(lease: &sbx::Lease, elapsed: Duration) -> String {
    fn safe(text: &str) -> String {
        text.chars().filter(|c| !c.is_control()).take(240).collect()
    }
    let state = lease
        .status
        .as_ref()
        .and_then(|s| s.state.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("unknown");
    let condition = lease
        .status
        .as_ref()
        .and_then(|s| s.conditions.iter().find(|c| c.r#type == "Admitted"));
    let status = match condition {
        Some(c) => format!("{}: {}", safe(&c.reason), safe(&c.message)),
        None => safe(state),
    };
    format!(
        "waiting for build machine (lease {}): {}; elapsed {}s",
        safe(&lease.name),
        status,
        elapsed.as_secs()
    )
}

/// Where progress, output and events go: raw output and `sylphx:` lines, or
/// NDJSON events on stdout with `-o json`.
struct Out {
    json: bool,
    quiet: bool,
}

impl Out {
    fn progress_line(&self, text: &str) -> Option<String> {
        (!self.json && !self.quiet).then(|| format!("sylphx: {text}"))
    }

    fn progress(&self, text: &str) {
        if let Some(line) = self.progress_line(text) {
            eprintln!("{line}");
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
    /// Actual credentials only: do not redact ordinary cache addresses/logs.
    output_secrets: Vec<String>,
    /// `build image`: where the guest pushes, resolved at `send_auth` (the
    /// managed mint, or the explicit flags). Holds no credential.
    publish: Option<image::Publish>,
}

impl Held {
    fn with<R>(&self, f: impl FnOnce(&mut HeldState) -> R) -> R {
        f(&mut self.0.lock().expect("not poisoned"))
    }
}

/// The counters of the `result` event.
#[derive(Default)]
struct Stats {
    /// The last accepted machine, retained after release for failure receipts.
    lease: Option<String>,
    bytes_up: u64,
    bytes_down: u64,
    artifacts: usize,
    /// Files the build machine took from the build store, not the upload.
    from_store: usize,
    /// `build image`: the script's metadata object.
    image: Option<Value>,
    /// `build image` (managed): the name of the Artifact the image was
    /// registered as.
    artifact: Option<String>,
    /// `build image` (managed): the deployable `repo@digest` reference.
    source_ref: Option<String>,
}

/// `sylphx build run`. `client` is the signed-in client, or why there is none.
pub async fn run(
    client: Result<Client, Failure>,
    key: Option<String>,
    m: &ArgMatches,
) -> Result<(), Failure> {
    let opts = Opts::parse(m).map_err(Failure::Usage)?;
    run_opts(client, key, opts).await
}

/// `sylphx build image`: the same machinery, with `sylphx-image-build` as the
/// command ([`image`]).
pub async fn run_image(
    client: Result<Client, Failure>,
    key: Option<String>,
    m: &ArgMatches,
) -> Result<(), Failure> {
    let mut opts = Opts::parse_image(m).map_err(Failure::Usage)?;
    if let Some(img) = opts.image.as_mut() {
        img.source = image::source_of(&opts.root);
    }
    run_opts(client, key, opts).await
}

async fn run_opts(
    client: Result<Client, Failure>,
    key: Option<String>,
    opts: Opts,
) -> Result<(), Failure> {
    let out = Out {
        json: opts.json,
        quiet: opts.quiet,
    };
    let t0 = Instant::now();
    if opts.image.as_ref().is_some_and(|i| i.source.is_none()) {
        out.progress("the work tree has local changes or no commit: building without SOURCE_DATE_EPOCH and a source commit, so the image is not reproducible");
    }
    if opts.dry_run {
        return dry_run(&opts, &out);
    }
    let client = match client {
        Ok(c) => c,
        Err(f) => {
            let reason = match f {
                Failure::Usage(s) | Failure::Refused(s) => s,
                Failure::Input(e) => format!("{}: {}", e.code, e.detail),
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
    if opts.no_tree {
        let created = Mutex::new(None::<String>);
        let ended = tokio::select! {
            o = remote::run(&client, &opts, &out, &created) => o,
            _ = interrupted() => remote::Ended::Outcome(Outcome::Interrupted),
        };
        // The Build outlives this process unless it is stopped.
        if matches!(ended, remote::Ended::Outcome(Outcome::Interrupted)) {
            let name = created.lock().ok().and_then(|mut c| c.take());
            if let Some(name) = name {
                remote::cancel(&client, &name).await;
            }
        }
        return match ended {
            // `--no-wait` printed the Build and is done.
            remote::Ended::Created => Ok(()),
            remote::Ended::Outcome(o) => finish(&out, &o, &Stats::default(), t0),
        };
    }
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
    if !sylphx_build_lease::release(client, name).await {
        eprintln!("sylphx: warning: {name} was not released; its idle timeout ends it");
    }
}

/// The final `-o json` event.
fn result_event(outcome: &Outcome, s: &Stats, ms: u64) -> Value {
    let mut result = json!({
        "type": "result",
        "exit_code": outcome.code(),
        "outcome": outcome.name(),
        "retryable": matches!(outcome, Outcome::Platform { retryable: true, .. } | Outcome::Lost(_)),
        "duration_ms": ms,
        "workspace": "cold",
        "bytes_up": s.bytes_up,
        "bytes_down": s.bytes_down,
        "from_store": s.from_store,
    });
    if let Some(lease) = &s.lease {
        result["lease"] = json!(lease);
    }
    if let Outcome::Platform { reason, .. } | Outcome::Lost(reason) = outcome {
        result["error"] = json!(reason);
    }
    if let Some(image) = &s.image {
        result["image"] = image.clone();
    }
    if let Some(artifact) = &s.artifact {
        result["artifact"] = json!(artifact);
    }
    if let Some(source_ref) = &s.source_ref {
        result["source_ref"] = json!(source_ref);
    }
    result
}

/// The `result` event and summary line, and the exit status.
fn finish(out: &Out, outcome: &Outcome, s: &Stats, t0: Instant) -> Result<(), Failure> {
    let ms = t0.elapsed().as_millis() as u64;
    if let Outcome::Platform { reason, retryable } = outcome {
        eprintln!(
            "sylphx: error: {}{}",
            with_lease(reason, s.lease.as_deref()),
            if *retryable { " [retryable]" } else { "" }
        );
    }
    if let Outcome::Lost(reason) = outcome {
        eprintln!(
            "sylphx: error: {} after the command started; it was not run again [retryable]",
            with_lease(reason, s.lease.as_deref())
        );
    }
    out.event(result_event(outcome, s, ms));
    let what = match outcome {
        Outcome::Ran(c) => format!("exit {c}"),
        Outcome::TimedOut => "timed out".into(),
        Outcome::Interrupted => "interrupted".into(),
        Outcome::Platform { .. } => "platform failure".into(),
        Outcome::Lost(_) => "lost the machine".into(),
    };
    let arts = match s.artifacts {
        0 => String::new(),
        1 => "; 1 artifact".into(),
        n => format!("; {n} artifacts"),
    };
    out.progress(&format!(
        "{} in {:.1} s{arts}",
        with_lease(&what, s.lease.as_deref()),
        ms as f64 / 1000.0
    ));
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
            "{} files, {} to send to a cold lease-owned workspace",
            plan.upload.len(),
            human(bytes)
        );
    }
    Ok(())
}

struct StatCache {
    cache: sync::StatCache,
}

impl StatCache {
    fn open(root: &Path) -> Self {
        Self {
            cache: sync::StatCache::load(root),
        }
    }

    fn save(&self) {
        let _ = self.cache.save();
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
    let parent = match parent_env(client, None, o.org.as_deref()).await {
        Ok(p) => p,
        Err(e) => return Outcome::platform(e, false),
    };
    // The tree is hashed once and reused on a fresh workspace after a retry.
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
    // Disabled/advanced source routing retains the pre-lease local mirrors.
    // Otherwise only sources without a public source grant need a local pack.
    let git_deps = if o.image.is_some() {
        Vec::new()
    } else if o.no_cache || build_source::overridden(&o.env) || key.is_none() {
        match prepare_git_deps(&o.root, &tree, &o.rel) {
            Ok(d) => d.into_iter().map(|(d, p)| (d, Some(p))).collect(),
            Err(e) => return Outcome::platform(e, false),
        }
    } else {
        match git_deps::locked(&o.root, &tree, &o.rel) {
            Ok(d) => d.into_iter().map(|d| (d, None)).collect(),
            Err(e) => return Outcome::platform(format!("reading the lock files: {e}"), false),
        }
    };
    // Bundling can take longer than a machine's idle window. Do it once,
    // while interruptible and before any lease, then borrow it on each guest.
    let source_history = match o.source_history.as_deref() {
        Some(base) => match history::prepare(&o.root, base).await {
            Ok(history) => Some(history),
            Err(e) => return Outcome::platform(e, false),
        },
        None => None,
    };
    let mut run = Run {
        client,
        o,
        out,
        held,
        stats,
        parent,
        tree,
        git_deps,
        source_history,
        key,
        cache_url: build_cache::base_url(),
        cache_warned: AtomicBool::new(false),
        source_warned: AtomicBool::new(false),
        accepted_region: None,
    };
    let mut retries = Retries::default();
    loop {
        match retries.next(run.attempt().await) {
            Step::Done(outcome) => return outcome,
            Step::Again { reason, backoff } => {
                out.progress(&format!("{reason}; retrying once"));
                out.event(json!({"type": "queued", "retry": true, "reason": reason, "labels": {}}));
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// Waited before the one retry, so a gateway that dropped a stream is not hit
/// again in the same instant.
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// The one retry a run gets before its command starts, whatever lost it: a
/// machine preempted or lost, or a process stream that broke.
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

/// Why [`Run::exec`] lost the command, and whether it had started: only a
/// command that never started may be run again.
enum ExecFault {
    Before(Fault),
    Setup(Fault),
    Launched(Fault),
}

impl ExecFault {
    fn before(fault: Fault) -> Self {
        Self::Before(fault)
    }

    fn after(launched: bool, fault: Fault) -> Self {
        if launched {
            Self::Launched(fault)
        } else {
            Self::Before(fault)
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
    tree: sync::Tree,
    /// Locked dependencies; only ungranted sources require a local tarball.
    git_deps: Vec<(git_deps::Dep, Option<PathBuf>)>,
    /// HEAD and the requested ref, bundled once before acquiring any lease.
    source_history: Option<(String, Vec<u8>)>,
    /// The caller's key, for minting the cache token.
    key: Option<&'a str>,
    cache_url: String,
    /// One warning per run: the build cache unavailable, or the build store
    /// not taking the sync.
    cache_warned: AtomicBool,
    /// Source authorization diagnostics must survive a compiler-cache warning.
    source_warned: AtomicBool,
    /// The first accepted lease's canonical region, even before READY.
    /// `None` is unselected; `Some("")` is the legacy home-Cell spelling.
    accepted_region: Option<String>,
}

/// A ready machine with its workspace.
struct Machine {
    lease: sbx::Lease,
    cache: BTreeMap<String, String>,
    store: Option<store::Store>,
    guest: Option<Guest>,
}

/// The managed publication the platform minted for one image build: where
/// the guest pushes, and the Docker config that authenticates it. It holds a
/// credential, so it has no `Debug` output and never touches this machine's
/// disk.
struct Minted {
    reference: String,
    auth: Vec<u8>,
}

const SOURCE_AUTH_DIR: &str = "/home/user/.sylphx-source";
const SOURCE_AUTH_WRITE: &str = r#"set -eu
umask 077
mkdir -p "$1"
chmod 700 "$1"
tmp=$(mktemp "$2.XXXXXX")
trap 'rm -f -- "$tmp"' EXIT
printf '%s' "$SYLPHX_SOURCE_HEADERS" > "$tmp"
unset SYLPHX_SOURCE_HEADERS
chmod 600 "$tmp"
mv -f -- "$tmp" "$2"
"#;
const SOURCE_AUTH_PATH: &str = "/home/user/.sylphx-source/gitconfig";

struct Sources {
    env: BTreeMap<String, String>,
    repositories: BTreeSet<String>,
    network: build_cache::Network,
    expires_at: u64,
}

impl Sources {
    fn expiring(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_secs())
            .unwrap_or(0);
        !self.repositories.is_empty() && self.expires_at.saturating_sub(now) <= 120
    }
}

impl Run<'_> {
    /// The accepted Cell, otherwise the explicit region or automatic placement.
    fn region(&self) -> &str {
        match self.accepted_region.as_deref() {
            // An empty CreateLease region would select automatically again.
            Some("") => "fsn1",
            Some(region) => region,
            None => self.o.region.as_deref().unwrap_or(""),
        }
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
        let warned_before =
            self.cache_warned.load(Ordering::Relaxed) || self.source_warned.load(Ordering::Relaxed);
        let mut m = match self.acquire().await {
            Ok(m) => m,
            Err(a) => return a,
        };
        let name = m.lease.name.clone();
        let mut guest = match m.guest.take() {
            Some(g) => g,
            None => match self.guest(&m.lease).await {
                Ok(g) => g,
                Err(e) => return self.lost_or(&name, e).await,
            },
        };
        let mut minted = Instant::now();
        // Keep request phases ordered on every attempt: a failed workspace
        // upload must be classified before dependency uploads start, and the
        // next accepted lease must mint its own source grants.
        let mut sources = match self.source(&m.lease).await {
            Ok(source) => source,
            Err(e) => return Attempt::Done(Outcome::platform(e, false)),
        };
        match self.sync(&guest, m.store.as_ref()).await {
            Ok(()) => {}
            Err(e) if workspace_full(e.text()) => return self.out_of_space(&m).await,
            Err(e) => return self.lost_or(&name, e).await,
        }
        let (mut git_env, mut setup) = match self.send_git_deps(&guest, &sources.repositories).await
        {
            Ok(e) => e,
            Err(e) if out_of_space(e.text()) => return self.out_of_space(&m).await,
            Err(e) => return self.lost_or(&name, e).await,
        };
        build_source::merge_env(&mut git_env, std::mem::take(&mut sources.env));
        match self.send_source_history(&guest).await {
            Ok(script) => setup.push_str(&script),
            Err(e) => return self.lost_or(&name, e).await,
        }
        let shape = format!("build-{}", self.o.size);
        let warned = !warned_before
            && (self.cache_warned.load(Ordering::Relaxed)
                || self.source_warned.load(Ordering::Relaxed));
        // A warning takes the place of this progress line.
        if !warned {
            self.out.progress(&format!(
                "running on {shape}, cold lease-owned workspace{}",
                self.where_()
            ));
        }
        if let Err(e) = self.send_auth(&guest).await {
            return self.lost_or(&name, e).await;
        }
        if self
            .held
            .with(|h| h.publish.as_ref().is_some_and(|p| p.auth))
        {
            setup.push_str(&shell_call(&["chmod", "600", &image::auth_path()]));
        }
        if !sources.repositories.is_empty() {
            let bytes = match build_source::take_headers(&mut git_env, SOURCE_AUTH_PATH) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return Attempt::Done(Outcome::platform(
                        "source session returned invalid Git settings",
                        false,
                    ))
                }
            };
            if let Err(error) = self.write_source_headers(&guest, &bytes).await {
                return self.lost_or(&name, error).await;
            }
        }
        sources.env = git_env;
        let has_sources = !sources.repositories.is_empty();
        let ran = self
            .exec(&mut guest, &mut minted, &name, m.cache, sources, &setup)
            .await;
        // Read the registry with this build's credential before removing it.
        // Keep the result until cleanup finishes, including on a refusal.
        let image_result = if self.o.image.is_some() && matches!(&ran, Ok(Some(0))) {
            self.collect_image(&guest, &name).await
        } else {
            Ok(())
        };
        self.drop_auth(&guest).await;
        if has_sources {
            let none = BTreeMap::new();
            let _ = guest
                .run(
                    USER,
                    &argv(&[
                        "/bin/sh",
                        "-c",
                        "rm -rf -- \"$1\"",
                        "source-auth-cleanup",
                        SOURCE_AUTH_DIR,
                    ]),
                    &none,
                )
                .await;
        }
        let code = match ran {
            Ok(Some(c)) => c,
            Ok(None) => return Attempt::Done(Outcome::TimedOut),
            Err(ExecFault::Before(fault)) => return self.lost_or(&name, fault).await,
            Err(ExecFault::Setup(fault)) => {
                return Attempt::Done(Outcome::platform(fault.text().to_string(), true))
            }
            Err(ExecFault::Launched(fault)) => return self.lost_after_start(&name, fault).await,
        };
        if let Err(o) = image_result {
            return Attempt::Done(o);
        }
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

    /// Mint through the public gateway using this caller only. Missing public
    /// grants retain the caller's existing synced dependency mirrors.
    async fn source(&self, lease: &sbx::Lease) -> Result<Sources, String> {
        let mut sources = Sources {
            env: BTreeMap::new(),
            repositories: BTreeSet::new(),
            network: build_cache::Network::Public,
            expires_at: u64::MAX,
        };
        if self.o.no_cache || build_source::overridden(&self.o.env) {
            return Ok(sources);
        }
        let (Some(key), Some(project)) = (self.key, build_cache::project_id(&self.parent)) else {
            return Ok(sources);
        };
        let network =
            build_cache::run_request(project.clone(), self.o.timeout, Duration::ZERO, lease)
                .network;
        sources.network = network;
        let mut repositories: BTreeSet<String> = self
            .git_deps
            .iter()
            .filter_map(|(dep, _)| build_source::repository(&dep.url))
            .collect();
        if let Some(origin) = build_source::origin(&self.o.root) {
            repositories.insert(origin);
        }
        let minted = pipeline(repositories, |repository| {
            let project = &project;
            async move {
                let minted =
                    build_source::mint(&self.cache_url, key, project, &repository, network, None)
                        .await;
                if let Ok(minted) = &minted {
                    self.remember_source(minted);
                }
                (repository, minted)
            }
        })
        .await;
        for (repository, minted) in minted {
            match minted {
                Ok(minted) => {
                    sources.expires_at = sources.expires_at.min(minted.expires_at.unwrap_or(0));
                    build_source::merge_env(&mut sources.env, minted.env);
                    sources.repositories.insert(repository);
                }
                // A missing grant applies only to this repository, not to
                // sessions already granted for the build's other sources.
                Err(build_cache::MintError::Status {
                    status: 403,
                    code: Some(code),
                    ..
                }) if code == "SOURCE_GRANT_MISSING" => {}
                Err(error)
                    if matches!(
                        &error,
                        build_cache::MintError::TimedOut
                            | build_cache::MintError::Unreachable
                            | build_cache::MintError::Status {
                                status: 401 | 403 | 404 | 500..=599,
                                ..
                            }
                    ) =>
                {
                    if !self.source_warned.swap(true, Ordering::Relaxed) {
                        eprintln!("sylphx: warning: source cache unavailable ({}); using the caller’s existing source inputs", error.diagnostic());
                    }
                    sources.env.clear();
                    sources.repositories.clear();
                    sources.expires_at = u64::MAX;
                    return Ok(sources);
                }
                Err(e) => return Err(format!("source session unavailable ({})", e.diagnostic())),
            }
        }
        Ok(sources)
    }

    fn remember_source(&self, minted: &build_cache::Minted) {
        self.held.with(|h| {
            h.secrets.extend(minted.secrets());
            h.output_secrets.extend(minted.token.iter().cloned());
            h.output_secrets.extend(
                minted
                    .env
                    .values()
                    .find(|v| v.starts_with("Authorization:"))
                    .cloned(),
            );
        });
    }

    async fn write_source_headers(&self, guest: &Guest, bytes: &[u8]) -> Result<(), Fault> {
        // Carry credentials only in the process environment, never argv or
        // output. One action creates a private temporary file and publishes it
        // atomically, including on renewal.
        let env = BTreeMap::from([(
            "SYLPHX_SOURCE_HEADERS".to_string(),
            String::from_utf8(bytes.to_vec())
                .map_err(|_| Fault::Other("invalid source credentials".into()))?,
        )]);
        let publish = argv(&[
            "/bin/sh",
            "-c",
            SOURCE_AUTH_WRITE,
            "source-auth",
            SOURCE_AUTH_DIR,
            SOURCE_AUTH_PATH,
        ]);
        let (code, _, _) = guest.run(USER, &publish, &env).await?;
        if code != 0 {
            return Err(Fault::Other(
                "publishing private source credentials failed".into(),
            ));
        }
        Ok(())
    }

    async fn renew_sources(&self, guest: &Guest, sources: &mut Sources) -> Result<(), Fault> {
        let key = self
            .key
            .ok_or_else(|| Fault::Other("source renewal has no caller identity".into()))?;
        let project = build_cache::project_id(&self.parent)
            .ok_or_else(|| Fault::Other("source renewal has no project".into()))?;
        let mut env = BTreeMap::new();
        let mut expires_at = u64::MAX;
        for repository in &sources.repositories {
            let minted = build_source::mint(
                &self.cache_url,
                key,
                &project,
                repository,
                sources.network,
                None,
            )
            .await
            .map_err(|error| {
                Fault::Other(format!(
                    "source session renewal failed ({})",
                    error.diagnostic()
                ))
            })?;
            self.remember_source(&minted);
            expires_at = expires_at.min(
                minted
                    .expires_at
                    .ok_or_else(|| Fault::Other("source renewal has no expiry".into()))?,
            );
            build_source::merge_env(&mut env, minted.env);
        }
        let bytes = build_source::take_headers(&mut env, SOURCE_AUTH_PATH)
            .map_err(|_| Fault::Other("source renewal returned invalid Git settings".into()))?;
        self.write_source_headers(guest, &bytes).await?;
        sources.expires_at = expires_at;
        Ok(())
    }

    /// The cache environment for the command, minted before the sync (its
    /// lifetime is the run's timeout plus slack, which covers the sync). Empty
    /// with `--no-cache`, and when no token
    /// can be had (a cache that is missing only slows the build). The build
    /// store, when the token reaches it, takes the sync's files.
    async fn cache(
        &self,
        lease: &sbx::Lease,
        queue_timeout: Duration,
    ) -> (BTreeMap<String, String>, Option<store::Store>) {
        if self.o.no_cache {
            return (BTreeMap::new(), None);
        }
        let minted = match (self.key, build_cache::project_id(&self.parent)) {
            (Some(key), Some(project)) => {
                let req = build_cache::run_request(project, self.o.timeout, queue_timeout, lease);
                build_cache::mint(&self.cache_url, key, &req).await
            }
            (None, _) => Err(build_cache::MintError::NoProject("not signed in".into())),
            (_, None) => Err(build_cache::MintError::NoProject(
                "the environment names no project".into(),
            )),
        };
        match minted {
            Ok(m) => {
                self.held.with(|h| {
                    h.secrets.extend(m.secrets());
                    h.output_secrets.extend(m.token.iter().cloned());
                });
                // The gateway as the build machine reaches it.
                let guest_base = m.env.get("TURBO_API").cloned().unwrap_or_default();
                let st = m.token.as_deref().and_then(|t| {
                    store::Store::new(
                        &self.cache_url,
                        &guest_base,
                        m.cache.as_deref().unwrap_or(""),
                        t,
                    )
                });
                (m.env, st)
            }
            Err(e) => {
                let first = !self.cache_warned.swap(true, Ordering::Relaxed);
                if first {
                    eprintln!(
                        "sylphx: warning: build cache unavailable ({}); building without it",
                        e.short()
                    );
                }
                (BTreeMap::new(), None)
            }
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

    /// Retry once before the command starts on another fresh lease-owned
    /// workspace. A second failure is 125, as any loss before the start.
    async fn out_of_space(&mut self, m: &Machine) -> Attempt {
        let name = &m.lease.name;
        self.held.with(|h| {
            h.proc = None;
            h.lease = None;
        });
        release(self.client, name).await;
        Attempt::Lost("the workspace ran out of space".into())
    }

    /// The command started and the run lost it: the machine ended, or the
    /// command cannot be followed. It is never run again (it may have done
    /// part of its work); the run ends as a killed process would.
    async fn lost_after_start(&self, name: &str, f: Fault) -> Attempt {
        let mut get = sbx::GetLeaseRequest::default();
        get.name = name.to_string();
        let lease = self.client.sandboxes().leases().get(get).await.ok();
        let reason = lease
            .as_ref()
            .and_then(|l| l.status.as_ref())
            .and_then(|s| s.end_reason.as_ref())
            .map(|r| format!("the build machine ended: {}", r.as_str()))
            .unwrap_or_else(|| f.text().to_string());
        self.held.with(|h| h.proc = None);
        Attempt::Done(Outcome::Lost(reason))
    }

    /// Lease first; the workload creates its cold workspace only after
    /// admission. Explicit regions pin placement, never a workspace Volume.
    ///
    /// The queue budget starts here, before the lease is created, and is the
    /// SDK client's shared deadline: the SDK's own retries of a retryable
    /// answer (a stale access snapshot answering 503, say) are spent inside
    /// it, and when they end — at its retry limit or at the deadline — the
    /// server's own answer is returned, message intact. No new logical call
    /// is made here: a fresh create would carry a fresh idempotency key and
    /// could duplicate the machine.
    async fn acquire(&mut self) -> Result<Machine, Attempt> {
        let deadline = Instant::now() + self.o.queue_timeout;
        let bounded = self.client.with_deadline(deadline);
        let lease = self.lease(&bounded).await.map_err(Attempt::Done)?;
        // The lease's own labels, as the platform holds them: the run's
        // `worktree`/`source` and the caller's `--label` beside `purpose`.
        let labels = lease
            .meta
            .as_ref()
            .map(|m| m.labels.clone())
            .unwrap_or_default();
        self.out.event(
            json!({"type": "queued", "lease": lease.name, "size": self.o.size, "region": lease.spec.as_ref().map(|s| s.region.as_str()).unwrap_or(self.region()), "volume": false, "labels": labels}),
        );
        // Cache credentials depend on accepted placement, not readiness.
        // Mint while the machine is admitted/booted, through the public cache API.
        let cache_started = Instant::now();
        let (ready, (mut cache, mut store), guest) = tokio::join!(
            self.ready(self.client, lease.clone(), deadline),
            self.cache(&lease, self.o.queue_timeout),
            async {
                // A queued lease cannot mint a token. A warm READY answer can
                // mint in parallel with the cache instead of serializing it.
                if state(&lease) == sbx::LeaseState::Ready {
                    self.guest(&lease).await.map(Some)
                } else {
                    Ok(None)
                }
            },
        );
        let lease = ready?;
        // Only a capped token can lose its setup/command lifetime in admission.
        // Refresh that exceptional path, never serialize normal READY setup.
        if !cache.is_empty()
            && build_cache::run_needs_refresh(
                self.o.timeout,
                self.o.queue_timeout,
                cache_started.elapsed(),
            )
        {
            (cache, store) = self.cache(&lease, Duration::ZERO).await;
        }
        let guest = match guest {
            Ok(g) => g,
            Err(e) => return Err(self.lost_or(&lease.name, e).await),
        };
        Ok(Machine {
            lease,
            cache,
            store,
            guest,
        })
    }

    /// Create a Build lease with no pre-created or attached workspace Volume.
    async fn lease(&mut self, client: &Client) -> Result<sbx::Lease, Outcome> {
        let domains = lease_domains(self.o);
        // The run's own worktree and repository name every reservation it
        // holds, so a held lease reads back as whose it is; the caller's
        // `--label` values apply on top. A `--no-tree` run has no work tree.
        let labels = if self.o.no_tree {
            self.o.labels.clone()
        } else {
            labels::merge(labels::defaults(&self.o.root), self.o.labels.clone())
        };
        let req = lease_request(
            &self.parent,
            &LeaseParams {
                size: &self.o.size,
                region: self.region(),
                timeout: self.o.timeout,
                domains: &domains,
                labels: &labels,
            },
        );
        let lease = client
            .sandboxes()
            .leases()
            .create(req)
            .await
            .map_err(|e| match &e {
                // The SDK observes this accepted request with the same key
                // until its result is available. A pending answer escapes
                // only when another poll would exceed the queue budget.
                sylphx::Error::Api { code, status: 409, retryable: true, .. }
                    if code.as_str() == "IDEMPOTENCY_IN_PROGRESS" => Outcome::platform(
                        format!(
                            "no build machine{} within --queue-timeout {}s (the accepted request is still running)",
                            self.where_(), self.o.queue_timeout.as_secs()
                        ),
                        true,
                    ),
                _ => api("creating the build machine", &e),
            })?;
        if self.accepted_region.is_none() {
            self.accepted_region = lease.spec.as_ref().map(|s| s.region.clone());
        }
        self.held.with(|h| h.lease = Some(lease.name.clone()));
        self.add(|stats| stats.lease = Some(lease.name.clone()));
        self.out
            .progress(&with_lease("accepted build machine", Some(&lease.name)));
        Ok(lease)
    }

    /// Wait for admission and READY within the caller's queue budget.
    async fn ready(
        &self,
        client: &Client,
        lease: sbx::Lease,
        deadline: Instant,
    ) -> Result<sbx::Lease, Attempt> {
        // The create already returned this resource. A failure before that
        // has no name; say so instead of printing an empty one.
        let label = lease_label(&lease.name);
        match sylphx_build_lease::machine::wait_ready_with_progress(
            client,
            lease.clone(),
            None,
            deadline,
            |lease, elapsed| self.out.progress(&queue_progress(lease, elapsed)),
        )
        .await
        {
            Ready::Ready(l) => Ok(*l),
            // No workspace node pin is requested by this caller.
            Ready::PinExpired => unreachable!("volume-free Build leases have no node pin"),
            Ready::QueueTimeout => {
                // The GET budget expired; the original cleanup client released
                // the lease. Only forget it here.
                self.held.with(|h| h.lease = None);
                Err(Attempt::Done(Outcome::platform(
                    format!(
                        "no build machine{} within --queue-timeout {}s ({label})",
                        self.where_(),
                        self.o.queue_timeout.as_secs()
                    ),
                    true,
                )))
            }
            Ready::Refused { why, retryable } => {
                self.held.with(|h| h.lease = None);
                Err(Attempt::Done(Outcome::platform(
                    format!("the build machine was refused: {why} ({label})"),
                    retryable,
                )))
            }
            Ready::Ended { reason, retryable } => {
                self.held.with(|h| h.lease = None);
                Err(Attempt::Done(Outcome::platform(
                    format!("the build machine ended before it was ready: {reason} ({label})"),
                    retryable,
                )))
            }
            Ready::Lost(reason) => {
                self.held.with(|h| h.lease = None);
                Err(Attempt::Lost(reason))
            }
            Ready::Api(e) => Err(Attempt::Done(api("reading the build machine", &e))),
        }
    }

    async fn token(&self, lease: &str) -> Result<String, Fault> {
        sylphx_build_lease::mint_token(self.client, lease).await
    }

    async fn guest(&self, lease: &sbx::Lease) -> Result<Guest, Fault> {
        sylphx_build_lease::guest(self.client, lease).await
    }

    /// Materialize a fresh workspace from in-cell packs. Direct tarballs
    /// remain the cache-failure and --no-cache path.
    async fn sync(&self, g: &Guest, store: Option<&store::Store>) -> Result<(), Fault> {
        let t = Instant::now();
        let none = BTreeMap::new();
        // The admitted workload owns a newly created workspace: no previous
        // lease's manifest or build state is available, even with --region.
        let plan = sync::plan(&self.tree, None);
        let bytes = plan.upload_bytes(&self.tree);
        let mut up = 0u64;
        let filled = match store {
            Some(st) if !plan.upload.is_empty() => {
                let mut known = store::Known::load(
                    Some(sync::source_cache(&self.o.root).join("tree-packs")),
                    &st.instance_key(),
                );
                let f = store::fill(
                    g,
                    st,
                    &mut known,
                    store::Local {
                        root: &self.o.root,
                        tree: &self.tree,
                    },
                    &plan,
                    WS,
                    USER,
                )
                .await?;
                known.save();
                up += f.sent;
                if let Some(w) = &f.warning {
                    if !self.cache_warned.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "sylphx: warning: build store: {w}; sending those files directly"
                        );
                    }
                }
                f.paths
            }
            _ => Default::default(),
        };
        let direct: Vec<String> = plan
            .upload
            .iter()
            .filter(|p| !filled.contains(*p))
            .cloned()
            .collect();
        let tars = sync::tarballs(&self.o.root, &direct, &self.tree).map_err(Fault::Other)?;
        let lists = sync::lists(&self.tree, &plan, &filled);
        let s = format!("{WS}/.sylphx");
        let mut uploads: Vec<(String, &[u8])> = tars
            .iter()
            .enumerate()
            .map(|(i, t)| (format!("{s}/in-{i:04}.tar.gz"), t.as_slice()))
            .collect();
        for (name, body) in [
            ("drop", lists.drop.as_bytes()),
            ("add", lists.add.as_bytes()),
            ("remove", &lists.remove[..]),
        ] {
            if !body.is_empty() {
                uploads.push((format!("{s}/{name}"), body));
            }
        }
        // Stop at the first fault: further requests could race the lease-loss
        // check and must not outlive the failed workspace setup phase.
        for (path, body) in uploads {
            g.upload(&path, USER, body).await?;
            up += body.len() as u64;
        }
        self.add(|st| {
            st.bytes_up += up;
            st.from_store = filled.len();
        });
        let full = match (self.o.fresh, plan.full) {
            (true, _) => "2",
            (false, true) => "1",
            (false, false) => "0",
        };
        // Credentials are uploaded only into a private directory. Prepare it
        // in the existing sync-apply action, never as another round trip.
        let apply = if self.o.image.is_some() {
            format!(
                "set -eu\numask 077\nmkdir -m 700 {}\n{}",
                shell_word(&image::private_dir()),
                sync::APPLY
            )
        } else {
            sync::APPLY.to_string()
        };
        let (code, _, err) = g
            .run(
                USER,
                &argv(&[
                    "/bin/sh",
                    "-c",
                    &apply,
                    "apply",
                    WS,
                    full,
                    if filled.is_empty() { "0" } else { "1" },
                ]),
                &none,
            )
            .await?;
        if code != 0 {
            return Err(Fault::Other(format!(
                "{APPLY_FAILED}: {}",
                String::from_utf8_lossy(&err).trim()
            )));
        }
        let secs = t.elapsed().as_secs_f64();
        self.out.progress(&format!(
            "synced {} files, {}, in {secs:.1} s{}{}",
            plan.upload.len(),
            human(up),
            if filled.is_empty() {
                String::new()
            } else {
                format!("; {} from the build store", filled.len())
            },
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
            "from_store": filled.len(),
            "duration_ms": t.elapsed().as_millis() as u64,
            "workspace": "cold",
            "volume": false,
        }));
        Ok(())
    }

    /// Upload only ungranted locked dependencies, then install them in the
    /// existing guest start action. Cold leases need no remote HAVE query.
    async fn send_git_deps(
        &self,
        g: &Guest,
        covered: &BTreeSet<String>,
    ) -> Result<(BTreeMap<String, String>, String), Fault> {
        let deps: Vec<_> = self
            .git_deps
            .iter()
            .filter(|(d, _)| {
                !build_source::repository(&d.url).is_some_and(|repo| covered.contains(&repo))
            })
            .collect();
        let mut up = 0u64;
        let mut args = vec!["/bin/sh", "-c", git_deps::APPLY, "git-deps-apply", WS];
        let names: Vec<String> = deps.iter().map(|(d, _)| d.name()).collect();
        for result in pipeline(deps.iter().zip(&names), |((d, tgz), n)| async move {
            let d = d.clone();
            let tgz = tgz.clone();
            let cache = sync::source_cache(&self.o.root).join("git-deps");
            // Git/tar and filesystem reads are blocking; keep them off the
            // async executor so independent grants/uploads keep progressing.
            let bytes = tokio::task::spawn_blocking(move || {
                let tgz = match tgz {
                    Some(path) => path,
                    None => {
                        std::fs::create_dir_all(&cache).map_err(|e| {
                            Fault::Other(format!("preparing git dependencies: {e}"))
                        })?;
                        git_deps::pack(&cache, &d).map_err(Fault::Other)?
                    }
                };
                std::fs::read(&tgz)
                    .map_err(|e| Fault::Other(format!("reading {}: {e}", tgz.display())))
            })
            .await
            .map_err(|_| Fault::Other("packing git dependencies failed".into()))??;
            g.upload(&format!("{WS}/{}/in/{n}.tgz", git_deps::DIR), USER, &bytes)
                .await?;
            Ok::<_, Fault>(bytes.len() as u64)
        })
        .await
        {
            up += result?;
        }
        args.extend(names.iter().map(String::as_str));
        self.add(|s| s.bytes_up += up);
        if !names.is_empty() {
            self.out.event(json!({"type": "git_deps", "deps": names.len(), "sent": names.len(), "bytes_up": up}));
        }
        let deps: Vec<git_deps::Dep> = deps.iter().map(|(d, _)| d.clone()).collect();
        Ok((
            git_deps::env(&deps, WS),
            if names.is_empty() {
                String::new()
            } else {
                shell_call(&args)
            },
        ))
    }

    async fn send_source_history(&self, g: &Guest) -> Result<String, Fault> {
        let Some((reference, bundle)) = &self.source_history else {
            return Ok(String::new());
        };
        g.upload(&format!("{WS}/.sylphx/source.bundle"), USER, bundle)
            .await?;
        let script = shell_call(&[
            "/bin/sh",
            "-c",
            history::APPLY,
            "source-history",
            WS,
            reference,
        ]);
        self.add(|s| s.bytes_up += bundle.len() as u64);
        self.out.event(
            json!({"type": "source_history", "reference": reference, "bytes_up": bundle.len()}),
        );
        Ok(script)
    }

    /// Runs the command, streaming its output; `None` when `--timeout` hit.
    /// A broken stream is reattached to the running command; the error says
    /// whether the command had started when the run lost it.
    async fn exec(
        &self,
        g: &mut Guest,
        minted: &mut Instant,
        lease: &str,
        cache: BTreeMap<String, String>,
        mut sources: Sources,
        setup: &str,
    ) -> Result<Option<i32>, ExecFault> {
        let mut args = vec![
            "/bin/sh".to_string(),
            "-c".into(),
            start_script(setup, self.o.image.is_none()),
            "sylphx-run".into(),
            WS.into(),
            self.rel().to_string(),
            "1".into(),
        ];
        // An image build's command carries the publication resolved at
        // send_auth (the managed reference is only known then); everything
        // else runs the command the caller gave.
        let command = self.held.with(|h| match (&self.o.image, &h.publish) {
            (Some(img), Some(publish)) => img.argv(self.rel(), publish),
            _ => self.o.command.clone(),
        });
        args.extend(command);
        // The command's own `--env` wins over the cache's and ours.
        let cached = !cache.is_empty();
        let mut env = cache;
        if let Some(img) = &self.o.image {
            // An image build gets the cache's mirror addresses, never its
            // write tokens: no step of an image build needs sccache or Turbo.
            env.retain(|k, _| image::forwarded(k));
            env.entry("SYLPHX_OCI_MIRROR".into())
                .or_insert_with(image::default_oci_mirror);
            env.extend(img.env());
        }
        if let Some(sha) = sync::head(&self.o.root) {
            env.insert("SYLPHX_BUILD_GIT_HEAD".into(), sha);
        }
        env.extend(std::mem::take(&mut sources.env));
        env.extend(self.o.env.clone());
        if self.o.source_history.is_some() {
            env.insert("SYLPHX_BUILD_SOURCE_HISTORY".into(), "1".into());
        } else {
            env.remove("SYLPHX_BUILD_SOURCE_HISTORY");
        }
        let action_state = format!(
            "/tmp/sylphx-action-{:016x}",
            sylphx_build_lease::guest::rand_u64()
        );
        env.insert("SYLPHX_BUILD_ACTION_STATE".into(), action_state.clone());
        let mut p = g
            .start(USER, &args, "", &env)
            .await
            .map_err(ExecFault::before)?;
        let mut startup = Startup::default();
        let mut deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        let mut tick = tokio::time::interval(KEEPALIVE);
        tick.tick().await;
        let mut registry_minted = Instant::now();
        let mut reattached = false;
        let mut output = redact::Output::new(self.out, self.held);
        let result = loop {
            tokio::select! {
                ev = p.next() => match ev {
                    Ok(Some(Event::Start(pid))) => {
                        let g2 = g.clone();
                        self.held.with(|h| h.proc = Some((g2, pid)));
                    }
                    Ok(Some(Event::Stdout(b))) => output.stdout(&b),
                    Ok(Some(Event::Stderr(b))) => {
                        let before = startup.started();
                        let bytes = startup.push(&b);
                        if !before && startup.started() {
                            deadline = tokio::time::Instant::now() + self.o.timeout;
                            self.out.event(json!({"type": "running", "lease": lease, "size": self.o.size, "region": self.region(), "workspace": "cold", "volume": false, "cache": cached}));
                        }
                        if !bytes.is_empty() { output.stderr(&bytes); }
                    }
                    Ok(Some(Event::End(c))) => {
                        let pending = startup.finish();
                        if !pending.is_empty() { output.stderr(&pending); }
                        if !startup.started() {
                            return Err(ExecFault::Setup(Fault::Other(format!("preparing the build machine failed (status {c}); see its stderr"))));
                        }
                        // An exit buffered while detached cannot outrun the
                        // original command deadline recovered from the guest.
                        break if tokio::time::Instant::now() >= deadline { None } else { Some(c) };
                    }
                    Ok(Some(Event::Error(m))) => {
                        let f = Fault::Other(format!("the command could not run: {m}"));
                        return Err(ExecFault::after(p.pid.is_some(), f));
                    }
                    Ok(None) | Err(Fault::Stream(_)) => {
                        // A proxy on the way restarted: follow the same
                        // process again rather than running it twice.
                        let broke = Instant::now();
                        if let Ok(t) = self.token(lease).await {
                            g.set_token(&t);
                            *minted = Instant::now();
                        }
                        // `--timeout` still holds while the stream is down.
                        let reattached_by = tokio::time::timeout_at(
                            deadline,
                            g.reattach_within(&mut p, REATTACH_BUDGET),
                        )
                        .await;
                        let Ok(reattach) = reattached_by else {
                            if let Some(pid) = p.pid {
                                let _ = g.signal(pid, "SIGKILL").await;
                            }
                            break None;
                        };
                        match reattach {
                            Ok(Reattach::Attached) => {
                                if !startup.started() {
                                    let elapsed = tokio::time::timeout_at(deadline, g.command_elapsed(&action_state))
                                        .await
                                        .map_err(|_| ExecFault::Setup(Fault::Other("recovering command-start state timed out".into())))?
                                        .map_err(|f| ExecFault::after(true, f))?;
                                    if let Some(elapsed) = elapsed {
                                        startup.recover();
                                        deadline = tokio::time::Instant::now() + self.o.timeout.saturating_sub(elapsed);
                                        self.out.event(json!({"type": "running", "lease": lease, "size": self.o.size, "region": self.region(), "workspace": "cold", "volume": false, "cache": cached}));
                                    }
                                }
                                let gap = broke.elapsed().as_millis() as u64;
                                self.out.event(json!({"type": "reattached", "gap_ms": gap}));
                                if !reattached {
                                    reattached = true;
                                    self.out.progress(&format!(
                                        "warning: the output stream broke; reattached to the running command after {:.1} s (output written meanwhile is not shown)",
                                        gap as f64 / 1000.0
                                    ));
                                }
                            }
                            Ok(Reattach::NotRunning) => {
                                let f = Fault::Stream("the output stream broke and the command is no longer on the machine".into());
                                return Err(ExecFault::after(p.pid.is_some(), f));
                            }
                            Err(f) => return Err(ExecFault::after(p.pid.is_some(), f)),
                        }
                    }
                    Err(f) => return Err(ExecFault::after(p.pid.is_some(), f)),
                },
                _ = tick.tick() => {
                    if minted.elapsed() >= TOKEN_RENEW {
                        if let Ok(t) = self.token(lease).await {
                            g.set_token(&t);
                            *minted = Instant::now();
                        }
                    }
                    if sources.expiring() {
                        if let Err(fault) = self.renew_sources(g, &mut sources).await {
                            if let Some(pid) = p.pid { let _ = g.signal(pid, "SIGKILL").await; }
                            return Err(ExecFault::after(p.pid.is_some(), fault));
                        }
                    }
                    if self.o.image.as_ref().is_some_and(|i| i.managed)
                        && registry_minted.elapsed() >= REGISTRY_TOKEN_RENEW
                    {
                        if let Err(fault) = self.renew_registry_auth(g).await {
                            if let Some(pid) = p.pid {
                                let _ = g.signal(pid, "SIGKILL").await;
                            }
                            return Err(ExecFault::after(p.pid.is_some(), fault));
                        }
                        registry_minted = Instant::now();
                    }
                    let _ = g.touch(WS).await;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if let Some(pid) = p.pid {
                        let _ = g.signal(pid, "SIGKILL").await;
                    }
                    if !startup.started() {
                        return Err(ExecFault::Setup(Fault::Other("preparing the build machine took longer than 900s".into())));
                    }
                    break None;
                }
            }
        };
        self.add(|s| s.bytes_down += p.bytes);
        self.held.with(|h| h.proc = None);
        Ok(result)
    }

    /// `build image`: resolves the publication (minting the managed one),
    /// puts its credentials on the machine as a private file (0600 in a 0700
    /// directory), and adds the file's values to the secrets scrubbed from
    /// any text that leaves this process. The file never exists on this
    /// machine, and the credentials never enter an argument, an environment
    /// variable, or a synced file.
    async fn send_auth(&self, g: &Guest) -> Result<(), Fault> {
        let Some(img) = self.o.image.as_ref() else {
            return Ok(());
        };
        let (publish, bytes) = if img.managed {
            let minted = self.mint_upload().await?;
            (
                image::Publish {
                    reference: Some(minted.reference),
                    auth: true,
                    managed: true,
                },
                minted.auth,
            )
        } else {
            let Some(file) = img.registry_auth.as_ref().filter(|f| f.is_file()) else {
                return Err(Fault::Other(
                    "--push needs a readable --registry-auth file".into(),
                ));
            };
            let bytes = std::fs::read(file).map_err(|e| {
                Fault::Other(format!("reading --registry-auth {}: {e}", file.display()))
            })?;
            (img.explicit_publish(), bytes)
        };
        self.held.with(|h| {
            let secrets = image::auth_secrets(&String::from_utf8_lossy(&bytes));
            h.output_secrets.extend(secrets.iter().cloned());
            h.secrets.extend(secrets);
        });
        let path = image::auth_path();
        g.upload(&path, USER, &bytes).await?;
        self.held.with(|h| h.publish = Some(publish));
        Ok(())
    }

    /// Mints this build's managed publication: the upload reference and a
    /// short-lived registry credential, both scoped by the verified grant and
    /// the run's canonical parent — never a repository a caller names. The
    /// registry host is the one the lease's egress was fixed to. An issuer
    /// that is down answers 503: a platform failure the caller may retry.
    async fn mint_upload(&self) -> Result<Minted, Fault> {
        let parent = image::project_parent(&self.parent).ok_or_else(|| {
            Fault::Other(format!(
                "{}: no project to publish the image in",
                self.parent
            ))
        })?;
        let answer = self
            .client
            .invoke(
                "artifacts.artifacts.prepare_upload",
                json!({"parent": parent}),
            )
            .await
            .map_err(|e| Fault::Other(format!("minting the image upload: {}", why(&e))))?;
        let reference = answer
            .get("image_reference")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let auth = answer
            .get("registry_auth")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if reference.is_empty() || auth.is_empty() {
            return Err(Fault::Other(
                "the image upload mint answered no reference".into(),
            ));
        }
        // The lease's egress list was fixed before this call: a reference to
        // another host would be pushed into a blocked route.
        if image::registry_host(reference).as_deref() != Some(image::MANAGED_REGISTRY_HOST) {
            return Err(Fault::Other(format!(
                "the image upload mint answered {reference}: not on {}",
                image::MANAGED_REGISTRY_HOST
            )));
        }
        Ok(Minted {
            reference: reference.to_string(),
            auth: auth.as_bytes().to_vec(),
        })
    }

    /// Replaces the private config atomically. The guest credential helper
    /// reads it on each BuildKit credential lookup, including publication.
    async fn renew_registry_auth(&self, g: &Guest) -> Result<(), Fault> {
        let minted = self.mint_upload().await?;
        let same_repository = self.held.with(|h| {
            h.publish
                .as_ref()
                .and_then(|p| p.reference.as_deref())
                .is_some_and(|reference| {
                    reference.rsplit_once(':').map(|(repository, _)| repository)
                        == minted
                            .reference
                            .rsplit_once(':')
                            .map(|(repository, _)| repository)
                })
        });
        if !same_repository {
            return Err(Fault::Other("the renewed upload changed repository".into()));
        }
        self.held.with(|h| {
            let secrets = image::auth_secrets(&String::from_utf8_lossy(&minted.auth));
            h.output_secrets.extend(secrets.iter().cloned());
            h.secrets.extend(secrets);
        });
        let path = image::auth_path();
        let next = format!("{path}.next");
        g.upload(&next, USER, &minted.auth).await?;
        let (code, _, _) = g
            .run(
                USER,
                &argv(&[
                    "/bin/sh",
                    "-c",
                    "chmod 600 \"$1\" && mv -f \"$1\" \"$2\"",
                    "registry-auth",
                    &next,
                    &path,
                ]),
                &BTreeMap::new(),
            )
            .await?;
        if code != 0 {
            return Err(Fault::Other(
                "renewing the private registry credentials failed".into(),
            ));
        }
        Ok(())
    }

    /// Removes what [`Self::send_auth`] put on the machine; best effort (the
    /// machine may be gone, and the next image build replaces the directory).
    async fn drop_auth(&self, g: &Guest) {
        if self
            .held
            .with(|h| h.publish.as_ref().is_some_and(|p| p.auth))
        {
            let _ = g
                .run(
                    USER,
                    &argv(&["rm", "-rf", &image::private_dir()]),
                    &BTreeMap::new(),
                )
                .await;
        }
    }

    /// `build image`: copies the script's metadata (and the `--oci-out`
    /// archive) back and prints the digest, frontend and process sandbox.
    async fn collect_image(&self, g: &Guest, lease: &str) -> Result<(), Outcome> {
        let fail = |what: &str, f: Fault| Outcome::platform(format!("{what}: {}", f.text()), true);
        let Some(bytes) = g
            .download(&image::metadata_path(), USER)
            .await
            .map_err(|f| fail("copying the image metadata back", f))?
        else {
            return Err(Outcome::platform(
                "the build ended without writing its metadata",
                false,
            ));
        };
        let meta: Value = serde_json::from_slice(&bytes).map_err(|e| {
            Outcome::platform(format!("the image metadata is not JSON: {e}"), false)
        })?;
        self.add(|s| s.bytes_down += bytes.len() as u64);
        let dir = PathBuf::from(".sylphx/out").join(lease.rsplit('/').next().unwrap_or("run"));
        let local = dir.join("image.json");
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&local, &bytes))
            .map_err(|e| Outcome::platform(format!("{}: {e}", local.display()), false))?;
        if let Some(dest) = self.o.image.as_ref().and_then(|i| i.oci_out.as_ref()) {
            let tar = g
                .download(&image::oci_path(), USER)
                .await
                .map_err(|f| fail("copying the image archive back", f))?
                .ok_or_else(|| Outcome::platform("the image archive was not written", false))?;
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(dest, &tar)
                .map_err(|e| Outcome::platform(format!("{}: {e}", dest.display()), false))?;
            self.add(|s| s.bytes_down += tar.len() as u64);
        }
        for line in image::summary(&meta) {
            if self.o.json {
                continue;
            }
            println!("{line}");
        }
        // A managed publication is registered in Sylphx Artifacts, so a
        // Release can name it; the deployable reference is what a caller
        // deploys with.
        if let Some(publish) = self.held.with(|h| h.publish.clone()).filter(|p| p.managed) {
            let (artifact, source_ref) = self.register_image(g, &meta, &publish).await?;
            if !self.o.json {
                println!("source_ref: {source_ref}");
            }
            self.add(|s| {
                s.artifact = Some(artifact);
                s.source_ref = Some(source_ref);
            });
        }
        self.add(|s| s.image = Some(meta));
        Ok(())
    }

    /// Registers the pushed image as an immutable Artifact and answers its
    /// name and the deployable `repo@digest` reference. A digest already
    /// registered is the same image (a digest addresses immutable bytes):
    /// the existing Artifact's name is reported, never a second one and
    /// never a success without it.
    async fn register_image(
        &self,
        g: &Guest,
        meta: &Value,
        publish: &image::Publish,
    ) -> Result<(String, String), Outcome> {
        let digest = image::image_digest(meta)
            .ok_or_else(|| Outcome::platform("the build metadata names no image digest", false))?;
        let reference = publish.reference.as_deref().unwrap_or_default();
        let source_ref = image::source_ref(reference, digest).ok_or_else(|| {
            Outcome::platform(format!("{reference}: not a published digest"), false)
        })?;
        let auth = g
            .download(&image::auth_path(), USER)
            .await
            .map_err(|f| {
                Outcome::platform(format!("reading registry credentials: {}", f.text()), true)
            })?
            .ok_or_else(|| {
                Outcome::platform("the build's registry credentials are missing", false)
            })?;
        let descriptor = image::registry_descriptor(&source_ref, &auth)
            .await
            .map_err(|e| Outcome::platform(e, false))?;
        let parent = image::project_parent(&self.parent).ok_or_else(|| {
            Outcome::platform(
                format!("{}: no project to register the image in", self.parent),
                false,
            )
        })?;
        let name = self
            .register(&parent, digest, descriptor, &source_ref)
            .await?;
        Ok((name, source_ref))
    }

    /// Registers one immutable Artifact for the pushed image and answers its
    /// name. A digest already registered is the same bytes: the Artifact
    /// there is read back and checked, never duplicated and never assumed.
    async fn register(
        &self,
        parent: &str,
        digest: &str,
        descriptor: image::ImageDescriptor,
        expected_reference: &str,
    ) -> Result<String, Outcome> {
        let mut artifact = sylphx::artifacts::Artifact::default();
        artifact.digest = digest.to_string();
        artifact.kind = Some(sylphx::artifacts::ArtifactKind::Image);
        artifact.media_type = descriptor.media_type;
        artifact.size_bytes = descriptor.size_bytes;
        let mut request = sylphx::artifacts::CreateArtifactRequest::default();
        request.parent = parent.to_string();
        request.artifact = Some(artifact);
        request.managed_upload = true;
        let registered = match self.client.artifacts().artifacts().create(request).await {
            Ok(a) => a,
            Err(sylphx::Error::Api {
                code: sylphx::common::ErrorCode::ResourceAlreadyExists,
                ..
            }) => self.existing_image(parent, digest).await?,
            Err(e) => return Err(api("registering the image artifact", &e)),
        };
        let expected_name = image::artifact_name(parent, digest)
            .ok_or_else(|| Outcome::platform("the image digest is not canonical", false))?;
        if registered.name != expected_name
            || registered.digest != digest
            || registered.kind != Some(sylphx::artifacts::ArtifactKind::Image)
            || registered.reference != expected_reference
        {
            return Err(Outcome::platform(
                "the registered Artifact does not name this managed image",
                false,
            ));
        }
        Ok(registered.name)
    }

    /// The Artifact already registered for `digest`, checked to be that same
    /// image.
    async fn existing_image(
        &self,
        parent: &str,
        digest: &str,
    ) -> Result<sylphx::artifacts::Artifact, Outcome> {
        let name = image::artifact_name(parent, digest).ok_or_else(|| {
            Outcome::platform(format!("{parent}: no artifact name for {digest}"), false)
        })?;
        let mut request = sylphx::artifacts::GetArtifactRequest::default();
        request.name = name.clone();
        let artifact = self
            .client
            .artifacts()
            .artifacts()
            .get(request)
            .await
            .map_err(|e| api(&format!("reading the registered image artifact {name}"), &e))?;
        if artifact.digest != digest {
            return Err(Outcome::platform(
                format!("{name} holds {} and not {digest}", artifact.digest.trim()),
                false,
            ));
        }
        Ok(artifact)
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

/// The locked git dependencies a command run in `rel` can use, each packed
/// on this machine ([`git_deps::pack`]); the first that cannot be fetched is
/// the error.
fn prepare_git_deps(
    root: &Path,
    tree: &sync::Tree,
    rel: &str,
) -> Result<Vec<(git_deps::Dep, PathBuf)>, String> {
    let deps =
        git_deps::locked(root, tree, rel).map_err(|e| format!("reading the lock files: {e}"))?;
    if deps.is_empty() {
        return Ok(Vec::new());
    }
    let cache = sync::source_cache(root).join("git-deps");
    std::fs::create_dir_all(&cache).map_err(|e| format!("preparing {}: {e}", cache.display()))?;
    deps.into_iter()
        .map(|d| git_deps::pack(&cache, &d).map(|p| (d, p)))
        .collect()
}

/// The lease resource already known to a failure. Create can fail before the
/// platform returns a name; say that instead of printing an empty one.
fn lease_label(name: &str) -> String {
    if name.is_empty() {
        "the lease was never created".into()
    } else {
        format!("lease {name}")
    }
}

/// Keep the machine receipt even when failure happens before source sync.
fn with_lease(text: &str, lease: Option<&str>) -> String {
    match lease.filter(|name| !name.is_empty() && !text.contains(*name)) {
        Some(name) => format!("{text} (lease {})", name.rsplit('/').next().unwrap_or(name)),
        None => text.to_string(),
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
        Outcome::Lost(reason) => Outcome::Lost(build_cache::scrub(&reason, secrets)),
        other => other,
    }
}

/// A failure text that says a disk was full: ENOSPC from a guest tool, or
/// envd's 507 for an upload it could not finish ("not enough disk space
/// available", "not enough inodes available").
fn out_of_space(text: &str) -> bool {
    text.contains("No space left on device")
        || text.contains("ENOSPC")
        || text.contains("507 Insufficient Storage")
        || text.contains("not enough disk space")
        || text.contains("not enough inodes")
}

/// How [`Run::sync`] names a failed apply step.
const APPLY_FAILED: &str = "applying the sync failed";

/// A sync failure the run treats as a full workspace: one that says so, or
/// any failure of the bootstrap or apply step. Whatever stops these steps (a
/// truncated upload, a tool that died without saying ENOSPC) can leave the
/// tree and its manifest half-written, so the one retry starts from an
/// emptied workspace (a cold sync) rather than a second try on the same
/// state. A second failure is 125, retryable, as any loss before the start.
fn workspace_full(text: &str) -> bool {
    out_of_space(text) || text.starts_with(APPLY_FAILED)
}

/// One shell command, preserving every argument without interpolation.
fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn shell_call(args: &[&str]) -> String {
    format!(
        "{}\n",
        args.iter()
            .map(|s| shell_word(s))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
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
    use sylphx_build_lease::scripts::{PROVISION, RUN};
    use sylphx_build_lease::{provision_failure, BUILD_PACKAGES};

    fn pending_lease(condition: Option<(&str, &str)>) -> sbx::Lease {
        let mut lease = sbx::Lease::default();
        lease.name = "sbx-progress".into();
        let mut status = sbx::LeaseStatus::default();
        status.state = Some(sbx::LeaseState::Granted);
        if let Some((reason, message)) = condition {
            let mut admitted = sylphx::common::Condition::default();
            admitted.r#type = "Admitted".into();
            admitted.reason = reason.into();
            admitted.message = message.into();
            status.conditions.push(admitted);
        }
        lease.status = Some(status);
        lease
    }

    #[test]
    fn queue_heartbeat_reports_present_admission_condition() {
        assert_eq!(queue_progress(&pending_lease(Some(("WaitingForCapacity", "waiting for capacity"))), Duration::from_secs(30)),
            "waiting for build machine (lease sbx-progress): WaitingForCapacity: waiting for capacity; elapsed 30s");
    }

    #[test]
    fn queue_heartbeat_falls_back_when_admission_condition_absent() {
        assert_eq!(
            queue_progress(&pending_lease(None), Duration::from_secs(60)),
            "waiting for build machine (lease sbx-progress): granted; elapsed 60s"
        );
    }

    #[test]
    fn queue_heartbeat_tolerates_unknown_admission_reason_and_controls() {
        let text = queue_progress(
            &pending_lease(Some(("FutureReason", "safe\nmessage\u{1b}"))),
            Duration::from_secs(90),
        );
        assert_eq!(text, "waiting for build machine (lease sbx-progress): FutureReason: safemessage; elapsed 90s");
    }

    #[test]
    fn queue_heartbeat_is_human_only_and_quiet_suppresses_it() {
        let text = queue_progress(&pending_lease(None), Duration::from_secs(30));
        assert_eq!(
            Out {
                json: false,
                quiet: false
            }
            .progress_line(&text),
            Some(
                "sylphx: waiting for build machine (lease sbx-progress): granted; elapsed 30s"
                    .into()
            )
        );
        for (json, quiet) in [(false, true), (true, false), (true, true)] {
            assert_eq!(Out { json, quiet }.progress_line(&text), None);
        }
    }

    #[tokio::test]
    async fn pipeline_bounds_concurrency_and_preserves_mint_upload_order() {
        use std::sync::atomic::AtomicUsize;
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let completed = Mutex::new(Vec::new());
        let results = pipeline(0..12, |i| {
            let (active, peak, completed) = (&active, &peak, &completed);
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                // Reverse completion order within each window.
                tokio::time::sleep(Duration::from_millis((4 - i % 4) * 5)).await;
                completed.lock().unwrap().push(i);
                active.fetch_sub(1, Ordering::SeqCst);
                i
            }
        })
        .await;
        assert_eq!(results, (0..12).collect::<Vec<_>>());
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_ne!(*completed.lock().unwrap(), results);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pipeline_drains_work_and_returns_errors_in_input_order() {
        let completed = Mutex::new(Vec::new());
        let results = pipeline(0..8, |i| {
            let completed = &completed;
            async move {
                tokio::task::yield_now().await;
                completed.lock().unwrap().push(i);
                match i {
                    1 | 3 => Err(i),
                    _ => Ok(i),
                }
            }
        })
        .await;
        assert_eq!(completed.lock().unwrap().len(), 8);
        assert_eq!(results.into_iter().collect::<Result<Vec<_>, _>>(), Err(1));
    }

    #[cfg(unix)]
    #[test]
    fn source_headers_are_private_atomic_and_not_shell_expanded() {
        use std::os::unix::fs::PermissionsExt;
        let tree = GitTree::new();
        let dir = tree.0.join("private");
        let path = dir.join("gitconfig");
        for secret in ["old", "new\n$(exit 1) ' \" \\\n"] {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", SOURCE_AUTH_WRITE, "source-auth"])
                .arg(&dir)
                .arg(&path)
                .env("SYLPHX_SOURCE_HEADERS", secret)
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(output.stdout.is_empty());
            assert!(output.stderr.is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), secret.as_bytes());
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        }
    }

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

    /// A fresh git work tree in a temporary directory, removed on drop, so a
    /// test that parses a run over a tree never depends on the caller's
    /// checkout (a copied source tree has no `.git`).
    struct GitTree(std::path::PathBuf);

    impl GitTree {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "sylphx-build-tree-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&p)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success();
            assert!(ok, "git init in {}", p.display());
            Self(p)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for GitTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run in a separate test process so Git tracing and CLI context are scoped
    /// to this invocation, without changing other tests' environment or cwd.
    #[tokio::test]
    #[ignore = "subprocess entry point for the history ordering tests"]
    async fn source_history_order_child() {
        let root = std::env::var_os("SYLPHX_TEST_HISTORY_ROOT")
            .expect("history ordering test must supply its scratch tree");
        let root = PathBuf::from(root);
        let base = std::env::var("SYLPHX_TEST_HISTORY_BASE").unwrap();
        let mut opts = parse(&[
            "sylphx",
            "build",
            "run",
            root.to_str().unwrap(),
            "--source-history",
            &base,
            "--",
            "true",
        ])
        .unwrap();
        opts.dry_run = std::env::var_os("SYLPHX_TEST_HISTORY_DRY_RUN").is_some();
        let client = Client::new(
            sylphx::HttpTransport::builder()
                .base_url(std::env::var("SYLPHX_TEST_HISTORY_API").unwrap())
                .build()
                .unwrap(),
        );
        let dry_run = opts.dry_run;
        let result = run_opts(Ok(client), None, opts).await;
        if dry_run {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(Failure::Exit(EXIT_PLATFORM))));
        }
    }

    fn bundle_starts(trace: &Path) -> Vec<Value> {
        std::fs::read_to_string(trace)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| {
                event["event"] == "start"
                    && event["argv"].as_array().is_some_and(|args| {
                        args.windows(2)
                            .any(|pair| pair[0] == "bundle" && pair[1] == "create")
                    })
            })
            .collect()
    }

    /// The local HTTP fixture used by acquisition and source-history tests.
    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> (String, Value) {
        use tokio::io::AsyncReadExt;
        let mut request = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0);
            request.extend_from_slice(&chunk[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    let line = headers.lines().next().unwrap().to_string();
                    let body = if length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap()
                    };
                    return (line, body);
                }
            }
        }
    }

    #[tokio::test]
    async fn queued_run_and_image_leases_never_reserve_volumes_and_keep_region_binding() {
        use tokio::io::AsyncWriteExt;
        for image_build in [false, true] {
            for region in [None, Some("fsn1")] {
                let tree = GitTree::new();
                let mut args = vec![
                    "sylphx",
                    "build",
                    if image_build { "image" } else { "run" },
                    tree.path(),
                ];
                if let Some(region) = region {
                    args.extend(["--region", region]);
                }
                if !image_build {
                    args.extend(["--fresh", "--", "true"]);
                }
                let root = Command::new("sylphx")
                    .arg(
                        Arg::new("output")
                            .long("output")
                            .short('o')
                            .global(true)
                            .default_value("table"),
                    )
                    .subcommand(
                        Command::new("build")
                            .subcommand(command())
                            .subcommand(image_command()),
                    );
                let matches = root.try_get_matches_from(args).unwrap();
                let (_, build) = matches.subcommand().unwrap();
                let (subcommand, matches) = build.subcommand().unwrap();
                let opts = if subcommand == "image" {
                    Opts::parse_image(matches)
                } else {
                    Opts::parse(matches)
                }
                .unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}", listener.local_addr().unwrap());
                let accepted = region.unwrap_or("gra");
                let server = tokio::spawn(async move {
                    // Only lease POST/GET is allowed. Any list/create/attach
                    // Volume request, including while queued, fails this fixture.
                    for attempt in 0..2 {
                        for create in [true, false] {
                            let (mut socket, _) = listener.accept().await.unwrap();
                            let (line, body) = read_http_request(&mut socket).await;
                            if create {
                                assert!(
                                    line.starts_with("POST ") && line.contains("/leases "),
                                    "{line}"
                                );
                                let wanted = if attempt == 0 {
                                    region.unwrap_or("")
                                } else {
                                    accepted
                                };
                                if wanted.is_empty() {
                                    assert!(
                                        body["spec"]
                                            .get("region")
                                            .is_none_or(|r| r.as_str() == Some("")),
                                        "{body}"
                                    );
                                } else {
                                    assert_eq!(body["spec"]["region"], wanted, "{body}");
                                }
                                assert!(
                                    body["spec"]
                                        .get("volumes")
                                        .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
                                    "{body}"
                                );
                                assert_eq!(body["spec"]["shape"], "build-large");
                            } else {
                                assert!(
                                    line.starts_with("GET ") && line.contains("/leases/l1 "),
                                    "{line}"
                                );
                            }
                            let response = json!({
                                "name": "orgs/org_a/projects/prj_a/envs/env_a/leases/l1",
                                "spec": {"region": accepted},
                                "status": if create { json!({"state": "queued"}) } else { json!({"state": "ended", "endReason": "machine_lost"}) },
                            }).to_string();
                            socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
                            socket.shutdown().await.unwrap();
                        }
                    }
                });
                let client = Client::new(
                    sylphx::HttpTransport::builder()
                        .base_url(url)
                        .build()
                        .unwrap(),
                );
                let out = Out {
                    json: false,
                    quiet: true,
                };
                let held = Held::default();
                let stats = Mutex::new(Stats::default());
                let mut run = Run {
                    client: &client,
                    o: &opts,
                    out: &out,
                    held: &held,
                    stats: &stats,
                    parent: "orgs/org_a/projects/prj_a/envs/env_a".into(),
                    tree: BTreeMap::new(),
                    git_deps: Vec::new(),
                    source_history: None,
                    key: None,
                    cache_url: String::new(),
                    cache_warned: AtomicBool::new(false),
                    source_warned: AtomicBool::new(false),
                    accepted_region: None,
                };
                for _ in 0..2 {
                    let acquired = tokio::time::timeout(Duration::from_secs(10), run.acquire())
                        .await
                        .unwrap();
                    assert!(matches!(acquired, Err(Attempt::Lost(_))));
                    assert_eq!(run.accepted_region.as_deref(), Some(accepted));
                    assert_eq!(run.region(), accepted);
                }
                server.await.unwrap();
            }
        }
    }

    async fn history_before_acquire(base: &str, dry_run: bool, expect_attempts: usize) {
        use tokio::io::AsyncWriteExt;

        let tree = GitTree::new();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&tree.0)
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        std::fs::write(tree.0.join("file"), "committed\n").unwrap();
        git(&["add", "file"]);
        git(&["commit", "-qm", "source"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(tree.0.join("file"), "dirty\n").unwrap();
        let trace = tree.0.join(".git/source-history-trace.jsonl");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let starts_at_acquire = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen = starts_at_acquire.clone();
        let source = tree.0.clone();
        let traced = trace.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (line, _) = read_http_request(&mut socket).await;
                let (status, body) = if line.starts_with("POST ") && line.contains("/leases ") {
                    let starts = bundle_starts(&traced);
                    let events = std::fs::read_to_string(&traced).unwrap_or_default();
                    let completed = starts
                        .iter()
                        .filter(|start| {
                            events.lines().any(|line| {
                                serde_json::from_str::<Value>(line).is_ok_and(|event| {
                                    event["event"] == "exit"
                                        && event["sid"] == start["sid"]
                                        && event["code"] == 0
                                })
                            })
                        })
                        .count();
                    seen.lock().push(completed);
                    // Any attempted re-preparation after this acquisition must
                    // fail: only the already-built bundle can be reused.
                    let reference = source.join(".git/refs/remotes/origin/main");
                    if reference.exists() {
                        std::fs::remove_file(reference).unwrap();
                    }
                    (
                        200,
                        json!({"name": "orgs/org_a/projects/prj_a/envs/env_a/leases/l1",
                        "status": {"state": "ended", "endReason": "machine_lost"}}),
                    )
                } else {
                    panic!("unexpected API request: {line}");
                };
                let body = body.to_string();
                socket.write_all(format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                ).as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "build_run::tests::source_history_order_child",
                "--ignored",
                "--nocapture",
            ])
            .current_dir(&tree.0)
            .env("SYLPHX_TEST_HISTORY_ROOT", &tree.0)
            .env("SYLPHX_TEST_HISTORY_BASE", base)
            .env("SYLPHX_TEST_HISTORY_API", url)
            .env(
                crate::context::ENV_VAR,
                "orgs/org_a/projects/prj_a/envs/env_a",
            )
            .env("GIT_TRACE2_EVENT", &trace)
            .env_remove("SYLPHX_TEST_HISTORY_DRY_RUN")
            .kill_on_drop(true);
        if dry_run {
            child.env("SYLPHX_TEST_HISTORY_DRY_RUN", "1");
        }
        let result = tokio::time::timeout(Duration::from_secs(30), child.output()).await;
        server.abort();
        let output = result.unwrap().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if expect_attempts == 0 && !dry_run {
            assert!(String::from_utf8_lossy(&output.stderr).contains("preparing source history"));
        }
        assert_eq!(*starts_at_acquire.lock(), vec![1; expect_attempts]);
        assert_eq!(
            bundle_starts(&trace).len(),
            usize::from(expect_attempts > 0)
        );
        assert_eq!(
            std::fs::read_to_string(tree.0.join("file")).unwrap(),
            "dirty\n"
        );
    }

    #[tokio::test]
    async fn source_history_finishes_before_acquire_and_is_not_reprepared_on_retry() {
        history_before_acquire("origin/main", false, 2).await;
    }

    #[tokio::test]
    async fn source_history_failure_never_acquires_a_lease() {
        history_before_acquire("origin/missing", false, 0).await;
    }

    #[tokio::test]
    async fn dry_run_does_not_prepare_source_history_or_acquire() {
        history_before_acquire("origin/missing", true, 0).await;
    }

    #[test]
    fn source_history_is_rejected_for_no_tree_runs() {
        assert!(parse(&[
            "sylphx",
            "build",
            "run",
            "--no-tree",
            "--source-history",
            "origin/main",
            "--",
            "true",
        ])
        .is_err());
    }

    #[test]
    fn lease_egress_is_the_cache_gateway_plus_allow_hosts_only() {
        // No internet package host by default: the mirrors serve them.
        assert_eq!(allowed_domains(&[]), ["build-cache.sylphx.net"]);
        for h in [
            "index.crates.io",
            "static.crates.io",
            "static.rust-lang.org",
            "registry.npmjs.org",
            "github.com",
            "codeload.github.com",
            "objects.githubusercontent.com",
        ] {
            assert!(!allowed_domains(&[]).contains(&h.to_string()), "{h}");
        }
        // A host the caller names is the opt-in.
        let d = allowed_domains(&["registry.npmjs.org".to_string()]);
        assert_eq!(d, ["build-cache.sylphx.net", "registry.npmjs.org"]);
    }

    #[test]
    fn an_image_build_leases_with_no_internet_package_host() {
        let root = Command::new("sylphx")
            .arg(
                Arg::new("output")
                    .long("output")
                    .short('o')
                    .global(true)
                    .default_value("table"),
            )
            .subcommand(Command::new("build").subcommand(image_command()));
        // A work tree of its own: the lease's copy has no `.git`.
        let tree = std::env::temp_dir().join(format!("sylphx-image-egress-{}", std::process::id()));
        std::fs::create_dir_all(&tree).unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(&tree)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let m = root
            .try_get_matches_from([
                "sylphx",
                "build",
                "image",
                tree.to_str().unwrap(),
                "--allow-host",
                "Proxy.Example.com",
            ])
            .unwrap();
        let (_, b) = m.subcommand().unwrap();
        let (_, r) = b.subcommand().unwrap();
        let o = Opts::parse_image(r).unwrap();
        let _ = std::fs::remove_dir_all(&tree);
        // A managed build (no --push) reaches the project's registry and
        // nothing else beyond the cache and the caller's own hosts.
        assert_eq!(
            lease_domains(&o),
            [
                "build-cache.sylphx.net",
                image::MANAGED_REGISTRY_HOST,
                "proxy.example.com"
            ]
        );
        assert!(o.command[5].ends_with("sylphx-image-build"));
        // `build run` keeps its preset.
        assert_eq!(allowed_domains(&[]).len(), BUILD_PACKAGES.len());
    }

    // The guest scripts run for real under /bin/sh in a scratch workspace;
    // rustup is a fake placed where the script puts the workspace's cargo/bin.
    struct Ws(PathBuf);

    /// The toolchain the fake rustup installs.
    const TC: &str = "1.99.0-x86_64-unknown-linux-gnu";

    impl Ws {
        fn new(tag: &str) -> Ws {
            let d = std::env::temp_dir().join(format!(
                "sylphx-build-run-{tag}-{}-{}",
                std::process::id(),
                sylphx_build_lease::guest::rand_u64()
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

        /// A fake rustup over real toolchain directories: `which rustc`
        /// answers `<home>/toolchains/<TC>/bin/rustc` when it is there, with
        /// the template's home (`/opt/rustup`) kept in `image-rustup` of the
        /// workspace; `toolchain install` writes the toolchain (whose rustc
        /// runs only while its driver library is intact) unless `offline`
        /// exists, and fails through the cache tier while `tier-down` exists.
        /// Like rustup, it leaves a toolchain directory that is already there
        /// alone. Each download adds a line to `fetches`.
        fn rustup(&self, fail_install: bool) {
            let w = self.0.display();
            if fail_install {
                self.file("offline", "");
            }
            let body = format!(
                r#"#!/bin/sh
H=${{RUSTUP_HOME:-$HOME/.rustup}}
[ "$H" = /opt/rustup ] && H='{w}/image-rustup'
T="$H/toolchains/{TC}"
case "$1 $2" in
 which*) [ -x "$T/bin/rustc" ] && echo "$T/bin/rustc" ;;
 "toolchain install")
  [ -e '{w}/offline' ] && {{ echo 'error: could not download channel-rust-1.99.0.toml.sha256 (Connection timed out)' >&2; exit 1; }}
  [ -n "$RUSTUP_DIST_SERVER" ] && [ -e '{w}/tier-down' ] && {{ echo 'error: 502 from the tier' >&2; exit 1; }}
  [ -d "$T" ] && exit 0
  mkdir -p "$T/bin" "$T/lib/rustlib"
  printf 'rustc\n' > "$T/lib/rustlib/components"
  printf 'driver\n' > "$T/lib/librustc_driver.so"
  printf '#!/bin/sh\n[ "$(cat "$(dirname "$0")/../lib/librustc_driver.so")" = driver ] || {{ echo "error while loading shared libraries: librustc_driver.so: cannot read file data" >&2; exit 127; }}\necho "rustc 1.99.0"\n' > "$T/bin/rustc"
  printf '#!/bin/sh\necho "cargo 1.99.0"\n' > "$T/bin/cargo"
  chmod +x "$T/bin/rustc" "$T/bin/cargo"
  echo "${{RUSTUP_DIST_SERVER:-direct}}" >> '{w}/fetches' ;;
 "toolchain uninstall") rm -rf "$H/toolchains/$3" ;;
 default*) echo 'error: no network' >&2; exit 1 ;;
esac
"#
            );
            self.file("cargo/bin/rustup", &body);
            use std::os::unix::fs::PermissionsExt;
            let p = self.0.join("cargo/bin/rustup");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// Bakes the toolchain into the template's home, as the image does.
        fn bake_image(&self) {
            // Through sh, not exec'd: a test thread forking while another
            // just wrote its fake would make the exec fail with ETXTBSY.
            let out = std::process::Command::new("/bin/sh")
                .arg(self.0.join("cargo/bin/rustup"))
                .args(["toolchain", "install"])
                .env_clear()
                .env("PATH", SYS_PATH)
                .env("RUSTUP_HOME", "/opt/rustup")
                .output()
                .unwrap();
            assert!(out.status.success());
            let _ = std::fs::remove_file(self.0.join("fetches"));
        }

        /// The toolchain directory under a rustup home of the workspace.
        fn tc(&self, home: &str) -> PathBuf {
            self.0.join(home).join("toolchains").join(TC)
        }

        /// How many downloads the fake rustup made.
        fn fetches(&self) -> usize {
            std::fs::read_to_string(self.0.join("fetches"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }

        fn sh(&self, script: &str, rel: &str, extra: &[&str]) -> (i32, String) {
            self.sh_env(script, rel, extra, &[])
        }

        fn sh_env(
            &self,
            script: &str,
            rel: &str,
            extra: &[&str],
            env: &[(&str, &str)],
        ) -> (i32, String) {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .arg("sylphx-test")
                .arg(&self.0)
                .arg(rel)
                .args(extra)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .envs(env.iter().copied())
                .output()
                .unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }
    }

    impl Ws {
        /// Runs [`RUN`] in `tree/<rel>` with `PATH` and extra `env`;
        /// answers the status, stdout and stderr.
        fn run(
            &self,
            rel: &str,
            path: &str,
            env: &[(&str, &str)],
            cmd: &str,
        ) -> (i32, String, String) {
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", RUN, "sylphx-run"])
                .arg(&self.0)
                .args([rel, "0", "sh", "-c", cmd])
                .env_clear()
                .env("PATH", path)
                .envs(env.iter().copied())
                .output()
                .unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        }

        /// A `bin` directory holding only links to `tools` (and `sh`).
        fn bin(&self, name: &str, tools: &[&str]) -> String {
            let d = self.0.join(name);
            std::fs::create_dir_all(&d).unwrap();
            for t in tools.iter().chain(&["sh"]) {
                let src = ["/usr/bin", "/bin"]
                    .iter()
                    .map(|b| Path::new(b).join(t))
                    .find(|p| p.exists())
                    .unwrap_or_else(|| panic!("{t} is not installed"));
                let _ = std::os::unix::fs::symlink(src, d.join(t));
            }
            d.display().to_string()
        }
    }

    const SYS_PATH: &str = "/usr/bin:/bin";

    #[test]
    fn the_tree_is_a_git_work_tree_naming_the_local_head() {
        let w = Ws::new("git-tree");
        w.file("tree/crate/a.txt", "a\n");
        w.file("tree/b.txt", "b\n");
        std::os::unix::fs::symlink("../target", w.0.join("tree/target")).unwrap();
        let (code, out, err) = w.run(
            "crate",
            SYS_PATH,
            &[("SYLPHX_BUILD_GIT_HEAD", "0123abc")],
            "git rev-parse --is-inside-work-tree && git log -1 --format=%s && git ls-files --full-name :/",
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(
            out,
            "true\nsylphx build run of 0123abc\nb.txt\ncrate/a.txt\n"
        );
        assert!(!err.contains("warning"), "{err}");
    }

    #[test]
    fn source_history_run_keeps_the_imported_head_and_dirty_files() {
        let w = Ws::new("source-history-run");
        w.file("tree/file", "base\n");
        let (code, head, err) = w.run(".", SYS_PATH, &[], "git rev-parse HEAD");
        assert_eq!(code, 0, "{err}");
        w.file("tree/file", "dirty\n");
        let (code, out, err) = w.run(
            ".",
            SYS_PATH,
            &[("SYLPHX_BUILD_SOURCE_HISTORY", "1")],
            "git rev-parse HEAD && git diff --name-only && cat file",
        );
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, format!("{head}file\ndirty\n"));
    }

    #[test]
    fn a_warm_workspace_keeps_one_commit_of_the_current_tree() {
        let w = Ws::new("git-warm");
        w.file("tree/keep.txt", "keep\n");
        w.file("tree/edit.txt", "v1\n");
        let (code, _, err) = w.run(".", SYS_PATH, &[], "true");
        assert_eq!(code, 0, "{err}");
        w.file("tree/edit.txt", "v2\n");
        let (code, out, err) = w.run(
            ".",
            SYS_PATH,
            &[],
            "git rev-list --count --all && git log -1 --format=%s && git status --porcelain && git show HEAD:edit.txt && git ls-files && test -z \"$(git reflog)\" && echo no-reflog",
        );
        assert_eq!(code, 0, "{err}");
        // One commit however many runs and no reflog keeping the replaced ones:
        // the warm repository does not grow per run.
        assert_eq!(
            out,
            "1\nsylphx build run of an unknown commit\nv2\nedit.txt\nkeep.txt\nno-reflog\n"
        );
        assert!(!err.contains("warning"), "{err}");
    }

    #[test]
    fn a_failed_git_step_warns_and_the_command_keeps_its_exit_code() {
        let w = Ws::new("git-fail");
        w.file("tree/a.txt", "a\n");
        w.file(
            "fake/git",
            "#!/bin/sh\necho 'fatal: disk full' >&2\nexit 128\n",
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(w.0.join("fake/git"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let path = format!("{}:{SYS_PATH}", w.0.join("fake").display());
        let (code, out, err) = w.run(".", &path, &[], "echo ran; exit 7");
        assert_eq!((code, out.as_str()), (7, "ran\n"), "{err}");
        assert_eq!(err.matches("sylphx: warning:").count(), 1, "{err}");
        assert!(err.contains("fatal: disk full"), "{err}");
    }

    #[test]
    fn a_machine_without_git_warns_and_runs_the_command() {
        let w = Ws::new("git-none");
        w.file("tree/a.txt", "a\n");
        let path = w.bin("nogit", &["mkdir", "rm", "tail", "python3"]);
        let (code, out, err) = w.run(".", &path, &[], "echo ran");
        assert_eq!((code, out.as_str()), (0, "ran\n"), "{err}");
        assert_eq!(err.matches("sylphx: warning:").count(), 1, "{err}");
        assert!(err.contains("has no git"), "{err}");
        assert!(!w.0.join("tree/.git").exists());
    }

    #[test]
    fn disk_full_messages_are_distinguished_from_other_guest_errors() {
        assert!(out_of_space("gzip: stdout: No space left on device"));
        assert!(!out_of_space("guest answered 503"));
    }

    /// Any failed apply and envd's 507 for a cut-short upload retry once on
    /// a fresh lease-owned workspace; other faults keep their own verdict.
    #[test]
    fn a_failed_apply_or_a_cut_short_upload_counts_as_a_full_workspace() {
        assert!(workspace_full(
            "applying the sync failed: gzip: stdin: unexpected end of file"
        ));
        assert!(workspace_full(&format!("{APPLY_FAILED}: tar: short read")));
        let envd = r#"guest answered 507 Insufficient Storage: {"code":507,"message":"not enough disk space available"}"#;
        assert!(out_of_space(envd) && workspace_full(envd));
        assert!(out_of_space("not enough inodes available: no space"));
        assert!(!workspace_full("guest answered 503 Service Unavailable"));
        assert!(!workspace_full("preparing the git dependencies failed: x"));
    }

    #[test]
    fn cargo_uses_the_persistent_target_unless_the_caller_says_otherwise() {
        let w = Ws::new("cargo-env");
        w.file("tree/a.txt", "a\n");
        let show =
            r#"echo "inc=${CARGO_INCREMENTAL-unset} debug=${CARGO_PROFILE_DEV_DEBUG-unset}""#;
        let (code, out, err) = w.run(".", SYS_PATH, &[], show);
        assert_eq!((code, out.as_str()), (0, "inc=1 debug=0\n"), "{err}");
        let (code, out, err) = w.run(
            ".",
            SYS_PATH,
            &[("CARGO_INCREMENTAL", "1"), ("CARGO_PROFILE_DEV_DEBUG", "")],
            show,
        );
        assert_eq!((code, out.as_str()), (0, "inc=1 debug=\n"), "{err}");
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
        assert!(w.tc("rustup").join(".sylphx-sha256").exists());
        // Already installed and intact: nothing to do.
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
        assert_eq!(w.fetches(), 1);
    }

    #[test]
    fn the_template_toolchain_serves_a_tree_it_holds_with_no_workspace_copy() {
        let w = Ws::new("tc-image");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        w.bake_image();
        // A lease's guest inherits no RUSTUP_HOME: the template's is chosen,
        // nothing is fetched and nothing is read from the workspace.
        let (code, err) = w.sh(PROVISION, ".", &[]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(w.fetches(), 0);
        assert!(!w.0.join("rustup").exists());
        // A stale workspace copy of the same version is never run.
        std::fs::create_dir_all(w.tc("rustup").join("lib")).unwrap();
        std::fs::write(w.tc("rustup").join("lib/librustc_driver.so"), "torn").unwrap();
        let (code, out, err) = w.run(".", SYS_PATH, &[], "echo $RUSTUP_HOME");
        assert_eq!((code, out.as_str()), (0, "/opt/rustup\n"), "{err}");
    }

    #[test]
    fn provisioning_prunes_only_the_workspace_copy_served_by_the_template() {
        let w = Ws::new("tc-image-prune");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
        w.file(&format!("rustup/update-hashes/{TC}"), "old");
        w.file("rustup/toolchains/other/bin/rustc", "keep");
        w.file("rustup/update-hashes/other", "keep");
        w.bake_image();

        for _ in 0..2 {
            let (code, err) = w.sh(PROVISION, ".", &[]);
            assert_eq!(code, 0, "{err}");
            assert!(!w.tc("rustup").exists());
            assert!(!w.0.join("rustup/update-hashes").join(TC).exists());
            assert!(w.0.join("rustup/toolchains/other/bin/rustc").exists());
            assert!(w.0.join("rustup/update-hashes/other").exists());
            assert!(w.tc("image-rustup").join("bin/rustc").exists());
            assert_eq!(w.fetches(), 0);
        }
    }

    #[test]
    fn a_broken_template_toolchain_is_a_platform_failure() {
        let w = Ws::new("tc-image-bad");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        w.bake_image();
        std::fs::write(w.tc("image-rustup").join("lib/librustc_driver.so"), "").unwrap();
        let (code, err) = w.sh(PROVISION, ".", &[]);
        assert_eq!(code, 4, "{err}");
        assert!(err.contains("cannot read file data"), "{err}");
        assert_eq!(
            Outcome::platform(provision_failure(code, err.as_bytes()), true).code(),
            125
        );
    }

    #[test]
    fn a_corrupted_workspace_toolchain_is_fetched_again_never_run() {
        let w = Ws::new("tc-corrupt");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
        assert_eq!(w.fetches(), 1);
        // One file torn on the Volume, the way fb 3718's driver was, and one
        // left readable but altered: each is caught before the command.
        for torn in ["", "driver\nextra\n"] {
            std::fs::write(w.tc("rustup").join("lib/librustc_driver.so"), torn).unwrap();
            let before = w.fetches();
            let (code, err) = w.sh(PROVISION, ".", &[]);
            assert_eq!(code, 0, "{err}");
            assert_eq!(w.fetches(), before + 1);
            let log = std::fs::read_to_string(w.0.join(".sylphx/toolchain.log")).unwrap();
            assert!(log.contains("failed its check"), "{log}");
            // The command runs the fresh copy.
            let (code, out, err) = w.run(
                ".",
                SYS_PATH,
                &[("TC", TC)],
                "\"$RUSTUP_HOME/toolchains/$TC/bin/rustc\" -vV",
            );
            assert_eq!((code, out.as_str()), (0, "rustc 1.99.0\n"), "{err}");
        }
    }

    #[test]
    fn a_component_added_by_a_command_keeps_the_workspace_toolchain() {
        let w = Ws::new("tc-component");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
        // What `rustup component add rust-src` changes and adds.
        let lib = w.tc("rustup").join("lib/rustlib");
        std::fs::write(lib.join("components"), "rustc\nrust-src\n").unwrap();
        std::fs::write(lib.join("manifest-rust-src"), "file:lib/rustlib/src\n").unwrap();
        let (code, err) = w.sh(PROVISION, ".", &[]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(w.fetches(), 1);
    }

    #[test]
    fn a_corrupted_workspace_toolchain_that_cannot_be_fetched_is_125() {
        let w = Ws::new("tc-corrupt-offline");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
        std::fs::write(w.tc("rustup").join("lib/librustc_driver.so"), "").unwrap();
        w.file("offline", "");
        let (code, err) = w.sh(PROVISION, ".", &[]);
        assert_eq!(code, 4, "{err}");
        assert_eq!(
            Outcome::platform(provision_failure(code, err.as_bytes()), true).code(),
            125
        );
    }

    #[test]
    fn a_half_written_or_unsealed_workspace_toolchain_is_installed_again() {
        // Left by a lease that was lost mid-install: the directory is there,
        // its rustc is not, so rustup alone would call it installed.
        let w = Ws::new("tc-half");
        w.file("tree/Cargo.toml", "[workspace]\n");
        w.rustup(false);
        std::fs::create_dir_all(w.tc("rustup").join("lib")).unwrap();
        let (code, err) = w.sh(PROVISION, ".", &[]);
        assert_eq!(code, 0, "{err}");
        assert!(w.tc("rustup").join("bin/rustc").exists());
        // Installed by an older client, which wrote no checksums: once again.
        std::fs::remove_file(w.tc("rustup").join(".sylphx-sha256")).unwrap();
        let before = w.fetches();
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
        assert_eq!(w.sh(PROVISION, ".", &[]).0, 0);
        assert_eq!(w.fetches(), before + 1);
    }

    #[test]
    fn toolchains_come_through_the_cache_tier_and_directly_when_it_fails() {
        let dist = "http://build-cache.env-1.svc.cluster.local/upstream/static.rust-lang.org";
        let cache: BTreeMap<String, String> = [
            ("RUSTUP_DIST_SERVER", dist.to_string()),
            ("RUSTUP_UPDATE_ROOT", format!("{dist}/rustup")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let pairs: Vec<(&str, &str)> = cache
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        for (down, want) in [(false, dist), (true, "direct")] {
            let w = Ws::new("tc-tier");
            w.file("tree/Cargo.toml", "[workspace]\n");
            w.rustup(false);
            if down {
                w.file("tier-down", "");
            }
            let (code, err) = w.sh_env(PROVISION, ".", &[], &pairs);
            assert_eq!(code, 0, "{err}");
            assert_eq!(
                std::fs::read_to_string(w.0.join("fetches")).unwrap(),
                format!("{want}\n")
            );
        }
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
        let tree = GitTree::new();
        let o = parse(&[
            "sylphx",
            "build",
            "run",
            tree.path(),
            "--",
            "cargo",
            "test",
            "-p",
            "x",
        ])
        .unwrap();
        assert_eq!(o.command, ["cargo", "test", "-p", "x"]);
        assert_eq!(o.size, "large");
        assert_eq!(o.timeout, DEFAULT_TIMEOUT);
        assert_eq!(
            o.region, None,
            "no --region: automatic ephemeral Build placement"
        );
        assert_eq!(o.queue_timeout, DEFAULT_QUEUE_TIMEOUT);
        assert!(!o.json);
        let o = parse(&[
            "sylphx",
            "-o",
            "json",
            "build",
            "run",
            tree.path(),
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
        let tree = GitTree::new();
        let o = parse(&[
            "sylphx",
            "build",
            "run",
            tree.path(),
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

    #[test]
    fn the_default_queue_budget_covers_measured_cold_fallback() {
        let tree = GitTree::new();
        let o = parse(&["sylphx", "build", "run", tree.path(), "--", "true"]).unwrap();
        assert_eq!(o.queue_timeout, Duration::from_secs(1800));
        // The incident sample's nearest-rank p99 was 225.128s to READY;
        // volume-pinned requeues made the longest caller wait over 450s.
        assert!(o.queue_timeout > Duration::from_secs(455));
        let explicit = parse(&[
            "sylphx",
            "build",
            "run",
            "--queue-timeout",
            "120s",
            tree.path(),
            "--",
            "true",
        ])
        .unwrap();
        assert_eq!(explicit.queue_timeout, Duration::from_secs(120));
    }

    #[test]
    fn attribution_labels_parse_and_are_sorted_for_the_lease() {
        let tree = GitTree::new();
        let o = parse(&[
            "sylphx",
            "build",
            "run",
            "--label",
            "worktree=w-22292",
            "--label",
            "source=sylphxai-cloud",
            tree.path(),
            "--",
            "true",
        ])
        .unwrap();
        assert_eq!(
            o.labels,
            vec![
                ("source".into(), "sylphxai-cloud".into()),
                ("worktree".into(), "w-22292".into()),
            ]
        );
        // No label is the default: the platform's own `purpose` is all there is.
        let plain = parse(&["sylphx", "build", "run", tree.path(), "--", "true"]).unwrap();
        assert!(plain.labels.is_empty());
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
            &["--env", "SYLPHX_ENVIRONMENT=orgs/o/projects/p/envs/e"],
            &["--label", "NOVALUE"],
            &["--label", "Key=v"],
            &["--label", "k=V"],
            &["--label", "purpose=other"],
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
                "queued" => &[("labels", Value::is_object)],
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
                    ("from_store", Value::is_u64),
                ],
                other => panic!("unknown event type {other}"),
            };
            for (k, ok) in need {
                assert!(ok(&v[*k]), "{t}.{k} in {v}");
            }
        }
        // The events as the code builds them.
        let stats = Stats {
            lease: None,
            bytes_up: 512,
            bytes_down: 2048,
            artifacts: 0,
            from_store: 7,
            image: Some(json!({"digest": "sha256:aa"})),
            artifact: Some("orgs/o/projects/p/artifacts/sha256-aa".into()),
            source_ref: Some("registry.sylphx.com/o/p-image@sha256:aa".into()),
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
            json!({"type": "sync", "full": true, "files": 1, "removed": 0, "bytes": 10, "bytes_up": 300, "duration_ms": 5, "workspace": "cold", "volume": false}),
        );
        sink(
            json!({"type": "running", "lease": "l", "size": "large", "workspace": "cold", "volume": false}),
        );
        sink(json!({"type": "artifact", "path": "p", "digest": "sha256:aa", "bytes": 3}));
        for v in captured.lock().unwrap().iter() {
            check(v);
        }
    }

    /// A `Run` with nothing but a client: the pieces that need no machine.
    fn api_only_run<'a>(
        client: &'a Client,
        o: &'a Opts,
        out: &'a Out,
        held: &'a Held,
        stats: &'a Mutex<Stats>,
        parent: &str,
    ) -> Run<'a> {
        Run {
            client,
            o,
            out,
            held,
            stats,
            parent: parent.to_string(),
            tree: BTreeMap::new(),
            git_deps: Vec::new(),
            source_history: None,
            key: None,
            cache_url: String::new(),
            cache_warned: AtomicBool::new(false),
            source_warned: AtomicBool::new(false),
            accepted_region: None,
        }
    }

    /// A digest already registered is the same image: the CLI reads that
    /// Artifact back and reports it, rather than failing or claiming a new
    /// registration.
    #[tokio::test]
    async fn an_already_registered_digest_is_read_back_not_duplicated() {
        use tokio::io::AsyncWriteExt;
        let digest = format!("sha256:{}", "e".repeat(64));
        let name = format!(
            "orgs/org_a/projects/prj_a/artifacts/sha256-{}",
            "e".repeat(64)
        );
        let reference = format!(
            "{}/org_a/prj_a-image@{digest}",
            image::MANAGED_REGISTRY_HOST
        );
        let foreign = format!(
            "{}/other/project-image@{digest}",
            image::MANAGED_REGISTRY_HOST
        );
        for held_reference in [reference.as_str(), "", foreign.as_str()] {
            let answer = json!({
                "name": name.clone(), "uid": "art_hk01e", "digest": digest.clone(),
                "kind": "image", "reference": held_reference,
            })
            .to_string();
            let conflict = json!({
                "code": "RESOURCE_ALREADY_EXISTS",
                "status": 409,
                "detail": "This digest is already registered.",
                "retryable": false,
            })
            .to_string();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let path = name.clone();
            let requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
            let seen = requests.clone();
            let server = tokio::spawn(async move {
                // The create is refused, then the Artifact is read by its name.
                for (status, payload) in [(409, conflict), (200, answer)] {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let (line, body) = read_http_request(&mut socket).await;
                    seen.lock().push((line, body));
                    socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                            payload.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                    socket.shutdown().await.unwrap();
                }
            });
            let client = Client::new(
                sylphx::HttpTransport::builder()
                    .base_url(url)
                    .build()
                    .unwrap(),
            );
            let tree = GitTree::new();
            let o = parse(&["sylphx", "build", "run", tree.path(), "--", "true"]).unwrap();
            let out = Out {
                json: true,
                quiet: true,
            };
            let held = Held::default();
            let stats = Mutex::new(Stats::default());
            let run = api_only_run(
                &client,
                &o,
                &out,
                &held,
                &stats,
                "orgs/org_a/projects/prj_a/envs/env_a",
            );
            let registered = run
                .register(
                    "orgs/org_a/projects/prj_a",
                    &digest,
                    image::ImageDescriptor {
                        media_type: "application/vnd.oci.image.index.v1+json".into(),
                        digest: digest.clone(),
                        size_bytes: 4096,
                    },
                    &reference,
                )
                .await;
            if held_reference == reference {
                assert_eq!(registered.expect("the existing managed artifact"), name);
            } else {
                assert_eq!(
                    registered
                        .expect_err("a different reference is not this upload")
                        .code(),
                    125
                );
            }
            server.await.unwrap();
            let requests = requests.lock();
            assert_eq!(requests.len(), 2, "{requests:?}");
            assert!(
                requests[0].0.starts_with(
                    "POST /v1/orgs/org_a/projects/prj_a/artifacts?managed_upload=true "
                ),
                "{}",
                requests[0].0
            );
            assert!(
                requests[1].0.starts_with(&format!("GET /v1/{path} ")),
                "{}",
                requests[1].0
            );
        }
    }

    /// Admission failures name the lease the create already returned, in the
    /// text `finish` prints and in the JSON `result` error. A create that
    /// never returned a name says so instead of printing an empty one.
    #[tokio::test]
    async fn queue_timeout_names_the_lease_in_text_and_json() {
        use tokio::io::AsyncWriteExt;

        let name = "orgs/org_a/projects/prj_a/envs/env_a/leases/l1";
        let tree = GitTree::new();
        let o = parse(&[
            "sylphx",
            "build",
            "run",
            "--region",
            "hk",
            "--queue-timeout",
            "1s",
            tree.path(),
            "--",
            "true",
        ])
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let released = name.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (line, _) = read_http_request(&mut socket).await;
            assert!(
                line.starts_with("POST ") && line.contains(&released) && line.contains(":release"),
                "{line}"
            );
            let payload = json!({"name": released, "status": {"state": "ending"}}).to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });
        let client = Client::new(
            sylphx::HttpTransport::builder()
                .base_url(url)
                .build()
                .unwrap(),
        );
        let out = Out {
            json: true,
            quiet: true,
        };
        let held = Held::default();
        let stats = Mutex::new(Stats::default());
        let run = api_only_run(
            &client,
            &o,
            &out,
            &held,
            &stats,
            "orgs/org_a/projects/prj_a/envs/env_a",
        );

        let mut refused = queued_lease(name, sbx::LeaseState::Refused);
        refused.status.as_mut().unwrap().refusal = Some(sbx::LeaseRefusal::NoCapacity);
        assert_admission(
            run.ready(&client, refused, Instant::now())
                .await
                .unwrap_err(),
            &format!("the build machine was refused: no_capacity (lease {name})"),
            true,
        );

        let mut ended = queued_lease(name, sbx::LeaseState::Ended);
        ended.status.as_mut().unwrap().end_reason = Some(sbx::EndReason::BootFailed);
        assert_admission(
            run.ready(&client, ended, Instant::now()).await.unwrap_err(),
            &format!("the build machine ended before it was ready: boot_failed (lease {name})"),
            true,
        );

        let mut unnamed = queued_lease("", sbx::LeaseState::Refused);
        unnamed.status.as_mut().unwrap().refusal = Some(sbx::LeaseRefusal::ShapeNotOffered);
        assert_admission(
            run.ready(&client, unnamed, Instant::now())
                .await
                .unwrap_err(),
            "the build machine was refused: shape_not_offered (the lease was never created)",
            false,
        );

        let err = run
            .ready(
                &client,
                queued_lease(name, sbx::LeaseState::Granted),
                Instant::now(),
            )
            .await
            .unwrap_err();
        assert_admission(
            err,
            &format!("no build machine in region hk within --queue-timeout 1s (lease {name})"),
            true,
        );
        server.await.unwrap();
    }

    fn queued_lease(name: &str, state: sbx::LeaseState) -> sbx::Lease {
        let mut status = sbx::LeaseStatus::default();
        status.state = Some(state);
        let mut lease = sbx::Lease::default();
        lease.name = name.to_string();
        lease.status = Some(status);
        lease
    }

    /// The plain-text failure and the JSON `result` error are the same string.
    fn assert_admission(attempt: Attempt, want: &str, retryable: bool) {
        let Attempt::Done(outcome) = attempt else {
            panic!("expected a finished admission failure");
        };
        let Outcome::Platform {
            reason,
            retryable: got,
        } = &outcome
        else {
            panic!("expected a platform failure, got {outcome:?}");
        };
        assert_eq!(reason, want);
        assert_eq!(*got, retryable);
        assert_eq!(
            result_event(&outcome, &Stats::default(), 0)["error"],
            json!(want)
        );
        let plain = format!(
            "sylphx: error: {reason}{}",
            if *got { " [retryable]" } else { "" }
        );
        assert!(plain.contains(want), "{plain}");
    }

    #[test]
    fn accepted_lease_receipts_survive_setup_failure_and_release() {
        let name = "orgs/o/projects/p/envs/e/leases/sbx-example";
        let stats = Stats {
            lease: Some(name.into()),
            ..Stats::default()
        };
        for outcome in [
            Outcome::platform("upload refused", true),
            Outcome::Lost("stream ended".into()),
            Outcome::Ran(1),
            Outcome::TimedOut,
        ] {
            assert_eq!(result_event(&outcome, &stats, 1)["lease"], name);
        }
        assert_eq!(
            with_lease("upload refused", Some(name)),
            "upload refused (lease sbx-example)"
        );
        assert_eq!(
            with_lease(&format!("refused ({name})"), Some(name)),
            format!("refused ({name})")
        );
        assert_eq!(with_lease("create refused", None), "create refused");
        assert!(result_event(
            &Outcome::platform("create refused", true),
            &Stats::default(),
            1
        )
        .get("lease")
        .is_none());
    }
}
