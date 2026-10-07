//! `sylphx work runner`: a self-hosted runner for Sylphx Work (Work ADR 0011
//! D11). It takes steps from a Work workspace through the claim protocol and
//! runs the command it is given for each one, on this machine:
//!
//! 1. a waiting claim (`POST /v2/claim` with `n: 1`, its labels, its runner
//!    name and `wait`) long-polls until a step it covers is ready; the runner
//!    is catalogued by Work on that claim, so it counts as a live runner in
//!    the workspace's coverage while it waits;
//! 2. the command runs with the claim (item, checkpoint, context, profile,
//!    workspace profile, claim token) as JSON on its standard input and in a
//!    file, and the claim token in its environment, never the runner's own key;
//! 3. while it runs, the runner beats the claim at half its time-to-live; a
//!    beat that answers `cancel` stops the command (its whole process group);
//! 4. when it ends, the runner hands the step over: a checkpoint that releases
//!    the claim, unless the command already handed it over with the claim
//!    token (the release then answers a refusal such as `stale_claim`).
//!
//! `--ephemeral` (one step, then exit 0) is the default, as GitHub recommends
//! for self-hosted runners; `--loop` keeps taking steps. Work names no harness
//! or model: the command is whatever the machine's owner configures.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json, Value};

use crate::{context, Failure};

/// Where Work answers when neither `--work-url` nor `SYLPHX_WORK_URL` is set.
const DEFAULT_URL: &str = "https://work.sylphx.com";
/// A waiting claim holds the call up to 110 s on the server.
const CALL_TIMEOUT: Duration = Duration::from_secs(130);
/// The pause after a transient failure (network, 429, 5xx) before trying again.
const RETRY: Duration = Duration::from_secs(5);
/// How long a cancelled command has to stop after SIGTERM before SIGKILL.
const GRACE: Duration = Duration::from_secs(10);
/// How many times the hand-over is tried through transient failures.
const RELEASE_TRIES: usize = 5;

pub fn command() -> Command {
    Command::new("work")
        .about("Sylphx Work: run a workspace's steps on this machine")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("runner")
                .about("A self-hosted runner: take a ready step whose labels this machine covers, run COMMAND for it, beat, and hand it over")
                .long_about("Long-polls Work for a ready step whose required labels --label covers, then runs COMMAND with the claim as JSON on standard input and in $SYLPHX_WORK_CLAIM_FILE, and the claim token (scoped to that one item) in $SYLPHX_WORK_CLAIM_TOKEN; $SYLPHX_WORK_URL, $SYLPHX_WORK_ITEM, $SYLPHX_WORK_GENERATION and $SYLPHX_WORK_RUNNER name the rest, and SYLPHX_API_KEY is removed. A renewed token is written to $SYLPHX_WORK_CLAIM_TOKEN_FILE at each beat. The claim is beaten at half its time-to-live while COMMAND runs; a cancel (the step was dropped, reassigned or revoked) stops COMMAND's process group. When COMMAND ends the runner releases the step with a checkpoint naming its exit status, unless COMMAND handed it over itself.\n\nWith --ephemeral (the default) it takes one step and exits 0 once the step is handed over, whatever COMMAND's exit status (that is recorded on the step); --loop keeps taking steps. Ctrl-C stops COMMAND, hands the step over and exits 130.\n\nExample: sylphx work runner --label kind:build --label member:group:builder -- ./run-step.sh")
                .arg(
                    Arg::new("label")
                        .long("label")
                        .short('l')
                        .value_name("LABEL")
                        .action(ArgAction::Append)
                        .value_delimiter(',')
                        .help("A label this runner covers, e.g. kind:build, member:group:builder or cap:repo-write (repeatable, or comma-separated)"),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .value_name("NAME")
                        .help("The runner's name in the workspace (no slash); default this machine's host name"),
                )
                .arg(
                    Arg::new("ephemeral")
                        .long("ephemeral")
                        .action(ArgAction::SetTrue)
                        .conflicts_with("loop")
                        .help("Take one step, hand it over and exit (the default)"),
                )
                .arg(
                    Arg::new("loop")
                        .long("loop")
                        .action(ArgAction::SetTrue)
                        .help("Keep taking steps, one at a time, until Ctrl-C"),
                )
                .arg(
                    Arg::new("ttl")
                        .long("ttl")
                        .value_name("SECONDS")
                        .value_parser(clap::value_parser!(u64).range(2..=259_200))
                        .help("The claim's time-to-live without a beat; default the step kind's lease (15 minutes for a runner)"),
                )
                .arg(
                    Arg::new("work-url")
                        .long("work-url")
                        .value_name("URL")
                        .help("Work's address; default SYLPHX_WORK_URL, else https://work.sylphx.com"),
                )
                .arg(
                    Arg::new("command")
                        .value_name("COMMAND")
                        .required(true)
                        .num_args(1..)
                        .last(true)
                        .help("The command run for each step, after --"),
                ),
        )
}

