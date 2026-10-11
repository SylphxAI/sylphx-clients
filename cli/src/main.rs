//! `sylphx`: the Sylphx CLI (docs/specs/one-platform/resource-api-and-clients.md §8.5).
//!
//! `sylphx <service> <collection> <verb> [NAME|PARENT|ID] [--flags]` is
//! generated from the one schema (`generated/commands.json`) and calls the
//! generated Rust SDK; the porcelain (`login`, `logout`, `whoami`, `link`,
//! `api`, `devices`, `build run`, `build cache env`, `events listen`, `work runner`, `mcp`, `completion`, `ai top`) is hand-written on the
//! same SDK.

mod ai_top;
mod ai_top_run;
mod apply;
mod auth;
mod build_cache;
mod build_run;
mod context;
mod devices;
mod docs_cmd;
mod enable;
mod events_listen;
mod names;
mod output;
mod porcelain;
mod token;
mod tree;
mod work_runner;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json, Map, Value};
use sylphx::{Client, Error, HttpRequest};

use context::Link;

use output::Format;
use tree::{MethodCmd, Tree};

/// Subcommands of a generated service this binary writes itself, as
/// `(service, word)`: each is matched in `run` before the generated tree, and
/// a short command (`MethodPolicy.porcelain`) starting with the same word
/// gives way to it. `build run`, `build image` and `build cache` run on a build
/// machine and mint the cache env locally until Sylphx Build serves its
/// methods; `build logs` prints and follows a served Build's log (the short
/// `build logs read` gives way; `build builds read-logs` stays).
const HAND_WRITTEN: &[(&str, &str)] = &[
    ("auth", "enable"),
    ("auth", "status"),
    ("build", "run"),
    ("build", "logs"),
    ("build", "image"),
    ("build", "cache"),
    ("ai", "top"),
    ("events", "listen"),
    ("data", "docs"),
];

