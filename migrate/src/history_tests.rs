use super::{reconcile, AtlasRevision, Source};
use crate::SourceTree;
use std::collections::BTreeMap;

// Pinned outputs from Atlas migrate hash for each staged directory, not hashes
// computed by the verifier under test. Notify also matches an actual Atlas
// non-linear apply on disposable PostgreSQL.
const FIRST: &str = "9mCTjzmWRpHPmi+i+3OLimPxAmh3qnzsAWNgW+aPR5M=";
const SECOND: &str = "gwON8k+73jehIguybyflQhbVimRxuOxKGaUrjGBwBMI=";
const THIRD_LINEAR: &str = "IZd5awvGcDIbyjzq6Z/dTEqEK0SXRRkZf1MfPJQoHn0=";
const FOURTH_LINEAR: &str = "FYovzeHwT2fWnzOjtINAi7tibcsm4A26Crdoz8n7m7s=";
const FIFTH_LINEAR: &str = "4AhFLxo4e01L/K06UNpev8E3XV1BK/7/FguqsfuJBQU=";
const THIRD_EARLY: &str = "7Z0s9ScEj0Qh2zsPuJuU44Hx3PDW76LWOrMGynssB7g=";
const NOTIFY: &[(i64, i64, &str)] = &[
    (22, 1, FIRST),
    (24, 2, THIRD_EARLY),
    (25, 2, "Ont5Pi85Z0uHVfO/hh3wdhAF4KLR1/KdTSTl3jYD4BA="),
    (23, 3, SECOND),
    (27, 4, FIFTH_LINEAR),
];
// Four lower filenames were added in two batches around another higher file.
const AGENTS: &[(i64, i64, &str)] = &[
    (22, 1, FIRST),
    (27, 2, "/uosD4KIkrfYKKJfc0ohwQfeB7FveXpVO6SA41ZtM7g="),
    (23, 3, SECOND),
    (24, 3, THIRD_LINEAR),
    (28, 4, "ZpI1ZnVBJ7Rjh4G4UacrkiYDYoDnzqJDmjMkaX1kJ0I="),
    (25, 5, FOURTH_LINEAR),
    (26, 5, "JFkKTgcr2UAHcA5IMnbbTg91OaNv4aKHrFCgwnVe01I="),
    (29, 5, "s+fAV6OhQ45I/zyf6GNgSynLC1RCsIrKV9RhaqiCs/s="),
];
// Two independent inversions, separated by a fully reconciled prefix.
const SANDBOXES: &[(i64, i64, &str)] = &[
    (22, 1, FIRST),
    (24, 2, THIRD_EARLY),
    (23, 3, SECOND),
    (26, 4, "LCac1yu3GwuY0rOk3vmo2ycZ9Srb3DmJGjXcCl0X5Wo="),
    (27, 4, "MqiZZ+P2cvk6vPLm0riPpWf30++tCIQBBTkVxVwYmKI="),
    (25, 5, FOURTH_LINEAR),
    (28, 6, "B+na/djYNkhrBUkf7nT6q3cm90GSdHgtAFLxDaVQoJg="),
];

fn fixture(rows: &[(i64, i64, &str)]) -> (SourceTree, BTreeMap<i64, AtlasRevision>) {
    let mut entries = vec![("21_pending.sql".into(), "SELECT 1;".into())];
    let atlas = rows
        .iter()
        .map(|&(id, executed_at_us, hash)| {
            let slug = match id {
                22 => "first",
                23 => "second",
                24 => "third",
                25 => "fourth",
                27 => "fifth",
                _ => "extra",
            };
            entries.push((
                format!("{id}_{slug}.sql"),
                format!("CREATE TABLE fixture_{id} (id integer);\n"),
            ));
            (
                id,
                AtlasRevision {
                    hash: hash.into(),
                    executed_at_us,
                },
            )
        })
        .collect();
    (SourceTree::from_blobs(entries, None).unwrap(), atlas)
}

fn assert_imports(rows: &[(i64, i64, &str)]) {
    let (tree, atlas) = fixture(rows);
    let report = reconcile(
        &tree.files,
        &BTreeMap::new(),
        &atlas,
        &BTreeMap::new(),
        false,
    )
    .unwrap();
    assert_eq!(
        report.imports,
        atlas
            .keys()
            .map(|&id| (id, Source::Atlas))
            .collect::<Vec<_>>()
    );
    assert_eq!(report.applied.len(), rows.len());
    assert_eq!(report.pending.len(), 1);
    assert_eq!(report.pending[0].id, "21");
}

#[test]
fn atlas_linear_history_ignores_pending_files() {
    assert_imports(&[
        (22, 1, FIRST),
        (23, 2, SECOND),
        (24, 3, THIRD_LINEAR),
        (25, 3, FOURTH_LINEAR),
        (27, 4, FIFTH_LINEAR),
    ]);
}

#[test]
fn atlas_nonlinear_histories_verify_each_historical_prefix() {
    for rows in [NOTIFY, AGENTS, SANDBOXES] {
        assert_imports(rows);
        for &(id, _, _) in rows {
            let (mut tree, atlas) = fixture(rows);
            tree.files.get_mut(&id).unwrap().sql.replace_range(..1, "c");
            let err = reconcile(
                &tree.files,
                &BTreeMap::new(),
                &atlas,
                &BTreeMap::new(),
                false,
            )
            .err()
            .expect("a one-byte change to any applied SQL file must be rejected");
            assert!(err.contains("Atlas checksum drift"), "{err}");
        }
    }
}

#[test]
fn atlas_rejects_wrong_hashes_and_missing_files() {
    let (mut tree, mut atlas) = fixture(NOTIFY);
    let wrong_hash = atlas[&25].hash.clone();
    let old_hash = std::mem::replace(&mut atlas.get_mut(&24).unwrap().hash, wrong_hash);
    assert_eq!(
        reconcile(
            &tree.files,
            &BTreeMap::new(),
            &atlas,
            &BTreeMap::new(),
            false
        )
        .err()
        .as_deref(),
        Some("Atlas checksum drift or unsupported revision lineage for migration 24")
    );
    atlas.get_mut(&24).unwrap().hash = old_hash;
    tree.files.remove(&24);
    assert_eq!(
        reconcile(
            &tree.files,
            &BTreeMap::new(),
            &atlas,
            &BTreeMap::new(),
            false
        )
        .err()
        .as_deref(),
        Some("legacy migration 24 is missing locally; supply the complete history")
    );
}
