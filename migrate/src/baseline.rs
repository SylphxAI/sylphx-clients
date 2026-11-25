//! Opt-in review of immutable legacy diagnostics; no blanket policy switch.
use std::collections::BTreeSet;

use serde::Deserialize;

use super::{lint, sql, valid_id, Files, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Legacy {
    Strict,
    Reviewed,
    Concurrent,
    Envelope,
    ImportOnly,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    file: String,
    sha256: String,
    findings: Vec<Finding>,
    // Slice 2's existing MIG002/MIG106 manifests predate this field.
    justification: Option<String>,
    #[serde(default)]
    autocommit: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    line: usize,
    rule_id: String,
}

pub(super) fn verify(bytes: &[u8], files: &mut Files) -> Result<()> {
    let entries: Vec<Entry> = serde_json::from_slice(bytes)
        .map_err(|_| "invalid legacy lint baseline; expected a JSON array of reviewed files")?;
    let mut seen = BTreeSet::new();
    for entry in entries {
        let id = entry.file.split_once('_').and_then(|(id, _)| valid_id(id));
        let file = id
            .and_then(|id| files.get_mut(&id))
            .filter(|file| file.file == entry.file)
            .ok_or("legacy lint baseline refers to a missing migration file")?;
        if !seen.insert(file.id) || file.sha256 != entry.sha256 {
            return Err(format!(
                "legacy lint baseline duplicate or checksum drift for {}",
                file.file
            ));
        }
        let checked = lint::check(file);
        if entry.findings.is_empty()
            || checked.diagnostics.iter().any(|d| {
                !matches!(
                    d.rule_id,
                    "MIG001"
                        | "MIG002"
                        | "MIG101"
                        | "MIG102"
                        | "MIG103"
                        | "MIG104"
                        | "MIG105"
                        | "MIG106"
                        | "MIG107"
                        | "MIG108"
                )
            })
            || !checked
                .diagnostics
                .iter()
                .map(|d| (d.line, d.rule_id))
                .eq(entry.findings.iter().map(|d| (d.line, d.rule_id.as_str())))
        {
            return Err(format!(
                "legacy lint baseline findings differ or are not eligible for {}",
                file.file
            ));
        }
        let has = |rule| entry.findings.iter().any(|d| d.rule_id == rule);
        let needs_reason = entry.autocommit
            || entry
                .findings
                .iter()
                .any(|d| !matches!(d.rule_id.as_str(), "MIG002" | "MIG106"));
        if (needs_reason && entry.justification.is_none())
            || entry
                .justification
                .as_ref()
                .is_some_and(|s| s.trim().is_empty())
        {
            return Err(format!(
                "legacy lint baseline is not eligible without a justification for {}",
                file.file
            ));
        }
        file.legacy = if entry.autocommit {
            // Never transform a pinned mixture of DML or transaction control
            // into a non-atomic sequence. Only the historical index shape
            // and a narrowly scoped timeout override are replayable this way.
            if !has("MIG107")
                || !checked.concurrent
                || !entry
                    .findings
                    .iter()
                    .all(|d| matches!(d.rule_id.as_str(), "MIG002" | "MIG107"))
                || !concurrent_shape(&file.sql)
            {
                return Err(format!("legacy concurrent replay requires an explicit autocommit pin for supported index SQL: {}", file.file));
            }
            Legacy::Concurrent
        } else if has("MIG107") {
            Legacy::ImportOnly
        } else if has("MIG108") {
            if !checked.concurrent && transaction_envelope(&file.sql) {
                Legacy::Envelope
            } else {
                Legacy::ImportOnly
            }
        } else if has("MIG001") {
            // The engine cannot safely construct statement spans for this
            // file. A lint pin does not make unscannable SQL executable.
            Legacy::ImportOnly
        } else {
            Legacy::Reviewed
        };
        if has("MIG105") {
            file.legacy_destructive_reason = entry.justification;
        }
    }
    Ok(())
}

fn concurrent_shape(source: &str) -> bool {
    sql::scan(source).is_ok_and(|statements| {
        statements.iter().all(|s| {
            let t = &s.tokens;
            let starts = |words: &[&str]| {
                t.len() >= words.len() && t.iter().zip(words).all(|(x, w)| x.is(w))
            };
            starts(&["create", "index", "concurrently"])
                || starts(&["create", "unique", "index", "concurrently"])
                || starts(&["drop", "index", "concurrently"])
                || (t.len() == 2 && starts(&["reset", "lock_timeout"]))
                || (t.len() == 4
                    && starts(&["set", "lock_timeout", "="])
                    && matches!(t[3].kind, sql::Kind::Number | sql::Kind::String))
        })
    })
}

fn transaction_envelope(source: &str) -> bool {
    sql::scan(source).is_ok_and(|s| {
        s.len() > 2
            && matches!(&s[0].tokens[..], [begin] if begin.is("begin"))
            && matches!(&s[s.len() - 1].tokens[..], [commit] if commit.is("commit"))
            && !s[1..s.len() - 1]
                .iter()
                .any(|s| lint::transaction(&s.tokens))
    })
}

#[cfg(test)]
#[path = "baseline_tests.rs"]
mod tests;
