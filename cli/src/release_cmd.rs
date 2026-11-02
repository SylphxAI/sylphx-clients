//! `sylphx deploy`, `sylphx status` and `sylphx rollback`: the linked
//! environment's Releases (Sylphx Release, `release.releases.*`).
//!
//! `deploy` asks for a new Release of a built commit (by default this
//! checkout's `HEAD`; `--redeploy` repeats the live Release's members) and
//! `rollback` for one carrying an earlier served Release's artifacts. Both
//! then follow the Release until it reaches the selected controller-live
//! milestone or ends: exit 0 at that milestone, exit 1 when it fails, is
//! rejected or superseded. `status` prints the live Release and, when a newer
//! one is in flight, that one too.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json, Value};
use sylphx::Client;

use crate::context::{self, Link};
use crate::output::{self, Format};
use crate::{confirm, names, Failure};

/// How often a followed Release is read.
const POLL: Duration = Duration::from_secs(3);

fn env_arg() -> Arg {
    Arg::new("env")
        .long("env")
        .value_name("NAME")
        .help("orgs/{org}/projects/{project}/envs/{env}; default: the linked or key's environment")
}

fn follow_args(cmd: Command) -> Command {
    cmd.arg(
        Arg::new("timeout")
            .long("timeout")
            .value_name("SECONDS")
            .default_value("1800")
            .value_parser(clap::value_parser!(u64).range(1..))
            .help("Stop following the Release after this long (exit 1)"),
    )
}

pub fn commands() -> [Command; 3] {
    [
        follow_args(
            Command::new("deploy")
                .about("Deploy the linked environment: a Release of this checkout's HEAD (or --commit), followed until selected controller-live or failed")
                .arg(env_arg())
                .arg(
                    Arg::new("commit")
                        .long("commit")
                        .value_name("SHA")
                        .conflicts_with("redeploy")
                        .help("The full commit SHA to deploy; it must be pushed and built"),
                )
                .arg(
                    Arg::new("redeploy")
                        .long("redeploy")
                        .action(ArgAction::SetTrue)
                        .help("Deploy the live Release's artifacts again, nothing rebuilt"),
                ),
        ),
        Command::new("status")
            .about("Show the linked environment's live Release and any newer one in flight")
            .arg(env_arg()),
        follow_args(
            Command::new("rollback")
                .about("Roll the linked environment back to the previous served artifacts (or --to), without a build")
                .arg(env_arg())
                .arg(
                    Arg::new("to")
                        .long("to")
                        .value_name("RELEASE")
                        .help("An earlier served Release (rel_… or its full name); default: the newest one that differs from the live Release"),
                ),
        ),
    ]
}

/// The environment the command acts on, as a full name of ids.
async fn environment(
    client: &Client,
    sub: &ArgMatches,
    org: Option<&str>,
) -> Result<String, Failure> {
    if let Some(given) = sub.get_one::<String>("env") {
        if !given.starts_with("orgs/") {
            return Err(Failure::Usage(
                "--env: a full name, orgs/{org}/projects/{project}/envs/{env}".into(),
            ));
        }
        return names::resolve(client, given).await;
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let link = match context::default_link_for(&cwd, org).map_err(Failure::Usage)? {
        Some(l) => l,
        None => Link::from_whoami(&client.invoke("access.whoami", json!({})).await?),
    };
    if link.env.is_empty() {
        return Err(Failure::Usage(
            "no environment: pass --env, or run `sylphx link --env …`".into(),
        ));
    }
    names::resolve(client, &link.env).await
}

fn head_commit() -> Result<String, Failure> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| Failure::Usage(format!("git rev-parse HEAD: {e}; pass --commit SHA")))?;
    if !out.status.success() {
        return Err(Failure::Usage(
            "not a git checkout: pass --commit SHA or --redeploy".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn text<'v>(v: &'v Value, pointer: &str) -> &'v str {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// One Release as lines: its id, state, reason and each service's artifact.
fn describe(release: &Value) -> String {
    let mut out = format!(
        "{}  {}",
        short(text(release, "/name")),
        text(release, "/status/state")
    );
    let reason = text(release, "/status/reason");
    if !reason.is_empty() {
        out.push_str(&format!("  ({reason})"));
    }
    if release["projection_incomplete"] == true {
        out.push_str("\n  Some inputs or serving observations are unavailable; lifecycle is not a traffic check.");
    }
    for service in release
        .get("services")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let source = text(service, "/source_sha");
        out.push_str(&format!(
            "\n  {}  {}  {}",
            text(service, "/service_name"),
            text(service, "/artifact_digest"),
            if source.is_empty() {
                "source unknown"
            } else {
                source
            }
        ));
    }
    if text(release, "/status/state") == "live"
        && text(release, "/status/current_release") == text(release, "/name")
    {
        for host in release
            .pointer("/status/served_hostnames")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            out.push_str(&format!("\n  https://{host}"));
        }
    }
    out
}