fn cli(tree: &Tree) -> Command {
    let global = |a: Arg| a.global(true);
    let mut cmd = Command::new("sylphx")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Sylphx: one API, one key, every service. https://sylphx.com/docs")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(global(
            Arg::new("output")
                .long("output")
                .short('o')
                .value_name("FORMAT")
                .default_value("table")
                .value_parser(["table", "json", "yaml", "name"])
                .help("Output format; tables show name, Ready, and age"),
        ))
        .arg(global(
            Arg::new("api-key")
                .long("api-key")
                .visible_alias("key")
                .value_name("KEY")
                .help("A Sylphx Access key; defaults to SYLPHX_API_KEY, then `sylphx login`"),
        ))
        .arg(global(
            Arg::new("org")
                .long("org")
                .value_name("ORG")
                .help("Use this organization's credential; with login, lock browser approval to this org"),
        ))
        .arg(global(
            Arg::new("base-url")
                .long("base-url")
                .value_name("URL")
                .hide(true),
        ))
        .arg(global(
            Arg::new("from-file")
                .long("from-file")
                .short('f')
                .value_name("PATH")
                .help("Read the whole request (wire JSON or YAML) from a file; flags override it"),
        ))
        .arg(global(
            Arg::new("no-wait")
                .long("no-wait")
                .action(ArgAction::SetTrue)
                .help("Return a long-running operation at once instead of waiting for it"),
        ))
        .arg(global(
            Arg::new("yes")
                .long("yes")
                .short('y')
                .action(ArgAction::SetTrue)
                .help("Do not ask before destructive calls"),
        ))
        .arg(global(
            Arg::new("all")
                .long("all")
                .action(ArgAction::SetTrue)
                .help("For list: follow every page"),
        ))
        .subcommand(
            Command::new("login")
                .about("Sign in: a browser approval (device flow), or an Access key with --api-key KEY|- (agents, CI)")
                .arg(
                    Arg::new("with-token")
                        .long("with-token")
                        .action(ArgAction::SetTrue)
                        .help("Read an Access key from standard input (same as --api-key -)"),
                )
                .arg(
                    Arg::new("no-verify")
                        .long("no-verify")
                        .action(ArgAction::SetTrue)
                        .help("Store the credential without calling whoami"),
                )
                .arg(
                    Arg::new("store-file")
                        .long("store-file")
                        .action(ArgAction::SetTrue)
                        .help("With no OS keychain, save the key in the default config directory's credentials.json (0600), shared by every process of this user; SYLPHX_CONFIG_DIR picks another directory"),
                ),
        )
        .subcommand(
            Command::new("token")
                .about("Print a short-lived token for one scope, from your login (for cargo and other tools)")
                .long_about("Prints one token on stdout (nothing else) that carries only --scope and expires in 15 minutes. It is minted from your login (or SYLPHX_API_KEY) and cached for reuse until 2 minutes before it expires; the login key itself is never printed. Errors go to stderr with a non-zero exit.\n\nExample, in .cargo/config.toml:\n  credential-provider = [\"cargo:token-from-stdout\", \"sylphx\", \"token\", \"--scope\", \"hosting:deploy\"]")
                .arg(
                    Arg::new("scope")
                        .long("scope")
                        .value_name("SCOPE")
                        .required(true)
                        .help("A registered scope, e.g. hosting:deploy or ai:inference"),
                ),
        )
        .subcommand(apply::command())
        .subcommand(Command::new("logout").about("Revoke and forget the selected org credential (--all: every stored credential)"))
        .subcommand(
            Command::new("org")
                .about("Choose the active organization for commands without an explicit or linked org")
                .subcommand_required(true)
                .subcommand(
                    Command::new("use")
                        .about("Use an organization already approved by sylphx login")
                        .arg(Arg::new("slug").required(true).value_name("SLUG")),
                ),
        )
        .subcommand(
            Command::new("whoami").about("Show the caller: principal, org, project, env, scopes"),
        )
        .subcommand(
            Command::new("link")
                .visible_alias("init")
                .about("Link this directory to an org, project, and env (.sylphx/project.json)")
                .arg(
                    Arg::new("project")
                        .long("project")
                        .value_name("NAME")
                        .help("orgs/{org}/projects/{project}"),
                )
                .arg(
                    Arg::new("env")
                        .long("env")
                        .value_name("NAME")
                        .help("orgs/{org}/projects/{project}/envs/{env}"),
                ),
        )
        .subcommand(
            Command::new("api")
                .about("Call any path of the API directly (raw escape hatch)")
                .arg(
                    Arg::new("method")
                        .required(true)
                        .value_parser(["GET", "POST", "PATCH", "DELETE"]),
                )
                .arg(
                    Arg::new("path")
                        .required(true)
                        .help("Path below the host, e.g. /v1/whoami"),
                )
                .arg(
                    Arg::new("data")
                        .long("data")
                        .short('d')
                        .value_name("JSON|@FILE")
                        .help("Request body"),
                ),
        )
        .subcommand(
            Command::new("mcp")
                .about("Run the Sylphx MCP server over stdio")
                .subcommand(
                    Command::new("setup")
                        .about("Register the Sylphx MCP server with the MCP clients on this machine")
                        .arg(
                            Arg::new("client")
                                .long("client")
                                .value_name("ID")
                                .action(ArgAction::Append)
                                .help("Only this client: claude-code, codex, cursor, vscode, vscode-insiders, claude-desktop, windsurf or gemini (repeatable)"),
                        )
                        .arg(
                            Arg::new("remove")
                                .long("remove")
                                .action(ArgAction::SetTrue)
                                .help("Remove the entry instead of adding it"),
                        )
                        .arg(
                            Arg::new("dry-run")
                                .long("dry-run")
                                .action(ArgAction::SetTrue)
                                .help("Print the changes without writing them"),
                        ),
                ),
        )
        .subcommand(devices::command())
        .subcommand(porcelain::command())
        .subcommand(work_runner::command())
        .subcommand(
            Command::new("completion")
                .about("Print a shell completion script")
                .arg(
                    Arg::new("shell")
                        .required(true)
                        .value_parser(clap::value_parser!(clap_complete::Shell)),
                ),
        );
    for s in tree::service_commands(tree, HAND_WRITTEN) {
        // Enable Auth is porcelain on the generated `auth` service: it binds
        // Sylphx Auth to an environment through the composition route.
        let s = if s.get_name() == "auth" {
            s.subcommand(
                Command::new("enable")
                    .about("Enable Sylphx Auth on an environment (idempotent); prints its instance")
                    .arg(
                        Arg::new("env")
                            .long("env")
                            .value_name("NAME|ID")
                            .help("orgs/{org}/projects/{project}/envs/{env}, or an environment id in the linked project; default: the linked or key's environment"),
                    ),
            )
            .subcommand(
                Command::new("status")
                    .about("Show whether Sylphx Auth is enabled, and its instance"),
            )
        } else if s.get_name() == "build" {
            // `build run` is porcelain on the generated `build` service: a
            // command on a remote build machine (`build_run/`).
            s.subcommand(build_run::command())
                .subcommand(build_run::logs_command())
                .subcommand(build_run::image_command())
                .subcommand(build_cache::command())
        } else if s.get_name() == "data" {
            // `data docs` is porcelain on the generated `data` service: a
            // Markdown tree in a search index (`docs_cmd`).
            s.subcommand(docs_cmd::command())
        } else if s.get_name() == "events" {
            // `events listen` is porcelain on the generated `events` service:
            // a topic relayed to a local URL through a temporary Queue.
            s.subcommand(events_listen::command())
        } else if s.get_name() == "ai" {
            s.subcommand(
                Command::new("top")
                    .about("Live view of the AI seats: usable and spent, resets, runway, accounts needed, what needs a person")
                    .long_about("Reads GET /v1/operator/seats (a platform key with ai:operator:seats:read; --base-url points it at the gateway) and shows usable and spent seats, the earliest reset, runway and seats needed at the 24h and 6h pace, and red lines for anything that needs a person (login, on hold, alarm). The pace is measured from weekly-window readings this command keeps in its config directory, so it appears after about 6 hours of use. Sessions, subagents and API-equivalent value show n/a until the gateway receipts carry them.\n\nOn a terminal it refreshes until Ctrl-C; --once or a pipe prints once; --json (or -o json) prints one JSON document.")
                    .arg(
                        Arg::new("once")
                            .long("once")
                            .action(ArgAction::SetTrue)
                            .help("Print once and exit (the default when stdout is not a terminal)"),
                    )
                    .arg(
                        Arg::new("json")
                            .long("json")
                            .action(ArgAction::SetTrue)
                            .help("Print one JSON document and exit (same as -o json)"),
                    )
                    .arg(
                        Arg::new("interval")
                            .long("interval")
                            .value_name("SECONDS")
                            .default_value("10")
                            .value_parser(clap::value_parser!(u64).range(1..))
                            .help("Seconds between refreshes in live mode"),
                    ),
            )
        } else {
            s
        };
        cmd = cmd.subcommand(s);
    }
    cmd
}

