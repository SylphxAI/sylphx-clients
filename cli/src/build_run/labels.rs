//! Caller attribution labels on a leased build machine.
//!
//! A `sylphx build run` creates a fresh lease per command, and every held
//! reservation must read back as whose it is: which worktree, which caller,
//! which source. The lease's metadata labels are the indexed, queryable
//! channel for that (list `labels.worktree=<v>`), so the caller's labels go
//! there beside the platform's own `purpose=build-run`, never in a parallel
//! record. The rules are the Sandboxes lease rules
//! (`services/sandboxes/crates/sandboxes-api/src/leases.rs`): keys and values
//! are lower-case letters, digits, `_` and `-`, at most 63 characters each,
//! with at most 64 labels.

/// The most labels a lease accepts (the Sandboxes rule).
pub const MAX_LABELS: usize = 64;
/// The most characters in one label key or value (the Sandboxes rule).
pub const MAX_LEN: usize = 63;

/// One `KEY=VALUE` from `--label`, refused before any machine is leased.
pub fn parse(kv: &str) -> Result<(String, String), String> {
    let (k, v) = kv
        .split_once('=')
        .ok_or_else(|| format!("--label {kv}: give KEY=VALUE"))?;
    if !key_ok(k) {
        return Err(format!(
            "--label {kv}: `{k}` is lower-case letters, digits, `_`, `-`, at most {MAX_LEN} characters"
        ));
    }
    if !value_ok(v) {
        return Err(format!(
            "--label {kv}: `{v}` is lower-case letters, digits, `_`, `-`, at most {MAX_LEN} characters"
        ));
    }
    Ok((k.to_string(), v.to_string()))
}

/// Sorted by key, refusing a duplicate and the platform's own `purpose`
/// (a caller label never rebinds it), and counting the whole set.
pub fn collect(
    pairs: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in pairs {
        if k == "purpose" {
            return Err("--label purpose: the platform sets this label on a build machine".into());
        }
        if out.iter().any(|(seen, _)| *seen == k) {
            return Err(format!("--label {k}=...: given more than once"));
        }
        out.push((k, v));
    }
    if out.len() > MAX_LABELS - 1 {
        // One slot is the platform's `purpose`.
        return Err(format!("--label: at most {} labels", MAX_LABELS - 1));
    }
    out.sort();
    Ok(out)
}

/// A label key must be non-empty and within the lease's character rules.
fn key_ok(s: &str) -> bool {
    !s.is_empty() && chars_ok(s)
}

/// A label value may be empty (the Sandboxes rule checks only `k.is_empty()`),
/// but every character must be within the lease's rules.
fn value_ok(s: &str) -> bool {
    chars_ok(s)
}

fn chars_ok(s: &str) -> bool {
    s.len() <= MAX_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The label value for a work-tree name or repository: lower-cased, every
/// character outside the lease rules folded to `-`, trimmed and capped, so a
/// path or remote URL always yields a valid value. Empty when nothing is left.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' as char || c == b'-' as char {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    out.chars()
        .take(MAX_LEN)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

/// The repository name of `origin`'s URL: `owner/name`, `name` or an `ssh://`
/// spelling. `None` when Git cannot answer or the answer has no name.
pub fn repo(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?.trim().to_string();
    let tail = url.rsplit(['/', ':']).next()?.trim_end_matches(".git");
    let name = slug(tail);
    (!name.is_empty()).then_some(name)
}

/// The run's own attribution: the work-tree name and, when Git names one, its
/// repository. These need no caller opt-in, so every held reservation reads
/// back as whose it is. A caller's `--label` is applied on top and wins.
pub fn defaults(root: &std::path::Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(name) = root.file_name().and_then(|n| n.to_str()) {
        let name = slug(name);
        if !name.is_empty() {
            out.push(("worktree".to_string(), name));
        }
    }
    if let Some(repo) = repo(root) {
        out.push(("source".to_string(), repo));
    }
    out
}

/// The full set for one run: the run's own defaults, then the caller's labels
/// (a caller key replaces a default of the same name). Sorted and deduplicated
/// by the lease rules; a value that no rule yields a non-empty slug for is
/// dropped rather than failing the run.
pub fn merge(
    defaults: Vec<(String, String)>,
    caller: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut out = defaults;
    for (k, v) in caller {
        match out.iter_mut().find(|(seen, _)| *seen == k) {
            Some(slot) => slot.1 = v,
            None => out.push((k, v)),
        }
    }
    out.retain(|(_, v)| !v.is_empty());
    out.sort();
    out.truncate(MAX_LABELS - 1);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_lower_case_and_within_the_lease_rules() {
        assert_eq!(
            parse("worktree=w-22292").unwrap(),
            ("worktree".into(), "w-22292".into())
        );
        // The value may be empty (a flag-like label) but the key may not.
        assert_eq!(parse("fresh=").unwrap(), ("fresh".into(), String::new()));
        for bad in [
            "novalue",
            "=v",
            "Key=v",
            "k=V",
            "k=a b",
            &format!("{}=v", "k".repeat(64)),
            &format!("k={}", "v".repeat(64)),
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn labels_are_sorted_and_unique_and_never_rebind_purpose() {
        let v = collect([
            ("worktree".to_string(), "w".to_string()),
            ("source".to_string(), "s".to_string()),
        ])
        .unwrap();
        assert_eq!(v[0].0, "source");
        assert_eq!(v[1].0, "worktree");
        assert!(collect([
            ("worktree".to_string(), "a".to_string()),
            ("worktree".to_string(), "b".to_string()),
        ])
        .is_err());
        assert!(collect([("purpose".to_string(), "other".to_string())]).is_err());
        let many: Vec<_> = (0..MAX_LABELS)
            .map(|i| (format!("k{i}"), "v".to_string()))
            .collect();
        assert!(collect(many).is_err());
    }

    #[test]
    fn a_slug_is_a_valid_label_value() {
        assert_eq!(slug("Build-Reservation-Demand"), "build-reservation-demand");
        assert_eq!(slug("SylphxAI/cloud"), "sylphxai-cloud");
        // Beyond the limit is capped, and a capped edge never ends on a `-`.
        let long = slug(&"a".repeat(80));
        assert_eq!(long.len(), MAX_LEN);
        assert!(!long.ends_with('-'));
        assert_eq!(slug("☃☃"), "");
    }

    #[test]
    fn defaults_name_the_worktree_without_a_caller_opt_in() {
        let root = std::path::Path::new("/scratch/wt/build-reservation-demand");
        let d = defaults(root);
        assert!(d.contains(&(
            "worktree".to_string(),
            "build-reservation-demand".to_string()
        )));
    }

    #[test]
    fn the_callers_label_wins_over_a_default_of_the_same_name() {
        let m = merge(
            vec![("worktree".to_string(), "from-git".to_string())],
            vec![("worktree".to_string(), "from-caller".to_string())],
        );
        assert_eq!(m, vec![("worktree".to_string(), "from-caller".to_string())]);
        // An empty value is dropped: it would be a label with nothing to say.
        let m = merge(vec![], vec![("fresh".to_string(), String::new())]);
        assert!(m.is_empty());
    }
}
