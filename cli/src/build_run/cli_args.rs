//! CLI subcommand definitions (`build run`, `build logs`) and option parsing.

use super::*;

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
        .after_long_help("For Hosting, pass the build attempt name from release build-attempts list, or its bda_ ID in the linked environment. Hosting output combines the build container streams. The latest --show-cursor value on stderr can be passed to --cursor to resume.")
        .arg(Arg::new("cursor").long("cursor").value_name("TOKEN")
            .help("Resume a Hosting build attempt after this cursor"))
        .arg(Arg::new("show-cursor").long("show-cursor").action(ArgAction::SetTrue)
            .help("Print the Hosting log cursor to stderr after each page"))
        .arg(Arg::new("build").value_name("BUILD").required(true).index(1)
            .help("The build name or id"))
        .arg(Arg::new("follow").long("follow").short('f').action(ArgAction::SetTrue)
            .help("Wait for new lines until the build ends"))
}

/// `sylphx build logs`.
pub async fn logs(client: Result<Client, Failure>, m: &ArgMatches) -> Result<(), Failure> {
    let client = client?;
    let arg = m.get_one::<String>("build").cloned().unwrap_or_default();
    if native_logs::is_attempt(&arg) {
        return native_logs::run(&client, &arg, m).await;
    }
    if m.get_one::<String>("cursor").is_some() || m.get_flag("show-cursor") {
        return Err(Failure::Usage(
            "--cursor and --show-cursor require a Hosting build attempt".into(),
        ));
    }
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
