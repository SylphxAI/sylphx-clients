//! Shared PostgreSQL migration engine.
mod baseline;
mod history;
mod lint;
mod postgres;
mod sql;

use std::{collections::BTreeMap, fs, path::Path};

use sha2::{Digest, Sha256};

pub type Result<T> = std::result::Result<T, String>;
type Files = BTreeMap<i64, Migration>;

pub use history::{File, Report, Source};
pub use lint::Diagnostic;
pub use postgres::{connect, Apply};

mod release;
pub use release::{ReleasePlan, ReleaseTarget};

pub struct SourceTree {
    files: Files,
    baseline_sha: Option<String>,
}

struct Migration {
    id: i64,
    file: String,
    sql: String,
    sha256: String,
    legacy: baseline::Legacy,
    legacy_destructive_reason: Option<String>,
}

fn valid_id(raw: &str) -> Option<i64> {
    let id = raw.parse::<i64>().ok()?;
    (id > 0 && id.to_string() == raw).then_some(id)
}

impl SourceTree {
    /// Read each SQL file once so the checked bytes are the executed bytes.
    pub fn load(dir: &Path, baseline: Option<&Path>) -> Result<Self> {
        let invalid = |e| format!("cannot load migration directory {}: {e}", dir.display());
        let mut entries = Vec::new();
        for entry in fs::read_dir(dir).map_err(invalid)? {
            let entry = entry.map_err(invalid)?;
            let file = entry
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF-8 migration filename")?;
            if file.ends_with(".sql") {
                if !entry.file_type().map_err(invalid)?.is_file() {
                    return Err(format!("migration must be a regular file: {file}"));
                }
                entries.push((file, fs::read_to_string(entry.path()).map_err(invalid)?));
            }
        }
        let baseline = baseline
            .map(fs::read)
            .transpose()
            .map_err(|_| "cannot read legacy lint baseline")?;
        Self::from_blobs(entries, baseline.as_deref())
    }

    /// Exact regular blobs of the nominated source directory.
    pub fn from_blobs(entries: Vec<(String, String)>, baseline: Option<&[u8]>) -> Result<Self> {
        let mut files = Files::new();
        for (file, sql) in entries {
            let Some(stem) = file.strip_suffix(".sql") else {
                continue;
            };
            let (name, down) = match stem.strip_suffix(".down") {
                Some(name) => (name, true),
                None => (stem.strip_suffix(".up").unwrap_or(stem), false),
            };
            let (id, slug) = name
                .split_once('_')
                .ok_or_else(|| format!("expected <id>_<name>.sql: {file}"))?;
            let id = valid_id(id)
                .ok_or_else(|| format!("invalid positive decimal migration ID: {file}"))?;
            if slug.is_empty() || file.contains(['/', '\\']) || file.chars().any(char::is_control) {
                return Err(format!("invalid migration filename: {file}"));
            }
            if down {
                continue;
            }
            let sha256 = format!("{:x}", Sha256::digest(sql.as_bytes()));
            if files
                .insert(
                    id,
                    Migration {
                        id,
                        file,
                        sql,
                        sha256,
                        legacy: baseline::Legacy::Strict,
                        legacy_destructive_reason: None,
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate migration ID {id}"));
            }
        }
        if let Some(bytes) = baseline {
            baseline::verify(bytes, &mut files)?;
        }
        let baseline_sha = baseline.map(|bytes| format!("{:x}", Sha256::digest(bytes)));
        Ok(Self {
            files,
            baseline_sha,
        })
    }

    pub fn lint(&self) -> Vec<Diagnostic<'_>> {
        self.files
            .values()
            .flat_map(|file| lint::admitted(file, false).diagnostics)
            .collect()
    }

    pub async fn inspect(&self, db: &mut sqlx::PgConnection) -> Result<Report<'_>> {
        postgres::inspect(db, &self.files).await
    }

    pub async fn apply(&self, db: &mut sqlx::PgConnection) -> Result<Apply<'_>> {
        postgres::apply(db, self, None).await
    }
}
