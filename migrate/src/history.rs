//! Deterministic reconciliation: no implicit max-version watermark.
use std::collections::BTreeMap;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Serialize;
use sha2::{Digest, Sha256, Sha384};

use super::{Files, Migration, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Sylphx,
    Atlas,
    Sqlx,
    AtlasAndSqlx,
}

impl Source {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Sylphx => "sylphx",
            Self::Atlas => "atlas",
            Self::Sqlx => "sqlx",
            Self::AtlasAndSqlx => "atlas_and_sqlx",
        }
    }

    pub(super) fn parse(source: &str) -> Result<Self> {
        match source {
            "sylphx" => Ok(Self::Sylphx),
            "atlas" => Ok(Self::Atlas),
            "sqlx" => Ok(Self::Sqlx),
            "atlas_and_sqlx" => Ok(Self::AtlasAndSqlx),
            _ => Err("unknown canonical migration source".into()),
        }
    }

    fn atlas(self) -> bool {
        matches!(self, Self::Atlas | Self::AtlasAndSqlx)
    }
    fn sqlx(self) -> bool {
        matches!(self, Self::Sqlx | Self::AtlasAndSqlx)
    }
}

pub(super) struct Recorded {
    pub sha256: String,
    pub source: Source,
    pub(super) done: bool,
}

#[derive(Serialize)]
pub struct File<'a> {
    pub id: String,
    pub file: &'a str,
    pub sha256: &'a str,
    pub source: Option<Source>,
}

impl<'a> File<'a> {
    pub(super) fn new(m: &'a Migration, source: Option<Source>) -> Self {
        Self {
            id: m.id.to_string(),
            file: &m.file,
            sha256: &m.sha256,
            source,
        }
    }
}

#[derive(Serialize)]
pub struct Report<'a> {
    pub applied: Vec<File<'a>>,
    pub pending: Vec<File<'a>>,
    // Rows to insert, already verified; not mixed into the status JSON.
    #[serde(skip)]
    pub(super) imports: Vec<(i64, Source)>,
}

pub(super) fn reconcile<'a>(
    files: &'a Files,
    canonical: &BTreeMap<i64, Recorded>,
    atlas: &BTreeMap<i64, String>,
    sqlx: &BTreeMap<i64, Vec<u8>>,
    initialized: bool,
) -> Result<Report<'a>> {
    for (&id, record) in canonical {
        if !record.done {
            return Err(format!("migration {id} has an unconfirmed concurrent index operation; inspect the target before any retry"));
        }
        let file = files
            .get(&id)
            .ok_or_else(|| format!("applied migration {id} is missing locally"))?;
        if file.sha256 != record.sha256 {
            return Err(format!(
                "checksum drift for applied migration {id} ({})",
                file.file
            ));
        }
    }
    for id in atlas.keys().chain(sqlx.keys()) {
        if !files.contains_key(id) {
            return Err(format!(
                "legacy migration {id} is missing locally; supply the complete history"
            ));
        }
    }
    // Atlas v1.3.0 stores the cumulative SHA-256 of name+bytes, not a
    // per-file hash. Verify the historical applied set in Atlas filename order,
    // excluding new pending files. Never consult or generate atlas.sum.
    {
        let mut prefix = Sha256::new();
        let mut applied = atlas
            .iter()
            .map(|(id, hash)| (&files[id], hash))
            .collect::<Vec<_>>();
        applied.sort_by_key(|(m, _)| &m.file);
        for (m, legacy_hash) in applied {
            prefix.update(m.file.as_bytes());
            prefix.update(m.sql.as_bytes());
            if &STANDARD.encode(prefix.clone().finalize()) != legacy_hash {
                return Err(format!(
                    "Atlas checksum drift or unsupported revision lineage for migration {}",
                    m.id
                ));
            }
        }
    }
    let mut report = Report {
        applied: Vec::new(),
        pending: Vec::new(),
        imports: Vec::new(),
    };
    for (&id, m) in files {
        let in_atlas = atlas.contains_key(&id);
        let in_sqlx = sqlx.contains_key(&id);
        if let Some(old_hash) = sqlx.get(&id) {
            if &Sha384::digest(m.sql.as_bytes())[..] != old_hash.as_slice() {
                return Err(format!("SQLx checksum drift for migration {id}"));
            }
        }
        if let Some(record) = canonical.get(&id) {
            // Legacy ledgers may be archived after cutover. They may not keep
            // writing new IDs, or claim an ID written by a different engine.
            if (in_atlas && !record.source.atlas()) || (in_sqlx && !record.source.sqlx()) {
                return Err(format!(
                    "conflicting ledger sources for migration {id}; stop the legacy writer"
                ));
            }
            report.applied.push(File::new(m, Some(record.source)));
        } else {
            let source = match (in_atlas, in_sqlx) {
                (true, true) => Some(Source::AtlasAndSqlx),
                (true, false) => Some(Source::Atlas),
                (false, true) => Some(Source::Sqlx),
                (false, false) => None,
            };
            if let Some(source) = source {
                if initialized {
                    return Err(format!(
                        "legacy migration {id} appeared after cutover; stop the legacy writer"
                    ));
                }
                report.imports.push((id, source));
                report.applied.push(File::new(m, Some(source)));
            } else {
                report.pending.push(File::new(m, None));
            }
        }
    }
    Ok(report)
}