/// What one step came to.
#[derive(Debug, Clone, PartialEq)]
enum Ended {
    /// The command exited with this status (`None`: killed by a signal).
    Exited(Option<i32>),
    /// A beat answered cancel, with its reason; the command was stopped.
    Cancelled(String),
    /// Ctrl-C; the command was stopped.
    Interrupted,
}

pub async fn run(api_key: Option<String>, m: &ArgMatches, json_out: bool) -> Result<(), Failure> {
    let Some(("runner", m)) = m.subcommand() else {
        return Err(Failure::Usage(
            "give a subcommand: `sylphx work runner --label <label> -- <command>`".into(),
        ));
    };
    let key = context::resolve(api_key, None)
        .ok_or_else(|| {
            Failure::Usage(
                "not signed in: run `sylphx login`, or give an Access key with SYLPHX_API_KEY or --api-key"
                    .into(),
            )
        })?
        .key;
    let url = m
        .get_one::<String>("work-url")
        .cloned()
        .or_else(|| {
            std::env::var("SYLPHX_WORK_URL")
                .ok()
                .filter(|u| !u.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let name = match m.get_one::<String>("name") {
        Some(n) if n.is_empty() || n.contains('/') || n.chars().any(char::is_whitespace) => {
            return Err(Failure::Usage(format!(
                "--name {n:?}: a runner name has no slash or space"
            )))
        }
        Some(n) => n.clone(),
        None => host_name(),
    };
    let labels: Vec<String> = m
        .get_many::<String>("label")
        .map(|l| l.filter(|l| !l.is_empty()).cloned().collect())
        .unwrap_or_default();
    let argv: Vec<String> = m
        .get_many::<String>("command")
        .expect("required")
        .cloned()
        .collect();
    let runner = Runner {
        work: Work::new(url, key)?,
        name,
        labels,
        ttl: m.get_one::<u64>("ttl").copied(),
        argv,
        json_out,
    };
    let keep_going = m.get_flag("loop");
    loop {
        let taken = tokio::select! {
            t = runner.take() => t?,
            _ = tokio::signal::ctrl_c() => return Err(Failure::Exit(130)),
        };
        let ended = runner.step(&taken).await?;
        if ended == Ended::Interrupted {
            return Err(Failure::Exit(130));
        }
        if !keep_going {
            return Ok(());
        }
    }
}

/// This machine's host name as a runner name: `HOSTNAME`, else
/// `/etc/hostname`, else `runner`, with anything but letters, digits, `.`,
/// `_` and `-` replaced by `-`.
fn host_name() -> String {
    let raw = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "runner".into());
    runner_name(&raw)
}

fn runner_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn op_id() -> String {
    let mut b = [0u8; 12];
    let _ = getrandom::getrandom(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("runner-{hex}")
}

/// A call's failure: a refusal Work answered (4xx other than 429), or
/// something worth trying again (the network, 429, 5xx).
#[derive(Debug)]
enum CallError {
    Refused { code: String, message: String },
    Transient(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Refused { code, message } => write!(f, "{code}: {message}"),
            CallError::Transient(m) => write!(f, "{m}"),
        }
    }
}

/// Work's REST verbs, called with the runner's Access key.
struct Work {
    http: reqwest::Client,
    url: String,
    key: String,
}

impl Work {
    fn new(url: String, key: String) -> Result<Self, Failure> {
        let http = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .user_agent(concat!(
                "sylphx-cli/",
                env!("CARGO_PKG_VERSION"),
                " work-runner"
            ))
            .build()
            .map_err(|e| Failure::Refused(format!("no HTTP client: {e}")))?;
        Ok(Work {
            http,
            url: url.trim_end_matches('/').to_string(),
            key,
        })
    }

    async fn post(&self, verb: &str, body: Value) -> Result<Value, CallError> {
        let r = self
            .http
            .post(format!("{}/v2/{verb}", self.url))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(|e| CallError::Transient(format!("{verb}: {e}")))?;
        let status = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(v);
        }
        let message = v["message"].as_str().unwrap_or(text.as_str()).to_string();
        if status == 429 || status >= 500 {
            return Err(CallError::Transient(format!("{verb}: {status} {message}")));
        }
        Err(CallError::Refused {
            code: v["code"].as_str().unwrap_or("refused").to_string(),
            message: format!("{verb} ({status}): {message}"),
        })
    }
}

struct Runner {
    work: Work,
    name: String,
    labels: Vec<String>,
    ttl: Option<u64>,
    argv: Vec<String>,
    json_out: bool,
}

impl Runner {
    /// Long-polls until one step is taken. A refusal ends the runner; a
    /// transient failure is tried again after a pause.
    async fn take(&self) -> Result<Value, Failure> {
        let mut said_waiting = false;
        loop {
            let mut body = json!({
                "opId": op_id(),
                "n": 1,
                "labels": self.labels,
                "runner": self.name,
                "wait": true,
            });
            if let Some(t) = self.ttl {
                body["ttl"] = json!(t);
            }
            match self.work.post("claim", body).await {
                Ok(v) => {
                    if let Some(t) = v["claims"].as_array().and_then(|c| c.first()) {
                        return Ok(t.clone());
                    }
                    if !said_waiting {
                        eprintln!(
                            "sylphx: runner {} is waiting for a step (labels: {})",
                            self.name,
                            if self.labels.is_empty() {
                                "none".to_string()
                            } else {
                                self.labels.join(",")
                            }
                        );
                        said_waiting = true;
                    }
                }
                Err(CallError::Refused { message, .. }) => {
                    return Err(Failure::Refused(message));
                }
                Err(CallError::Transient(m)) => {
                    eprintln!("sylphx: warning: {m}; trying again in {}s", RETRY.as_secs());
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }

    /// Runs the command for one taken step, beats it, and hands it over.
    async fn step(&self, taken: &Value) -> Result<Ended, Failure> {
        let claim = &taken["claim"];
        let item = claim["item"].as_str().unwrap_or_default().to_string();
        let generation = claim["generation"].as_i64().unwrap_or_default();
        let runner_key = claim["runner"].as_str().unwrap_or(&self.name).to_string();
        let ttl = claim["ttlSeconds"]
            .as_u64()
            .filter(|t| *t > 0)
            .unwrap_or(900);
        let beat_every = Duration::from_secs((ttl / 2).max(1));
        eprintln!("sylphx: took {item} (generation {generation}) as {runner_key}");

        let dir = StepDir::new(&item)?;
        let claim_json = serde_json::to_vec_pretty(taken).unwrap_or_default();
        let claim_file = dir.write("claim.json", &claim_json)?;
        let token = taken["token"].as_str().unwrap_or_default();
        let token_file = dir.write("token", token.as_bytes())?;
        let config_dir = dir.0.join("config");
        create_private_dir(&config_dir).map_err(|e| {
            Failure::Refused(format!("could not create {}: {e}", config_dir.display()))
        })?;

        let mut cmd = tokio::process::Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..])
            .stdin(Stdio::piped())
            .env_remove("SYLPHX_API_KEY")
            .env("SYLPHX_CONFIG_DIR", &config_dir)
            .env("SYLPHX_WORK_URL", &self.work.url)
            .env("SYLPHX_WORK_CLAIM_TOKEN", token)
            .env("SYLPHX_WORK_CLAIM_TOKEN_FILE", &token_file)
            .env("SYLPHX_WORK_CLAIM_FILE", &claim_file)
            .env("SYLPHX_WORK_ITEM", &item)
            .env("SYLPHX_WORK_GENERATION", generation.to_string())
            .env("SYLPHX_WORK_RUNNER", &runner_key)
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let ended = match cmd.spawn() {
            Err(e) => {
                eprintln!("sylphx: could not start {}: {e}", self.argv[0]);
                Ended::Exited(Some(127))
            }
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt;
                    // a command that does not read its input closes the pipe
                    let _ = stdin.write_all(&claim_json).await;
                    drop(stdin);
                }
                self.supervise(&mut child, &item, generation, beat_every, &dir)
                    .await
            }
        };
        self.hand_over(&item, generation, &runner_key, &ended)
            .await?;
        Ok(ended)
    }

    /// Waits for the command, beating the claim; stops it on a cancel or Ctrl-C.
    async fn supervise(
        &self,
        child: &mut tokio::process::Child,
        item: &str,
        generation: i64,
        beat_every: Duration,
        dir: &StepDir,
    ) -> Ended {
        let mut beat =
            tokio::time::interval_at(tokio::time::Instant::now() + beat_every, beat_every);
        loop {
            tokio::select! {
                status = child.wait() => {
                    return Ended::Exited(status.ok().and_then(|s| s.code()));
                }
                _ = beat.tick() => {
                    let body = json!({
                        "opId": op_id(),
                        "claims": [format!("{item}@{generation}")],
                        "freeSlots": 0,
                        "runner": self.name,
                    });
                    match self.work.post("beat", body).await {
                        Ok(v) => {
                            let answer = &v["claims"][0];
                            if answer["cancel"].as_bool() == Some(true) {
                                let reason = answer["reason"].as_str().unwrap_or("revoked").to_string();
                                eprintln!("sylphx: {item} was cancelled ({reason}); stopping the command");
                                stop(child).await;
                                return Ended::Cancelled(reason);
                            }
                            if let Some(t) = answer["token"].as_str() {
                                let _ = dir.write("token", t.as_bytes());
                            }
                        }
                        Err(e) => eprintln!("sylphx: warning: beat failed ({e}); the next one is in {}s", beat_every.as_secs()),
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("sylphx: interrupted; stopping the command and handing {item} over");
                    stop(child).await;
                    return Ended::Interrupted;
                }
            }
        }
    }

    /// Releases the claim with a checkpoint. A refusal means the claim is
    /// no longer this runner's: the command handed it over, or Work ended it.
    async fn hand_over(
        &self,
        item: &str,
        generation: i64,
        runner_key: &str,
        ended: &Ended,
    ) -> Result<(), Failure> {
        let done = match ended {
            Ended::Exited(Some(c)) => format!("the command exited with status {c}"),
            Ended::Exited(None) => "the command was killed by a signal".to_string(),
            Ended::Cancelled(r) => {
                format!("the command was stopped: the claim was cancelled ({r})")
            }
            Ended::Interrupted => "the command was stopped: the runner was interrupted".to_string(),
        };
        let body = json!({
            "opId": op_id(),
            "node": format!("item:{item}"),
            "kind": "checkpoint",
            "text": format!("{runner_key}: {done}."),
            "done": done,
            "where": runner_key,
            "release": true,
            "generation": generation,
        });
        let mut by = "runner";
        let mut tries = 0;
        loop {
            match self.work.post("note", body.clone()).await {
                Ok(_) => break,
                Err(CallError::Refused { code, .. }) => {
                    by = if matches!(ended, Ended::Exited(_)) {
                        "command"
                    } else {
                        "work"
                    };
                    eprintln!("sylphx: {item} was already handed over ({code})");
                    break;
                }
                Err(CallError::Transient(m)) => {
                    tries += 1;
                    if tries >= RELEASE_TRIES {
                        return Err(Failure::Refused(format!(
                            "could not hand {item} over ({m}); its claim lapses at its expiry"
                        )));
                    }
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
        if by == "runner" {
            eprintln!("sylphx: handed {item} over: {done}");
        }
        if self.json_out {
            let (exit, cancelled) = match ended {
                Ended::Exited(c) => (json!(c), Value::Null),
                Ended::Cancelled(r) => (Value::Null, json!(r)),
                Ended::Interrupted => (Value::Null, json!("interrupted")),
            };
            println!(
                "{}",
                json!({ "item": item, "generation": generation, "runner": runner_key,
                        "exitCode": exit, "cancelled": cancelled, "handedOverBy": by })
            );
        }
        Ok(())
    }
}

/// Stops the command and everything it started: SIGTERM to its process
/// group, then SIGKILL after the grace period.
async fn stop(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let group = pid as libc::pid_t;
        // SAFETY: kill(2) with a negative pid signals that process group; the
        // group is the command's own (`process_group(0)` at spawn).
        unsafe {
            libc::kill(-group, libc::SIGTERM);
        }
        if tokio::time::timeout(GRACE, child.wait()).await.is_ok() {
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
            return;
        }
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

/// A private directory for one step's claim and token files, removed after.
struct StepDir(PathBuf);

impl StepDir {
    fn new(item: &str) -> Result<Self, Failure> {
        let safe: String = item
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = std::env::temp_dir().join(format!("sylphx-work-{safe}-{}", op_id()));
        create_private_dir(&dir)
            .map_err(|e| Failure::Refused(format!("could not create {}: {e}", dir.display())))?;
        Ok(StepDir(dir))
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, Failure> {
        let p = self.0.join(name);
        context::write_private(&p, bytes)
            .map_err(|e| Failure::Refused(format!("could not write {}: {e}", p.display())))?;
        Ok(p)
    }
}

impl Drop for StepDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn create_private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(p)
}

#[cfg(not(unix))]
fn create_private_dir(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names_become_runner_names() {
        assert_eq!(runner_name("build box/1"), "build-box-1");
        assert_eq!(runner_name("ci-01.example_a"), "ci-01.example_a");
    }
}
