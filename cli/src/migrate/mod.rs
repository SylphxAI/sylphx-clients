//! Local PostgreSQL migration porcelain.
use crate::Failure;
use clap::{value_parser, Arg, ArgMatches, Command};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};
use sylphx_migrate::{Apply, ReleaseTarget, RuntimeRoles, SourceTree};
type Result<T> = std::result::Result<T, String>;

pub(crate) fn command() -> Command {
    Command::new("migrate")
        .about("Plan, lint, apply or inspect PostgreSQL schema migrations (JSON output)")
        .subcommand_required(true)
        .arg(
            Arg::new("config")
                .long("config")
                .global(true)
                .value_parser(value_parser!(PathBuf))
                .value_name("PATH")
                .help("Path to sylphx.toml (default: ./sylphx.toml)"),
        )
        .arg(
            Arg::new("dir")
                .long("dir")
                .global(true)
                .value_parser(value_parser!(PathBuf))
                .conflicts_with("config")
                .value_name("PATH")
                .help("Use this SQL directory instead of a manifest"),
        )
        .arg(
            Arg::new("legacy-lint-baseline")
                .long("legacy-lint-baseline")
                .global(true)
                .value_parser(value_parser!(PathBuf))
                .value_name("PATH")
                .help("Validate reviewed legacy findings and replay options pinned to whole-file SHA-256"),
        )
        .subcommands([
            Command::new("plan")
                .about("Read target history and list local SQL pending execution")
                .arg(
                    Arg::new("release")
                        .long("release")
                        .action(clap::ArgAction::SetTrue)
                        .hide(true),
                )
                .arg(runtime_role_arg().requires("release")),
            Command::new("lint").about("Lint every SQL file without contacting PostgreSQL"),
            Command::new("apply")
                .about("Lint and execute pending SQL against DATABASE_URL")
                .arg(Arg::new("release-seal").long("release-seal").hide(true))
                .arg(runtime_role_arg()),
            Command::new("status").about("Read target history and reconcile it with local SQL"),
        ])
}

fn runtime_role_arg() -> Arg {
    Arg::new("grant-runtime-role")
        .long("grant-runtime-role")
        .action(clap::ArgAction::Append)
        .value_name("ROLE")
        .help("Grant runtime access to this login (repeatable). Falls back to comma-separated SYLPHX_MIGRATE_RUNTIME_ROLES only when absent")
}

fn runtime_roles(args: &ArgMatches, fallback: Option<OsString>) -> Result<RuntimeRoles> {
    if let Some(explicit) = args.get_many::<String>("grant-runtime-role") {
        return RuntimeRoles::new(explicit);
    }
    match fallback {
        None => Ok(RuntimeRoles::default()),
        Some(roles) => RuntimeRoles::new(
            roles
                .to_str()
                .ok_or("SYLPHX_MIGRATE_RUNTIME_ROLES must be UTF-8")?
                .split(','),
        ),
    }
}

#[derive(Deserialize)]
struct Manifest {
    database: Database,
}
#[derive(Deserialize)]
struct Database {
    migrations: MigrationConfig,
}
#[derive(Deserialize)]
struct MigrationConfig {
    engine: String,
    dir: PathBuf,
}

fn config_dir(path: &Path) -> Result<PathBuf> {
    let source =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    // Do not echo TOML deserialization errors: a manifest can contain secrets.
    let config: Manifest = toml::from_str(&source).map_err(|_| {
        "manifest must declare [database.migrations] with engine and dir".to_string()
    })?;
    if config.database.migrations.engine != "sylphx" {
        return Err("manifest must opt in with [database.migrations] engine = \"sylphx\"".into());
    }
    let dir = config.database.migrations.dir;
    if dir.as_os_str().is_empty()
        || dir.is_absolute()
        || dir
            .components()
            .any(|p| p == std::path::Component::ParentDir)
    {
        return Err(
            "database.migrations.dir must be a non-empty project-relative directory".into(),
        );
    }
    Ok(path.parent().unwrap_or_else(|| Path::new(".")).join(dir))
}

fn write_json<T: serde::Serialize>(value: &T) -> std::result::Result<(), Failure> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|_| Failure::Refused("cannot encode migration report".into()))?
    );
    Ok(())
}

pub(crate) async fn run(args: &ArgMatches) -> std::result::Result<(), Failure> {
    let (action, args) = args.subcommand().expect("requires a migration subcommand");
    let dir = match args.get_one::<PathBuf>("dir") {
        Some(dir) => dir.clone(),
        None => config_dir(
            args.get_one::<PathBuf>("config")
                .map(PathBuf::as_path)
                .unwrap_or(Path::new("sylphx.toml")),
        )
        .map_err(Failure::Usage)?,
    };
    let files = SourceTree::load(
        &dir,
        args.get_one::<PathBuf>("legacy-lint-baseline")
            .map(PathBuf::as_path),
    )
    .map_err(Failure::Usage)?;
    if action == "lint" {
        let diagnostics = files.lint();
        write_json(&serde_json::json!({"diagnostics": diagnostics}))?;
        return if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(Failure::Exit(1))
        };
    }
    let seal = if action == "apply" {
        args.get_one::<String>("release-seal")
    } else {
        None
    };
    let release_plan = action == "plan" && args.get_flag("release");
    let files = if release_plan || action == "apply" {
        files.with_runtime_roles(
            runtime_roles(args, std::env::var_os("SYLPHX_MIGRATE_RUNTIME_ROLES"))
                .map_err(Failure::Usage)?,
        )
    } else {
        files
    };
    let key = if release_plan || seal.is_some() {
        "MIGRATOR_DATABASE_URL"
    } else {
        "DATABASE_URL"
    };
    let url = std::env::var(key)
        .map_err(|_| Failure::Usage(format!("set {key} for the intended PostgreSQL target")))?;
    let target = (release_plan || seal.is_some())
        .then(|| ReleaseTarget::parse(&url))
        .transpose()
        .map_err(Failure::Refused)?;
    let mut db = sylphx_migrate::connect(&url)
        .await
        .map_err(Failure::Refused)?;
    if release_plan {
        let plan = files
            .release_plan(&mut db, target.as_ref().expect("release target"))
            .await
            .map_err(Failure::Refused)?;
        let pending: Vec<_> = plan.pending.iter().map(|(file, _)| file).collect();
        return write_json(&serde_json::json!({"seal": plan.seal, "pending": pending}));
    }
    if action == "apply" {
        let result = if let Some((target, seal)) = target.as_ref().zip(seal) {
            files.apply_release(&mut db, target, seal).await
        } else {
            files.apply(&mut db).await
        };
        match result.map_err(Failure::Refused)? {
            Apply::Applied(report) => write_json(&report),
            Apply::LintFailed(diagnostics) => {
                write_json(&serde_json::json!({"diagnostics": diagnostics}))?;
                Err(Failure::Exit(1))
            }
        }
    } else {
        let report = files.inspect(&mut db).await.map_err(Failure::Refused)?;
        if action == "plan" {
            write_json(&report.pending)
        } else {
            write_json(&report)
        }
    }
}

#[cfg(test)]
mod tests;
