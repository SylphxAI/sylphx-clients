//! `sylphx build run --no-tree`: the run is a Build on the Build service.
//!
//! The command runs on a build machine the service leases, over no input tree
//! (an uploaded tree follows the store's CAS door). The Build is created with
//! `POST /v1/{parent}/builds`; its output is its log, followed with
//! `:readLogs`; its end is its status. Exit codes are those of the attached
//! run: the command's own, 124 for the time limit, 125 for anything before the
//! command started, 130 when interrupted.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use sylphx::build as bld;
use sylphx::Client;

use super::{Opts, Out, Outcome};
use crate::devices::why;
use crate::Failure;

/// How a remote run ended.
pub enum Ended {
    /// `--no-wait`: the Build was created and printed.
    Created,
    Outcome(Outcome),
}

/// The longest one `:readLogs` call waits for new lines.
const FOLLOW_WAIT: &str = "20s";
/// Pause before reading again after a call failed.
const RETRY: Duration = Duration::from_secs(2);
/// Failed reads in a row before the run is given up as a platform failure.
const MAX_FAILED_READS: u32 = 5;

fn api(what: &str, e: &sylphx::Error) -> Outcome {
    let retryable = match e {
        sylphx::Error::Api {
            retryable, status, ..
        } => *retryable || *status == 501 || *status == 503,
        sylphx::Error::Transport(_) => true,
        _ => false,
    };
    Outcome::Platform {
        reason: format!("{what}: {}", why(e)),
        retryable,
    }
}

/// `orgs/{org}/projects/{project}`: the linked project, else the key's. A
/// Build belongs to a project, so an org-wide or project-wide key runs one
/// (a key pinned to one environment cannot write a project's resources).
pub(super) async fn project(client: &Client) -> Result<String, Outcome> {
    let mut defaults = crate::Defaults { client, link: None };
    defaults
        .at("orgs/{org}/projects/{project}")
        .await
        .map_err(|f| Outcome::Platform {
            reason: match f {
                Failure::Usage(s) | Failure::Refused(s) => s,
                Failure::Api(e) => why(&e),
                Failure::Exit(_) => "stopped".into(),
            },
            retryable: false,
        })
}

pub async fn run(client: &Client, o: &Opts, out: &Out, created: &Mutex<Option<String>>) -> Ended {
    match create_and_follow(client, o, out, created).await {
        Ok(ended) => ended,
        Err(outcome) => Ended::Outcome(outcome),
    }
}

async fn create_and_follow(
    client: &Client,
    o: &Opts,
    out: &Out,
    created: &Mutex<Option<String>>,
) -> Result<Ended, Outcome> {
    let parent = project(client).await?;
    let mut target = bld::CommandTarget::default();
    target.argv = o.command.clone();
    let mut spec = bld::BuildSpec::default();
    spec.command = Some(target);
    spec.size = Some(bld::BuildSize::from_wire(&o.size));
    spec.timeout = format!("{}s", o.timeout.as_secs());
    spec.allow_hosts = o.allow_hosts.clone();
    let mut build = bld::Build::default();
    build.spec = Some(spec);
    let mut req = bld::CreateBuildRequest::default();
    req.parent = parent;
    req.build = Some(build);
    let op = client
        .build()
        .builds()
        .create(req)
        .await
        .map_err(|e| api("creating the build", &e))?;
    // The Operation names its target: the Build.
    let name = op.operation.target.clone();
    if name.is_empty() {
        return Err(Outcome::Platform {
            reason: "the Build service answered an Operation with no target".into(),
            retryable: true,
        });
    }
    if let Ok(mut c) = created.lock() {
        *c = Some(name.clone());
    }
    out.event(json!({"type": "queued", "build": name, "size": o.size}));
    if o.no_wait {
        let b = get(client, &name).await?;
        if o.json {
            out.event(json!({"type": "build", "build": b}));
        } else {
            println!("{name}");
        }
        return Ok(Ended::Created);
    }
    out.progress(&format!("build {name}"));
    follow(client, &name, out, true).await?;
    let b = get(client, &name).await?;
    Ok(Ended::Outcome(outcome_of(&b)))
}

async fn get(client: &Client, name: &str) -> Result<bld::Build, Outcome> {
    let mut req = bld::GetBuildRequest::default();
    req.name = name.to_owned();
    client
        .build()
        .builds()
        .get(req)
        .await
        .map_err(|e| api("reading the build", &e))
}