fn print(release: &Value, format: Format) {
    if format == Format::Table {
        println!("{}", describe(release));
    } else {
        println!("{}", output::render(release, format));
    }
}

/// Follows `name` to the selected controller-live milestone (Ok) or failure.
async fn follow(
    client: &Client,
    name: &str,
    timeout: Duration,
    format: Format,
) -> Result<(), Failure> {
    let tty = std::io::stderr().is_terminal();
    let started = Instant::now();
    loop {
        let release = client
            .invoke("release.releases.get", json!({"name": name}))
            .await?;
        let state = text(&release, "/status/state");
        let current = text(&release, "/status/current_release") == name;
        let ended = matches!(state, "failed" | "rejected" | "superseded");
        if (state == "live" && current) || ended {
            if tty {
                eprint!("\r");
            }
            print(&release, format);
            return if ended {
                let reason = text(&release, "/status/reason");
                Err(Failure::Refused(format!(
                    "{} {state}: {}",
                    short(name),
                    if reason.is_empty() {
                        "no reason recorded"
                    } else {
                        reason
                    }
                )))
            } else {
                Ok(())
            };
        }
        if started.elapsed() >= timeout {
            return Err(Failure::Refused(format!(
                "{} is still {state} after {}s; follow it with `sylphx status`",
                short(name),
                timeout.as_secs()
            )));
        }
        if tty {
            eprint!("\r{} {state}…", short(name));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The Release a write's done Operation answers.
fn written(op: &Value) -> Result<String, Failure> {
    op.pointer("/response/name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Failure::Refused("the answer names no Release".into()))
}

/// The initial unfiltered page carries the authoritative environment pointer
/// on each record; neither the newest record's identity nor state selects live.
fn current_release(page: &Value) -> &str {
    text(page, "/releases/0/status/current_release")
}

pub async fn run(
    client: &Client,
    verb: &str,
    sub: &ArgMatches,
    org: Option<&str>,
    format: Format,
    yes: bool,
    no_wait: bool,
) -> Result<(), Failure> {
    let env = environment(client, sub, org).await?;
    if verb == "status" {
        let page = client
            .invoke(
                "release.releases.list",
                json!({"parent": env, "page_size": 1}),
            )
            .await?;
        let current = current_release(&page).to_owned();
        let newest = page.pointer("/releases/0").cloned();
        let live = if current.is_empty() {
            None
        } else {
            Some(
                client
                    .invoke("release.releases.get", json!({"name": current}))
                    .await?,
            )
        };
        if format != Format::Table {
            println!(
                "{}",
                output::render(&json!({"live": live, "newest": newest}), format)
            );
            return Ok(());
        }
        match &live {
            Some(release) => println!("live: {}", describe(release)),
            None => println!("live: none"),
        }
        if let Some(newest) = newest.filter(|n| text(n, "/name") != current) {
            println!("newest: {}", describe(&newest));
        }
        return Ok(());
    }
    let op = if verb == "deploy" {
        let commit = match sub.get_one::<String>("commit") {
            Some(c) => c.clone(),
            None if sub.get_flag("redeploy") => String::new(),
            None => head_commit()?,
        };
        client
            .invoke(
                "release.releases.deploy",
                json!({"parent": env, "commit": commit}),
            )
            .await?
    } else {
        let to = match sub.get_one::<String>("to") {
            Some(to) if to.starts_with("orgs/") => names::resolve(client, to).await?,
            Some(to) => format!("{env}/releases/{to}"),
            None => String::new(),
        };
        if !yes {
            confirm(&format!(
                "Roll {} back to {}?",
                short(&env),
                if to.is_empty() {
                    "the previous served Release"
                } else {
                    short(&to)
                }
            ))?;
        }
        client
            .invoke(
                "release.releases.rollback",
                json!({"parent": env, "release": to}),
            )
            .await?
    };
    let name = written(&op)?;
    if no_wait {
        print(op.get("response").unwrap_or(&op), format);
        return Ok(());
    }
    let timeout = Duration::from_secs(*sub.get_one::<u64>("timeout").expect("defaulted"));
    follow(client, &name, timeout, format).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_prints_each_service_and_never_invents_a_source() {
        let release = json!({
            "name": "orgs/o/projects/p/envs/e/releases/rel_1",
            "status": {"state": "failed", "reason": "auto_halt:unavailable:web"},
            "services": [
                {"service_name": "web", "artifact_digest": "sha256:aa", "source_sha": "b4a3"},
                {"service_name": "api", "artifact_digest": "sha256:bb"}
            ]
        });
        assert_eq!(
            describe(&release),
            "rel_1  failed  (auto_halt:unavailable:web)\n  web  sha256:aa  b4a3\n  api  sha256:bb  source unknown"
        );
    }

    #[test]
    fn the_initial_page_pointer_never_infers_live_from_newest() {
        let page = json!({"releases": [{
            "name": "orgs/o/projects/p/envs/e/releases/rel_newest",
            "status": {"state": "rolling_out", "current_release": "orgs/o/projects/p/envs/e/releases/rel_serving"}
        }]});
        assert_eq!(
            current_release(&page),
            "orgs/o/projects/p/envs/e/releases/rel_serving"
        );
        assert_eq!(current_release(&json!({"releases": []})), "");
        assert_eq!(
            current_release(
                &json!({"releases": [{"name": "newest", "status": {"state": "live"}}]})
            ),
            ""
        );
    }

    #[test]
    fn hostnames_print_only_for_the_current_live_release() {
        let mut release = json!({
            "name": "orgs/o/projects/p/envs/e/releases/rel_1",
            "status": {"state": "live", "current_release": "orgs/o/projects/p/envs/e/releases/rel_1",
                "served_hostnames": ["app.example.com", "api.example.com"]}
        });
        assert_eq!(
            describe(&release),
            "rel_1  live\n  https://app.example.com\n  https://api.example.com"
        );
        release["status"]["current_release"] = json!("orgs/o/projects/p/envs/e/releases/rel_2");
        assert_eq!(describe(&release), "rel_1  live");
        release["status"]["current_release"] = release["name"].clone();
        release["status"]["state"] = json!("rolling_out");
        assert_eq!(describe(&release), "rel_1  rolling_out");
    }

    #[test]
    fn a_write_answers_its_release() {
        let op =
            json!({"done": true, "response": {"name": "orgs/o/projects/p/envs/e/releases/rel_2"}});
        assert_eq!(
            written(&op).unwrap(),
            "orgs/o/projects/p/envs/e/releases/rel_2"
        );
        assert!(written(&json!({"done": true})).is_err());
    }

    #[test]
    fn the_three_commands_parse() {
        let mut root = Command::new("sylphx");
        for c in commands() {
            root = root.subcommand(c);
        }
        let m = root
            .clone()
            .try_get_matches_from(["sylphx", "deploy", "--redeploy"])
            .unwrap();
        assert!(m.subcommand_matches("deploy").unwrap().get_flag("redeploy"));
        assert!(root
            .clone()
            .try_get_matches_from(["sylphx", "deploy", "--redeploy", "--commit", "a"])
            .is_err());
        assert!(root
            .try_get_matches_from(["sylphx", "rollback", "--to", "rel_1"])
            .is_ok());
    }
}