#[tokio::main]
async fn main() -> ExitCode {
    let tree = tree::load();
    let matches = cli(&tree).get_matches();
    match run(&tree, &matches).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(e)) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
        Err(Failure::Refused(e)) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
        Err(Failure::Api(e)) => {
            eprintln!("{}", describe(&e));
            ExitCode::from(1)
        }
        Err(Failure::Exit(code)) => ExitCode::from(code),
    }
}

enum Failure {
    Usage(String),
    /// A one-line reason on stderr, exit 1.
    Refused(String),
    Api(Error),
    /// The command ran to a verdict and printed its own report; the number is
    /// its exit status (`sylphx devices run`).
    Exit(u8),
}

impl From<String> for Failure {
    fn from(s: String) -> Self {
        Failure::Usage(s)
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Api(e)
    }
}

fn describe(e: &Error) -> String {
    match e {
        Error::Api {
            code,
            status,
            detail,
            request_id,
            retryable,
            ..
        } => {
            let mut s = format!("error: {} ({status}): {detail}", code.as_str());
            if *retryable {
                s.push_str(" [retryable]");
            }
            if let Some(id) = request_id {
                s.push_str(&format!("\nrequest id: {id}"));
            }
            s
        }
        other => format!("error: {other}"),
    }
}

async fn run(tree: &Tree, m: &ArgMatches) -> Result<(), Failure> {
    let format = Format::parse(
        m.get_one::<String>("output")
            .map(String::as_str)
            .unwrap_or("table"),
    )?;
    let api_key = m.get_one::<String>("api-key").cloned();
    let base_url = m.get_one::<String>("base-url").cloned();
    let org = m
        .get_one::<String>("org")
        .map(String::as_str)
        .or_else(|| command_org(m));
    let client = || async {
        context::client(api_key.clone(), base_url.clone(), org)
            .await?
            .ok_or_else(|| {
                Failure::Usage(
                    "not signed in: run `sylphx login` (browser), or give an Access key with \
                     SYLPHX_API_KEY=sylphx_sk_… or `sylphx login --api-key -` (stdin)"
                        .into(),
                )
            })
    };
    let (name, sub) = m.subcommand().expect("subcommand_required");
    match name {
        "apply" => apply::run(sub, api_key, base_url, org, format).await,
        "login" => {
            login(
                api_key.clone(),
                base_url.clone(),
                sub.get_flag("with-token"),
                sub.get_one::<String>("org").cloned(),
                sub.get_flag("no-verify"),
                sub.get_flag("store-file"),
            )
            .await
        }
        "token" => {
            let scope = sub.get_one::<String>("scope").expect("required");
            let t = token::token(scope, api_key.clone(), base_url.clone(), org)
                .await
                .map_err(Failure::Refused)?;
            println!("{t}");
            Ok(())
        }
        "org" => {
            let (_, use_) = sub.subcommand().expect("subcommand_required");
            let slug = use_.get_one::<String>("slug").expect("required");
            context::use_org(slug).map_err(Failure::Usage)?;
            println!("Active organization: {slug}.");
            Ok(())
        }
        "logout" => {
            let creds = context::load_credentials();
            let all = m.get_flag("all");
            let legacy = org.is_none() && creds.active_org.is_none() && creds.store.is_some();
            let selected = if all || legacy {
                None
            } else {
                let linked = std::env::current_dir()
                    .ok()
                    .and_then(|cwd| context::default_link(&cwd).ok().flatten());
                context::selected_org(
                    &creds,
                    org.or_else(|| linked.as_ref().and_then(|l| l.at(1))),
                )
                .map(|o| o.id.clone())
            };
            let mut keys = Vec::new();
            for entry in &creds.orgs {
                if all || selected.as_deref() == Some(&entry.id) {
                    if let Some(key) = context::org_key(&creds, entry) {
                        keys.push(key);
                    }
                }
            }
            if all || legacy || creds.orgs.is_empty() {
                if let Some(key) = context::stored_key(&creds) {
                    keys.push(key);
                }
            }
            let url = base_url
                .clone()
                .or_else(|| std::env::var("SYLPHX_BASE_URL").ok())
                .or(creds.base_url.clone());
            for key in &keys {
                if key.starts_with("sylphx_sk_") {
                    if let Err(e) = auth::revoke_self(&auth::api_root(url.as_deref()), key).await {
                        eprintln!("warning: could not revoke a key server-side ({e}); revoke it in Settings → API keys");
                    }
                }
            }
            if all || creds.orgs.is_empty() {
                context::forget(&creds)?;
            } else if legacy {
                context::forget_legacy(&creds)?;
            } else if let Some(id) = selected {
                context::forget_org(&creds, &id)?;
            } else {
                return Err(Failure::Usage(
                    "no stored credential for the selected organization".into(),
                ));
            }
            println!("Logged out: stored credentials removed; server revocation attempted.");
            Ok(())
        }
        "whoami" => {
            let me = client().await?.invoke("access.whoami", json!({})).await?;
            println!(
                "{}",
                output::render(
                    &me,
                    if format == Format::Table {
                        Format::Yaml
                    } else {
                        format
                    }
                )
            );
            Ok(())
        }
        "link" => link(&client().await?, sub).await,
        "devices" => devices::run(&client().await?, sub).await,
        "work" => work_runner::run(api_key.clone(), org, sub, format == Format::Json).await,
        "api" => {
            let method = sub.get_one::<String>("method").expect("required").clone();
            let path = sub.get_one::<String>("path").expect("required").clone();
            let body = sub
                .get_one::<String>("data")
                .map(|d| read_json_arg(d))
                .transpose()?;
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            };
            let (path, query) = match path.split_once('?') {
                Some((p, q)) => (
                    p.to_string(),
                    q.split('&')
                        .filter_map(|kv| {
                            kv.split_once('=')
                                .map(|(k, v)| (k.to_string(), v.to_string()))
                        })
                        .collect(),
                ),
                None => (path, vec![]),
            };
            let method: &'static str = match method.as_str() {
                "GET" => "GET",
                "POST" => "POST",
                "PATCH" => "PATCH",
                _ => "DELETE",
            };
            let v: Value = client()
                .await?
                .call(HttpRequest {
                    method,
                    path,
                    query,
                    body,
                    mutation: method != "GET",
                    origin: None,
                    effect_ids: false,
                })
                .await?;
            println!(
                "{}",
                output::render(
                    &v,
                    if format == Format::Table {
                        Format::Json
                    } else {
                        format
                    }
                )
            );
            Ok(())
        }
        "mcp" if sub.subcommand_matches("setup").is_some() => {
            let setup = sub.subcommand_matches("setup").expect("matched above");
            let options = sylphx_mcp::SetupOptions {
                dry_run: setup.get_flag("dry-run"),
                remove: setup.get_flag("remove"),
                clients: setup
                    .get_many::<String>("client")
                    .map(|c| c.cloned().collect()),
            };
            sylphx_mcp::setup(&options)
                .map(|_| ())
                .map_err(|e| Failure::Usage(e.to_string()))
        }
        "mcp" => {
            let c = context::client(api_key.clone(), base_url.clone(), org).await?;
            sylphx_mcp::serve_stdio(
                sylphx_mcp::Server::new(c).with_docs(sylphx_mcp::docs::DocsIndex::from_env()),
            )
            .await
            .map_err(|e| Failure::Usage(e.to_string()))
        }
        porcelain::COMMAND => {
            let manifest = porcelain::manifest(&cli(tree), tree);
            println!(
                "{}",
                serde_json::to_string_pretty(&manifest).expect("a JSON value serializes")
            );
            Ok(())
        }
        "completion" => {
            let shell = *sub
                .get_one::<clap_complete::Shell>("shell")
                .expect("required");
            let mut cmd = cli(tree);
            clap_complete::generate(shell, &mut cmd, "sylphx", &mut std::io::stdout());
            Ok(())
        }
        "build" if sub.subcommand_name() == Some("run") => {
            let (_, rm) = sub.subcommand().expect("checked");
            let key = context::resolve(api_key.clone(), base_url.clone(), org).map(|r| r.key);
            build_run::run(client().await, key, rm).await
        }
        "build" if sub.subcommand_name() == Some("logs") => {
            let (_, lm) = sub.subcommand().expect("checked");
            build_run::logs(client().await, lm).await
        }
        "build" if sub.subcommand_name() == Some("image") => {
            let (_, im) = sub.subcommand().expect("checked");
            let key = context::resolve(api_key.clone(), base_url.clone(), org).map(|r| r.key);
            build_run::run_image(client().await, key, im).await
        }
        "build" if sub.subcommand_name() == Some("cache") => {
            let (_, cm) = sub.subcommand().expect("checked");
            let creds = context::resolve(api_key.clone(), base_url.clone(), org);
            build_cache::run(creds, cm, format).await
        }
        "ai" if sub.subcommand_name() == Some("top") => {
            let (_, tm) = sub.subcommand().expect("checked");
            ai_top_run::run(
                &client().await?,
                tm.get_flag("json") || format == Format::Json,
                tm.get_flag("once"),
                *tm.get_one::<u64>("interval").expect("defaulted"),
            )
            .await
            .map_err(|e| match e {
                ai_top_run::TopError::Api(e) => Failure::Api(e),
                ai_top_run::TopError::Msg(m) => Failure::Refused(m),
            })
        }
        "data" if sub.subcommand_name() == Some("docs") => {
            let (_, dm) = sub.subcommand().expect("checked");
            docs_cmd::run(&client().await?, dm, format).await
        }
        "events" if sub.subcommand_name() == Some("listen") => {
            let (_, lm) = sub.subcommand().expect("checked");
            events_listen::run(&client().await?, lm, format == Format::Json).await
        }
        "auth" if matches!(sub.subcommand_name(), Some("enable" | "status")) => {
            let (verb, vm) = sub.subcommand().expect("checked");
            auth_enable(&client().await?, verb, vm, format).await
        }
        service => {
            let (method, mm) = tree::find(tree, service, sub)
                .ok_or_else(|| Failure::Usage(format!("unknown command under `{service}`")))?;
            if matches!(
                method.method.as_str(),
                "workflows.schedules.list" | "workflows.schedules.get"
            ) {
                return compute_schedules(api_key, base_url, org, method, &mm, format).await;
            }
            generated(&client().await?, method, &mm, m, format).await
        }
    }
}