/// Writes a build's log until it ends (`wait`: follow a running one, else
/// read what is there). Stdout lines go to stdout, the rest to stderr.
pub async fn follow(client: &Client, name: &str, out: &Out, wait: bool) -> Result<(), Outcome> {
    let mut token = String::new();
    let mut failed = 0u32;
    loop {
        let mut req = bld::ReadBuildLogsRequest::default();
        req.name = name.to_owned();
        req.page_token = token.clone();
        if wait {
            req.wait = FOLLOW_WAIT.into();
        }
        let page = match client.build().logs().read(req).await {
            Ok(p) => {
                failed = 0;
                p
            }
            Err(e) => {
                // A broken read is retried; the log is still there.
                failed += 1;
                if failed >= MAX_FAILED_READS {
                    return Err(api("reading the build log", &e));
                }
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        let empty = page.lines.is_empty();
        for l in &page.lines {
            line(out, &l.stream, &l.text);
        }
        if page.next_page_token.is_empty() {
            return Ok(());
        }
        token = page.next_page_token;
        // Without `wait` an empty page is the end of what is written.
        if !wait && empty {
            return Ok(());
        }
    }
}

fn line(out: &Out, stream: &str, text: &str) {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(b'\n');
    match stream {
        "stdout" => out.stdout(&bytes),
        // `sylphx:` lines are the platform's own, like the attached run's.
        "system" => out.progress(text.strip_prefix("sylphx: ").unwrap_or(text)),
        _ => out.stderr(&bytes),
    }
}

/// What a final Build means for the caller.
pub fn outcome_of(b: &bld::Build) -> Outcome {
    let status = b.status.clone().unwrap_or_default();
    let state = status.state.as_ref().map(|s| s.as_str().to_owned());
    let failure = status.failure.as_ref().map(|f| f.as_str().to_owned());
    let exit = status.output.as_ref().map(|o| o.exit_code).unwrap_or(0);
    match (state.as_deref(), failure.as_deref()) {
        (Some("succeeded"), _) => Outcome::Ran(0),
        (Some("cancelled"), _) => Outcome::Interrupted,
        (Some("failed"), Some("timeout")) => Outcome::TimedOut,
        (Some("failed"), Some("build_error")) => Outcome::Ran(if exit == 0 { 1 } else { exit }),
        (Some("failed"), Some(f)) => Outcome::Platform {
            reason: format!("the build failed before its command started ({f}); see its log"),
            retryable: f == "no_capacity" || f == "setup_failed",
        },
        _ => Outcome::Platform {
            reason: "the build did not reach a final state".into(),
            retryable: true,
        },
    }
}

pub async fn cancel(client: &Client, name: &str) {
    let mut req = bld::CancelBuildRequest::default();
    req.name = name.to_owned();
    let r =
        tokio::time::timeout(Duration::from_secs(10), client.build().builds().cancel(req)).await;
    if !matches!(r, Ok(Ok(_))) {
        eprintln!("sylphx: warning: {name} was not cancelled; its time limit ends it");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(state: &str, failure: Option<&str>, exit: i32) -> bld::Build {
        let mut v = json!({"status": {"state": state}});
        if let Some(f) = failure {
            v["status"]["failure"] = json!(f);
        }
        if exit != 0 {
            v["status"]["output"] = json!({"exit_code": exit});
        }
        serde_json::from_value(v).unwrap_or_default()
    }

    #[test]
    fn a_final_build_exits_like_the_attached_run() {
        assert_eq!(outcome_of(&build("succeeded", None, 0)), Outcome::Ran(0));
        assert_eq!(
            outcome_of(&build("failed", Some("build_error"), 3)),
            Outcome::Ran(3)
        );
        assert_eq!(
            outcome_of(&build("failed", Some("timeout"), 0)).code(),
            super::super::EXIT_TIMEOUT
        );
        assert_eq!(
            outcome_of(&build("cancelled", None, 0)).code(),
            super::super::EXIT_INTERRUPTED
        );
        // Anything before the command starts is 125, so the caller builds locally.
        for f in ["no_capacity", "setup_failed", "source_unavailable"] {
            assert_eq!(
                outcome_of(&build("failed", Some(f), 0)).code(),
                super::super::EXIT_PLATFORM,
                "{f}"
            );
        }
        assert_eq!(
            outcome_of(&build("running", None, 0)).code(),
            super::super::EXIT_PLATFORM
        );
    }

    #[test]
    fn a_failed_command_with_no_recorded_code_is_still_not_success() {
        assert_eq!(
            outcome_of(&build("failed", Some("build_error"), 0)),
            Outcome::Ran(1)
        );
    }
}
