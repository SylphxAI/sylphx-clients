//! Opt-in review of immutable legacy diagnostics; no blanket policy switch.
use std::collections::BTreeSet;

use serde::Deserialize;

use super::{lint, valid_id, Files, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    file: String,
    sha256: String,
    findings: Vec<Finding>,
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
            || checked
                .diagnostics
                .iter()
                .any(|d| !matches!(d.rule_id, "MIG002" | "MIG106"))
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
        file.legacy_lint_reviewed = true;
    }
    Ok(())
}