/// A full resource name also selects its organization credential.
fn command_org(matches: &ArgMatches) -> Option<&str> {
    if let Some((_, sub)) = matches.subcommand() {
        if let Some(org) = command_org(sub) {
            return Some(org);
        }
    }
    for id in matches.ids() {
        if let Some(values) = matches.get_raw(id.as_str()) {
            for value in values {
                if let Some(name) = value.to_str().filter(|v| v.starts_with("orgs/")) {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// `sylphx workflows schedules list|get`: the schedules a manifest declares
/// (`[[compute.schedules]]`) live in Compute, which answers them at
/// `/v1/schedules` for the key's own project, with the last tick and the
/// dead letters. The platform api's `schedules` collection only holds the
/// retired cron rows, so these two verbs read Compute.
async fn compute_schedules(
    api_key: Option<String>,
    base_url: Option<String>,
    org: Option<&str>,
    method: &MethodCmd,
    mm: &ArgMatches,
    format: Format,
) -> Result<(), Failure> {
    let url = std::env::var("SYLPHX_COMPUTE_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .or(base_url)
        .or_else(|| {
            std::env::var("SYLPHX_BASE_URL")
                .ok()
                .filter(|u| !u.is_empty())
        })
        .unwrap_or_else(|| "https://api.compute.sylphx.com".to_string());
    let client = context::client(api_key, Some(url), org)
        .await?
        .ok_or_else(|| {
            Failure::Usage(
                "not signed in: run `sylphx login`, or give an Access key with SYLPHX_API_KEY"
                    .into(),
            )
        })?;
    let (path, query) = if method.verb == "get" {
        let id = mm.get_one::<String>("__positional").ok_or_else(|| {
            Failure::Usage("name the schedule: sylphx workflows schedules get <id>".into())
        })?;
        (format!("/v1/schedules/{}", enable::bare_id(id)), vec![])
    } else {
        let (values, _) = tree::flag_values(method, mm)?;
        let mut query = vec![];
        for (field, value) in values {
            let key = match field.as_str() {
                "page_size" => "limit",
                "page_token" => "cursor",
                _ => continue,
            };
            let text = match &value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            query.push((key.to_string(), text));
        }
        ("/v1/schedules".to_string(), query)
    };
    let v: Value = client
        .call(HttpRequest {
            method: "GET",
            path,
            query,
            body: None,
            mutation: false,
            origin: None,
            effect_ids: false,
        })
        .await?;
    let text = output::render(&v, format);
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

/// `sylphx auth enable [--env]` and `sylphx auth status`.
async fn auth_enable(
    client: &Client,
    verb: &str,
    sub: &ArgMatches,
    format: Format,
) -> Result<(), Failure> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let org = sub
        .try_get_one::<String>("org")
        .ok()
        .flatten()
        .map(String::as_str)
        .or_else(|| command_org(sub));
    let link = match context::default_link_for(&cwd, org).map_err(Failure::Usage)? {
        Some(l) => l,
        None => Link::from_whoami(&client.invoke("access.whoami", json!({})).await?),
    };
    // Full names may carry slugs; resolve them to ids first.
    let given = sub.try_get_one::<String>("env").ok().flatten().cloned();
    let (project, env) = match given {
        Some(e) if e.starts_with("orgs/") => {
            let full = names::resolve(client, &e).await?;
            let project = full.split("/envs/").next().unwrap_or_default().to_string();
            (project, full)
        }
        Some(e) => (link.project.clone(), e),
        None => (link.project.clone(), link.env.clone()),
    };
    if project.is_empty() || env.is_empty() {
        return Err(Failure::Usage(
            "no environment: pass --env, or run `sylphx link --env …`".into(),
        ));
    }
    let path = enable::bindings_path(&project);
    let render = |v: &Value| {
        output::render(
            v,
            if format == Format::Table {
                Format::Yaml
            } else {
                format
            },
        )
    };
    if verb == "status" {
        let list: Value = client
            .call(HttpRequest {
                method: "GET",
                path,
                query: vec![],
                body: None,
                mutation: false,
                origin: None,
                effect_ids: false,
            })
            .await?;
        println!("{}", render(&enable::status(&list)));
        return Ok(());
    }
    let answer: Value = client
        .call(HttpRequest {
            method: "PUT",
            path,
            query: vec![],
            body: Some(enable::enable_body(&env)),
            mutation: true,
            origin: None,
            effect_ids: false,
        })
        .await?;
    let instance = enable::organization_id(&answer).ok_or_else(|| {
        Failure::Refused("Auth was bound but the answer names no instance".into())
    })?;
    if format == Format::Table {
        println!("Sylphx Auth is enabled on {}.", enable::bare_id(&env));
        println!("Instance (SYLPHX_AUTH_ORGANIZATION_ID): {instance}");
    } else {
        println!(
            "{}",
            render(&json!({"organizationId": instance, "binding": answer}))
        );
    }
    Ok(())
}

async fn login(
    api_key: Option<String>,
    base_url: Option<String>,
    with_token: bool,
    org: Option<String>,
    no_verify: bool,
    store_file: bool,
) -> Result<(), Failure> {
    let base_url = base_url
        .or_else(|| {
            std::env::var("SYLPHX_BASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| context::load_credentials().base_url);
    // Where the key will be kept is settled before a key is read or issued:
    // a device key minted and then dropped would be a live key nobody holds.
    context::check_key_store(base_url.as_deref(), store_file).map_err(Failure::Usage)?;
    // A key is read from stdin when asked for (`--api-key -`, `--with-token`)
    // or piped in; an empty stdin that was not asked for (a script, an agent
    // shell) falls through to the device flow.
    let asked = api_key.as_deref() == Some("-") || (api_key.is_none() && with_token);
    let piped = api_key.is_none() && !std::io::stdin().is_terminal();
    let key = if asked || piped {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| e.to_string())?;
        let s = s.trim().to_string();
        (asked || !s.is_empty()).then_some(s)
    } else {
        api_key
    };
    let issued_here = key.is_none();
    let (keys, active_after) = match key {
        Some(k) if k.is_empty() => return Err(Failure::Usage("no key on stdin".into())),
        Some(k) => {
            if !k.starts_with("sylphx_sk_") && !k.starts_with("sylphx_pk_") {
                eprintln!("warning: an Access key starts with sylphx_sk_ (secret) or sylphx_pk_ (publishable)");
            }
            (
                vec![auth::IssuedKey {
                    api_key: k,
                    org_id: String::new(),
                    org_slug: String::new(),
                    key_name: String::new(),
                }],
                std::time::Duration::ZERO,
            )
        }
        None => {
            let issued = auth::device_login(&auth::api_root(base_url.as_deref()), org).await?;
            for key in &issued.keys {
                eprintln!("Approved in {}.", key.org_slug);
            }
            (issued.keys, issued.active_after)
        }
    };
    // Device-issued keys must survive temporary verification failures. Supplied
    // keys are verified before storage, so a rejected key cannot erase a login.
    let stored = if issued_here {
        Some(context::store_keys(base_url.clone(), &keys, store_file)?)
    } else {
        None
    };
    if !no_verify {
        tokio::time::sleep(active_after).await;
    }
    let mut principal = None;
    let mut unconfirmed = false;
    if !no_verify {
        for key in &keys {
            match verify_login_key(&key.api_key, base_url.clone(), issued_here).await {
                Ok(me) => {
                    principal = me["principal"]
                        .as_str()
                        .or_else(|| me["email"].as_str())
                        .or_else(|| me["user"]["email"].as_str())
                        .map(str::to_string)
                        .or(principal);
                }
                Err((e, transient)) if issued_here || transient => {
                    unconfirmed = true;
                    eprintln!("A key could not be confirmed yet ({e}). Run `sylphx whoami` in a minute; there is no need to sign in again.");
                }
                Err((e, _)) => return Err(e.into()),
            }
        }
    }
    let (path, in_keychain) = match stored {
        Some(stored) => stored,
        None => context::store_key(base_url, &keys[0].api_key, None, store_file)?,
    };
    let where_ = if in_keychain {
        "the OS keychain".to_string()
    } else {
        eprintln!(
            "note: no OS keychain here; credentials are in {} (mode 0600)",
            path.display()
        );
        path.display().to_string()
    };
    if no_verify || unconfirmed {
        println!("Credentials stored in {where_}.");
    } else {
        println!(
            "Signed in as {} (key stored in {where_}).",
            principal.as_deref().unwrap_or("?")
        );
    }
    Ok(())
}

async fn verify_login_key(
    key: &str,
    base_url: Option<String>,
    issued_here: bool,
) -> Result<Value, (Error, bool)> {
    let client = context::client_for(
        &context::resolve(Some(key.to_string()), base_url, None)
            .expect("an explicit credential resolves"),
    )
    .map_err(|e| (Error::Transport(e), false))?;
    let mut waits = [500u64, 1000, 2000, 4000, 8000].into_iter();
    loop {
        match client.invoke("access.whoami", json!({})).await {
            Err(e) if transient_whoami(&e) || (issued_here && unknown_key(&e)) => {
                match waits.next() {
                    Some(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                    None => return Err((e, true)),
                }
            }
            Err(e) => return Err((e, false)),
            Ok(me) => return Ok(me),
        }
    }
}

fn unknown_key(e: &Error) -> bool {
    matches!(e, Error::Api { detail, .. } if detail.contains("unknown_key"))
}

/// A `whoami` failure worth retrying right after login: the key is not yet
/// verifiable where the call landed, or the network or server was briefly
/// unavailable.
fn transient_whoami(e: &Error) -> bool {
    match e {
        Error::Api {
            code,
            status,
            detail,
            ..
        } => {
            *code == sylphx::common::ErrorCode::NotYetPropagated
                || detail.contains("not_yet_propagated")
                || *status == 429
                || *status >= 500
        }
        Error::Transport(_) => true,
        _ => false,
    }
}

async fn link(client: &Client, sub: &ArgMatches) -> Result<(), Failure> {
    let me = client.invoke("access.whoami", json!({})).await?;
    let mine = Link::from_whoami(&me);
    let org = sub
        .get_one::<String>("org")
        .cloned()
        .unwrap_or(mine.org.clone());
    if org.is_empty() {
        return Err(Failure::Usage("the key names no org; pass --org".into()));
    }
    let project = sub.get_one::<String>("project").cloned();
    let env = sub.get_one::<String>("env").cloned();
    // Flags may be slugs, ids, or full names; the link stores full names.
    let l = if project.is_none() && env.is_none() && sub.get_one::<String>("org").is_none() {
        mine
    } else {
        let project = project.or_else(|| (!mine.project.is_empty()).then(|| mine.project.clone()));
        let (org, project, env) =
            names::link_names(client, &org, project.as_deref(), env.as_deref()).await?;
        Link {
            org,
            project: project.unwrap_or_default(),
            env: env.unwrap_or_default(),
        }
    };
    let dir = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = context::write_link(&dir, &l)?;
    println!(
        "Linked {} to {}.",
        dir.display(),
        if !l.env.is_empty() {
            &l.env
        } else if !l.project.is_empty() {
            &l.project
        } else {
            &l.org
        }
    );
    println!("Wrote {}.", p.display());
    Ok(())
}

fn read_json_arg(s: &str) -> Result<Value, String> {
    let text = match s.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?,
        None => s.to_string(),
    };
    serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))
}

/// Defaults for names and parents: `SYLPHX_ENVIRONMENT`, else the linked project, else
/// the key's scope.
struct Defaults<'c> {
    client: &'c Client,
    link: Option<Link>,
    org: Option<String>,
}

impl Defaults<'_> {
    async fn at(&mut self, pattern: &str) -> Result<String, Failure> {
        let level = tree::context_level(pattern).ok_or_else(|| {
            Failure::Usage(format!("give the parent explicitly (it matches {pattern})"))
        })?;
        if self.link.is_none() {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            self.link = Some(
                match context::default_link_for(&cwd, self.org.as_deref())
                    .map_err(Failure::Usage)?
                {
                    Some(l) => l,
                    None => {
                        Link::from_whoami(&self.client.invoke("access.whoami", json!({})).await?)
                    }
                },
            );
        }
        self.link
            .as_ref()
            .and_then(|l| l.at(level))
            .map(str::to_string)
            .ok_or_else(|| {
                Failure::Usage(format!(
                    "no default for {pattern}: pass it explicitly or run `sylphx link --env …`"
                ))
            })
    }

    /// A full name from a full name or a bare id below the default parent.
    async fn name(&mut self, value: &str, pattern: &str) -> Result<String, Failure> {
        if value.starts_with("orgs/") {
            // A full name may carry project and environment slugs.
            return names::resolve(self.client, value).await;
        }
        if value.contains('/') || !pattern.contains('/') {
            return Ok(value.to_string());
        }
        let (parent, collection) = tree::split_pattern(pattern)
            .ok_or_else(|| Failure::Usage(format!("`{value}` must be a full name ({pattern})")))?;
        Ok(format!("{}/{collection}/{value}", self.at(&parent).await?))
    }
}

/// A singleton's name: its parent and its literal segment; any other
/// PARENT is the name as is.
fn singleton_name(parent: String, suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("{parent}/{s}"),
        None => parent,
    }
}

async fn generated(
    client: &Client,
    method: &MethodCmd,
    mm: &ArgMatches,
    root: &ArgMatches,
    format: Format,
) -> Result<(), Failure> {
    let mut args = match root.get_one::<String>("from-file") {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            let v: Value = if path.ends_with(".json") {
                serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?
            } else {
                serde_yaml_ng::from_str(&text).map_err(|e| format!("{path}: {e}"))?
            };
            match v {
                Value::Object(o) => o,
                _ => {
                    return Err(Failure::Usage(format!(
                        "{path}: the request must be an object"
                    )))
                }
            }
        }
        None => Map::new(),
    };
    let mut defaults = Defaults {
        client,
        link: None,
        org: root.get_one::<String>("org").cloned(),
    };

    if let Some(p) = &method.positional {
        let given = mm.get_one::<String>("__positional").cloned();
        let value = match (p.arg.as_str(), given) {
            ("ID", v) => v,
            ("PARENT", Some(v)) => Some(singleton_name(
                names::resolve(client, &v).await?,
                p.suffix.as_deref(),
            )),
            ("PARENT", None)
                if tree::get_path(&Value::Object(args.clone()), &p.field).is_none() =>
            {
                Some(singleton_name(
                    defaults
                        .at(p.pattern.as_deref().unwrap_or_default())
                        .await?,
                    p.suffix.as_deref(),
                ))
            }
            (_, Some(v)) => Some(
                defaults
                    .name(&v, p.pattern.as_deref().unwrap_or_default())
                    .await?,
            ),
            _ => None,
        };
        if let Some(v) = value {
            tree::set_path(&mut args, &p.field, Value::String(v));
        }
    }
    let (values, mask) = tree::flag_values(method, mm)?;
    for (field, value) in values {
        tree::set_path(&mut args, &field, value);
    }
    // `--parent` defaults from context.
    for f in method.flags.iter().filter(|f| f.default_from_context) {
        if tree::get_path(&Value::Object(args.clone()), &f.field).is_none() {
            let v = defaults
                .at(f.pattern.as_deref().unwrap_or_default())
                .await?;
            tree::set_path(&mut args, &f.field, Value::String(v));
        }
    }
    if method.is_update() && !mask.is_empty() && !args.contains_key("update_mask") {
        args.insert("update_mask".into(), Value::String(mask.join(",")));
    }
    let missing = tree::missing_required(method, &Value::Object(args.clone()));
    if !missing.is_empty() {
        return Err(Failure::Usage(format!(
            "missing required flags: {}",
            missing.join(", ")
        )));
    }

    // Updates and deletes send the etag they read (§3.5), unless one is given.
    if method.sends_etag || method.flags.iter().any(|f| f.field == "etag") {
        etag(client, method, &mut args).await?;
    }

    if method.effect == "destructive" && !root.get_flag("yes") {
        let snapshot = Value::Object(args.clone());
        let target = method
            .positional
            .as_ref()
            .and_then(|p| tree::get_path(&snapshot, &p.field))
            .and_then(Value::as_str)
            .unwrap_or(&method.method)
            .to_string();
        confirm(&format!("{} {target}?", method.verb))?;
    }

    let mut result = client
        .invoke(&method.method, Value::Object(args.clone()))
        .await?;
    if method.paginates && root.get_flag("all") {
        result = follow_pages(client, method, args, result).await?;
    }
    if method.long_running() && !root.get_flag("no-wait") && result.get("validate_only").is_none() {
        result = wait(client, result).await?;
    }
    let text = output::render(&result, format);
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

/// Reads the Resource first and sends its etag with the write.
async fn etag(
    client: &Client,
    method: &MethodCmd,
    args: &mut Map<String, Value>,
) -> Result<(), Failure> {
    let Some(get) = method
        .method
        .rsplit_once('.')
        .map(|(c, _)| format!("{c}.get"))
    else {
        return Ok(());
    };
    let (name_field, etag_path) = match method.resource_field() {
        Some(r) if method.is_update() => (format!("{r}.name"), format!("{r}.meta.etag")),
        _ => ("name".to_string(), "etag".to_string()),
    };
    let current = Value::Object(args.clone());
    if tree::get_path(&current, &etag_path).is_some() || sylphx::methods::method(&get).is_none() {
        return Ok(());
    }
    let Some(name) = tree::get_path(&current, &name_field)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    match client.invoke(&get, json!({ "name": name })).await {
        Ok(r) => {
            if let Some(e) = r
                .get("meta")
                .and_then(|m| m.get("etag"))
                .and_then(Value::as_str)
            {
                tree::set_path(args, &etag_path, Value::String(e.to_string()));
            }
            Ok(())
        }
        // An upsert of a missing Resource has no etag to send.
        Err(Error::Api { status: 404, .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn confirm(prompt: &str) -> Result<(), Failure> {
    if !std::io::stdin().is_terminal() {
        return Err(Failure::Usage(format!(
            "{prompt} This is destructive: pass --yes to confirm"
        )));
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if matches!(line.trim(), "y" | "Y" | "yes") {
        Ok(())
    } else {
        Err(Failure::Usage("aborted".into()))
    }
}

async fn follow_pages(
    client: &Client,
    method: &MethodCmd,
    mut args: Map<String, Value>,
    first: Value,
) -> Result<Value, Failure> {
    let key = first
        .as_object()
        .and_then(|o| {
            o.iter()
                .find(|(k, v)| k.as_str() != "next_page_token" && v.is_array())
        })
        .map(|(k, _)| k.clone());
    let Some(key) = key else {
        return Ok(first);
    };
    let mut items = first[&key].as_array().cloned().unwrap_or_default();
    let mut token = first["next_page_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    while !token.is_empty() {
        args.insert("page_token".into(), Value::String(token));
        let page = client
            .invoke(&method.method, Value::Object(args.clone()))
            .await?;
        items.extend(page[&key].as_array().cloned().unwrap_or_default());
        token = page["next_page_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
    }
    Ok(json!({ key: items }))
}

/// Waits for an Operation (spec §3.9) and returns its settled Resource.
async fn wait(client: &Client, mut op: Value) -> Result<Value, Failure> {
    let Some(name) = op.get("name").and_then(Value::as_str).map(str::to_string) else {
        return Ok(op);
    };
    if !name.contains("/operations/") {
        return Ok(op);
    }
    let tty = std::io::stderr().is_terminal();
    while op.get("done") != Some(&Value::Bool(true)) {
        if tty {
            let msg = op
                .get("progress")
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("waiting");
            eprint!("\r{msg}…");
        }
        op = client
            .call(HttpRequest {
                method: "POST",
                path: format!("/v1/{name}:wait"),
                query: vec![("timeout".into(), "60s".into())],
                body: Some(json!({})),
                mutation: false,
                origin: None,
                effect_ids: false,
            })
            .await?;
    }
    if tty {
        eprint!("\r");
    }
    if let Some(err) = op.get("error").filter(|e| !e.is_null()) {
        let problem: sylphx::common::ProblemDetails =
            serde_json::from_value(err.clone()).map_err(|e| Failure::Usage(e.to_string()))?;
        return Err(Failure::Api(Error::Api {
            code: sylphx::common::ErrorCode::from_wire(&problem.code),
            status: problem.status.max(0) as u16,
            retryable: problem.retryable,
            effect: problem.effect.clone(),
            detail: problem.detail.clone(),
            request_id: None,
            problem: Box::new(problem),
        }));
    }
    Ok(match op.get("response") {
        Some(Value::Object(r)) => {
            let mut r = r.clone();
            r.remove("@type");
            Value::Object(r)
        }
        _ => op,
    })
}
