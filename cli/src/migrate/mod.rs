//! Local PostgreSQL migration porcelain. No Cloud API or legacy executable.
mod history;
mod lint;
mod postgres;
mod sql;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use clap::{value_parser, Arg, ArgMatches, Command};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::Failure;

type Result<T> = std::result::Result<T, String>;
type Files = BTreeMap<i64, Migration>;

struct Migration {
    id: i64,
    file: String,
    sql: String,
    sha256: String,
}

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
        .subcommands([
            Command::new("plan").about("Read target history and list local SQL pending execution"),
            Command::new("lint").about("Lint every SQL file without contacting PostgreSQL"),
            Command::new("apply").about("Lint and execute pending SQL against DATABASE_URL"),
            Command::new("status").about("Read target history and reconcile it with local SQL"),
        ])
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

fn valid_id(raw: &str) -> Option<i64> {
    let id = raw.parse::<i64>().ok()?;
    (id > 0 && id.to_string() == raw).then_some(id)
}

fn load(dir: &Path) -> Result<Files> {
    let invalid = |e| format!("cannot load migration directory {}: {e}", dir.display());
    let mut files = Files::new();
    // File name order is used only for legacy Atlas reconciliation. Reading
    // once pins the exact bytes that will be linted, executed and recorded.
    let mut entries = fs::read_dir(dir)
        .map_err(invalid)?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(invalid)?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let file = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF-8 migration filename")?;
        let Some(stem) = file.strip_suffix(".sql") else {
            continue;
        };
        // SQLx reversible migrations use .up.sql and .down.sql. Forward only.
        let (name, down) = if let Some(name) = stem.strip_suffix(".down") {
            (name, true)
        } else {
            (stem.strip_suffix(".up").unwrap_or(stem), false)
        };
        let (id, slug) = name
            .split_once('_')
            .ok_or_else(|| format!("expected <id>_<name>.sql: {file}"))?;
        let id =
            valid_id(id).ok_or_else(|| format!("invalid positive decimal migration ID: {file}"))?;
        if slug.is_empty() || !entry.file_type().map_err(invalid)?.is_file() {
            return Err(format!(
                "migration must be a named regular file (not a symlink): {file}"
            ));
        }
        if down {
            continue;
        }
        let sql = fs::read_to_string(entry.path()).map_err(invalid)?;
        let sha256 = format!("{:x}", Sha256::digest(sql.as_bytes()));
        if files
            .insert(
                id,
                Migration {
                    id,
                    file,
                    sql,
                    sha256,
                },
            )
            .is_some()
        {
            return Err(format!("duplicate migration ID {id}"));
        }
    }
    Ok(files)
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
    let files = load(&dir).map_err(Failure::Usage)?;
    if action == "lint" {
        let diagnostics = files
            .values()
            .flat_map(|file| lint::check(file).diagnostics)
            .collect::<Vec<_>>();
        write_json(&serde_json::json!({"diagnostics": diagnostics}))?;
        return if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(Failure::Exit(1))
        };
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        Failure::Usage("set DATABASE_URL for the intended PostgreSQL target".into())
    })?;
    let mut db = postgres::connect(&url).await.map_err(Failure::Refused)?;
    if action == "apply" {
        match postgres::apply(&mut db, &files)
            .await
            .map_err(Failure::Refused)?
        {
            postgres::Apply::Applied(report) => write_json(&report),
            postgres::Apply::LintFailed(diagnostics) => {
                write_json(&serde_json::json!({"diagnostics": diagnostics}))?;
                Err(Failure::Exit(1))
            }
        }
    } else {
        let report = postgres::inspect(&mut db, &files)
            .await
            .map_err(Failure::Refused)?;
        if action == "plan" {
            write_json(&report.pending)
        } else {
            write_json(&report)
        }
    }
}
