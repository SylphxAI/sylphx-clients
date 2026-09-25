//! `sylphx`: the Sylphx CLI (docs/specs/one-platform/resource-api-and-clients.md §8.5).
//!
//! `sylphx <service> <collection> <verb> [NAME|PARENT|ID] [--flags]` is
//! generated from the one schema (`generated/commands.json`) and calls the
//! generated Rust SDK; the porcelain (`login`, `logout`, `whoami`, `link`,
//! `api`, `mcp`, `completion`) is hand-written on the same SDK.

mod auth;
mod context;
mod output;
mod tree;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{json, Map, Value};
use sylphx::{Client, Error, HttpRequest};

use context::Link;

use output::Format;
use tree::{MethodCmd, Tree};

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
                .value_name("KEY")
                .help("A Sylphx Access key; defaults to SYLPHX_API_KEY, then `sylphx login`"),
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
                    Arg::new("org")
                        .long("org")
                        .value_name("ORG")
                        .help("Organization (id or slug) when your account is in several"),
                )
                .arg(
                    Arg::new("no-verify")
                        .long("no-verify")
                        .action(ArgAction::SetTrue)
                        .help("Store the credential without calling whoami"),
                ),
        )
        .subcommand(Command::new("logout").about("Revoke the stored key and forget it"))
        .subcommand(
            Command::new("whoami").about("Show the caller: principal, org, project, env, scopes"),
        )
        .subcommand(
            Command::new("link")
                .visible_alias("init")
                .about("Link this directory to an org, project, and env (.sylphx/project.json)")
                .arg(
                    Arg::new("org")
                        .long("org")
                        .value_name("NAME")
                        .help("orgs/{org}"),
                )
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
        .subcommand(Command::new("mcp").about("Run the Sylphx MCP server over stdio"))
        .subcommand(
            Command::new("completion")
                .about("Print a shell completion script")
                .arg(
                    Arg::new("shell")
                        .required(true)
                        .value_parser(clap::value_parser!(clap_complete::Shell)),
                ),
        );
    for s in tree::service_commands(tree) {
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
        Err(Failure::Api(e)) => {
            eprintln!("{}", describe(&e));
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Usage(String),
    Api(Error),
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
    let client = || async {
        context::client(api_key.clone(), base_url.clone())
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
        "login" => {
            login(
                api_key.clone(),
                base_url.clone(),
                sub.get_flag("with-token"),
                sub.get_one::<String>("org").cloned(),
                sub.get_flag("no-verify"),
            )
            .await
        }
        "logout" => {
            let creds = context::load_credentials();
            if let Some(key) = context::stored_key(&creds) {
                let url = base_url.clone().or(creds.base_url.clone());
                if key.starts_with("sylphx_sk_") {
                    if let Err(e) = auth::revoke_self(&auth::api_root(url.as_deref()), &key).await {
                        eprintln!("warning: could not revoke the key server-side ({e}); revoke it in Settings → API keys");
                    }
                }
            }
            match context::forget(&creds)? {
                Some(p) => println!(
                    "Logged out: the key is revoked and removed ({}).",
                    p.display()
                ),
                None => println!("Not logged in."),
            }
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
        "mcp" => {
            let c = context::client(api_key.clone(), base_url.clone()).await?;
            sylphx_mcp::serve_stdio(sylphx_mcp::Server::new(c))
                .await
                .map_err(|e| Failure::Usage(e.to_string()))
        }
        "completion" => {
            let shell = *sub
                .get_one::<clap_complete::Shell>("shell")
                .expect("required");
            let mut cmd = cli(tree);
            clap_complete::generate(shell, &mut cmd, "sylphx", &mut std::io::stdout());
            Ok(())
        }
        service => {
            let (method, mm) = tree::find(tree, service, sub)
                .ok_or_else(|| Failure::Usage(format!("unknown command under `{service}`")))?;
            generated(&client().await?, method, &mm, m, format).await
        }
    }
}

async fn login(
    api_key: Option<String>,
    base_url: Option<String>,
    with_token: bool,
    org: Option<String>,
    no_verify: bool,
) -> Result<(), Failure> {
    let stdin_key = api_key.as_deref() == Some("-")
        || (api_key.is_none() && (with_token || !std::io::stdin().is_terminal()));
    let key = if stdin_key {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| e.to_string())?;
        Some(s.trim().to_string())
    } else {
        api_key
    };
    let (key, key_name) = match key {
        Some(k) if k.is_empty() => return Err(Failure::Usage("no key on stdin".into())),
        Some(k) => {
            if !k.starts_with("sylphx_sk_") && !k.starts_with("sylphx_pk_") {
                eprintln!("warning: an Access key starts with sylphx_sk_ (secret) or sylphx_pk_ (publishable)");
            }
            (k, None)
        }
        None => {
            let api = auth::api_root(base_url.as_deref());
            let issued = auth::device_login(&api, org).await?;
            eprintln!("Approved in {}.", issued.org_slug);
            (
                issued.api_key,
                Some(issued.key_name).filter(|n| !n.is_empty()),
            )
        }
    };
    let (path, in_keychain) = context::store_key(base_url.clone(), &key, key_name)?;
    let where_ = if in_keychain {
        "the OS keychain".to_string()
    } else {
        eprintln!(
            "note: no OS keychain here; the key is in {} (mode 0600)",
            path.display()
        );
        path.display().to_string()
    };
    if no_verify {
        println!("Credential stored in {where_}.");
        return Ok(());
    }
    let client = context::client(Some(key.clone()), base_url.clone())
        .await?
        .expect("a credential was stored");
    match client.invoke("access.whoami", json!({})).await {
        Ok(me) => println!(
            "Signed in as {} (key stored in {where_}).",
            me["principal"]
                .as_str()
                .or_else(|| me["email"].as_str())
                .or_else(|| me["user"]["email"].as_str())
                .unwrap_or("?"),
        ),
        Err(e) => {
            let _ = context::forget(&context::load_credentials());
            return Err(e.into());
        }
    }
    Ok(())
}

async fn link(client: &Client, sub: &ArgMatches) -> Result<(), Failure> {
    let me = client.invoke("access.whoami", json!({})).await?;
    let mut l = Link::from_whoami(&me);
    for (k, slot) in [
        ("org", &mut l.org),
        ("project", &mut l.project),
        ("env", &mut l.env),
    ] {
        if let Some(v) = sub.get_one::<String>(k) {
            *slot = v.clone();
        }
    }
    if l.org.is_empty() {
        return Err(Failure::Usage("the key names no org; pass --org".into()));
    }
    let dir = std::env::current_dir().map_err(|e| e.to_string())?;
    let p = context::write_link(&dir, &l)?;
    println!(
        "Linked {} to {}.",
        dir.display(),
        if l.env.is_empty() { &l.project } else { &l.env }
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

/// Defaults for names and parents: the linked project, else the key's scope.
struct Defaults<'c> {
    client: &'c Client,
    link: Option<Link>,
}

impl Defaults<'_> {
    async fn at(&mut self, pattern: &str) -> Result<String, Failure> {
        let level = tree::context_level(pattern).ok_or_else(|| {
            Failure::Usage(format!("give the parent explicitly (it matches {pattern})"))
        })?;
        if self.link.is_none() {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            self.link = Some(match context::find_link(&cwd) {
                Some((_, l)) => l,
                None => Link::from_whoami(&self.client.invoke("access.whoami", json!({})).await?),
            });
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
        if value.contains('/') || !pattern.contains('/') {
            return Ok(value.to_string());
        }
        let (parent, collection) = tree::split_pattern(pattern)
            .ok_or_else(|| Failure::Usage(format!("`{value}` must be a full name ({pattern})")))?;
        Ok(format!("{}/{collection}/{value}", self.at(&parent).await?))
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
    let mut defaults = Defaults { client, link: None };

    if let Some(p) = &method.positional {
        let given = mm.get_one::<String>("__positional").cloned();
        let value = match (p.arg.as_str(), given) {
            ("ID", v) => v,
            ("PARENT", Some(v)) => Some(v),
            ("PARENT", None)
                if tree::get_path(&Value::Object(args.clone()), &p.field).is_none() =>
            {
                Some(
                    defaults
                        .at(p.pattern.as_deref().unwrap_or_default())
                        .await?,
                )
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
