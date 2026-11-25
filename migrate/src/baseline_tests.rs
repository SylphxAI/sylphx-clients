use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::Legacy;
use crate::{lint, SourceTree};

const FILE: &str = "1_immutable.sql";

fn review(sql: &str, findings: &[(usize, &str)], autocommit: bool) -> Value {
    json!([{
        "file": FILE,
        "sha256": format!("{:x}", Sha256::digest(sql.as_bytes())),
        "findings": findings.iter().map(|(line, rule_id)| json!({"line":line,"rule_id":rule_id})).collect::<Vec<_>>(),
        "justification": "Approved fixture: historical bytes already recorded by Atlas",
        "autocommit": autocommit,
    }])
}

fn load(sql: &str, pin: &Value) -> crate::Result<SourceTree> {
    SourceTree::from_blobs(
        vec![(FILE.into(), sql.into())],
        Some(&pin.to_string().into_bytes()),
    )
}

#[test]
fn exact_pins_cover_every_legacy_rule_and_never_cover_ledgers() {
    for (sql, rule) in [
        ("/* unterminated", "MIG001"),
        ("UPDATE t SET x = 1;", "MIG002"),
        ("ALTER TABLE t ADD x int NOT NULL;", "MIG101"),
        ("ALTER TABLE t ALTER x TYPE bigint;", "MIG102"),
        ("ALTER TABLE t ADD CONSTRAINT c CHECK (x > 0);", "MIG103"),
        ("CREATE INDEX i ON t (x);", "MIG104"),
        ("DROP TABLE t;", "MIG105"),
        ("CREATE TABLE t (org_id text);", "MIG106"),
        (
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t (x);",
            "MIG107",
        ),
        ("COMMIT;", "MIG108"),
    ] {
        let pin = review(sql, &[(1, rule)], false);
        let mut tree = load(sql, &pin).unwrap();
        assert!(tree.lint().is_empty(), "{sql}");
        assert!(load(&format!("{sql} "), &pin)
            .err()
            .unwrap()
            .contains("checksum drift"));
        let mut duplicate = pin.clone();
        duplicate[0]["findings"]
            .as_array_mut()
            .unwrap()
            .push(json!({"line":1,"rule_id":rule}));
        assert!(load(sql, &duplicate)
            .err()
            .unwrap()
            .contains("findings differ"));
        // An otherwise identical new migration is still strict.
        let strict = SourceTree::from_blobs(vec![("2_new.sql".into(), sql.into())], None).unwrap();
        tree.files.extend(strict.files);
        assert_eq!(tree.lint()[0].rule_id, rule);
        assert_eq!(tree.lint()[0].file, "2_new.sql");
        let pending = lint::admitted(&tree.files[&1], true);
        assert_eq!(
            pending.diagnostics.len(),
            usize::from(matches!(rule, "MIG001" | "MIG107" | "MIG108"))
        );
        if rule == "MIG105" {
            assert_eq!(
                pending.overrides,
                [pin[0]["justification"].as_str().unwrap()]
            );
        }
    }
    for (sql, findings) in [
        (
            "COMMENT ON TABLE public._sylphx_migrations IS 'no';",
            vec![(1, "MIG109")],
        ),
        (
            "DROP TABLE public._sylphx_migrations;\n/* unfinished",
            vec![(1, "MIG109"), (2, "MIG001")],
        ),
    ] {
        assert!(load(sql, &review(sql, &findings, false))
            .err()
            .unwrap()
            .contains("not eligible"));
    }
}

#[test]
fn exact_concurrent_replay_shape_requires_opt_in_and_rejects_unrelated_statements() {
    let sql = "SET lock_timeout = 0;\nDROP INDEX CONCURRENTLY IF EXISTS i;\nCREATE INDEX CONCURRENTLY i ON t (x);\nRESET lock_timeout;";
    let findings = [(1, "MIG002"), (2, "MIG107"), (3, "MIG107"), (4, "MIG002")];
    let pinned = load(sql, &review(sql, &findings, true)).unwrap();
    let file = &pinned.files[&1];
    assert_eq!(file.legacy, Legacy::Concurrent);
    let checked = lint::admitted(file, true);
    assert!(checked.concurrent);
    assert!(checked.diagnostics.is_empty());
    assert_eq!(
        checked
            .spans
            .iter()
            .map(|s| &sql[s.clone()])
            .collect::<Vec<_>>(),
        sql.lines()
            .map(|s| s.trim_end_matches(';'))
            .collect::<Vec<_>>()
    );
    let adoption = load(sql, &review(sql, &findings, false)).unwrap();
    assert_eq!(adoption.files[&1].legacy, Legacy::ImportOnly);
    assert_eq!(
        lint::admitted(&adoption.files[&1], true).diagnostics.len(),
        2
    );
    for other in [
        "UPDATE t SET x = 1;",
        "SET search_path = other;",
        "SET LOCAL lock_timeout = '2s';",
    ] {
        let sql = format!("{other}\nCREATE INDEX CONCURRENTLY i ON t(x);");
        assert!(
            load(&sql, &review(&sql, &[(1, "MIG002"), (2, "MIG107")], true))
                .err()
                .unwrap()
                .contains("supported index SQL")
        );
    }
    let sql = "BEGIN;\nCREATE INDEX CONCURRENTLY i ON t(x);\nCOMMIT;";
    assert!(load(
        sql,
        &review(sql, &[(1, "MIG108"), (2, "MIG107"), (3, "MIG108")], true)
    )
    .is_err());
}

#[test]
fn only_a_plain_begin_commit_envelope_is_replaced_by_the_runner_transaction() {
    let sql = "BEGIN;\nCREATE TABLE t (x int);\nCOMMIT;";
    let tree = load(sql, &review(sql, &[(1, "MIG108"), (3, "MIG108")], false)).unwrap();
    let file = &tree.files[&1];
    assert_eq!(file.legacy, Legacy::Envelope);
    let checked = lint::admitted(file, true);
    assert!(checked.diagnostics.is_empty());
    assert!(!checked.concurrent);
    assert_eq!(checked.spans.len(), 1);
    assert_eq!(&sql[checked.spans[0].clone()], "CREATE TABLE t (x int)");
    let nested = "BEGIN;\nSAVEPOINT s;\nCOMMIT;";
    let tree = load(
        nested,
        &review(
            nested,
            &[(1, "MIG108"), (2, "MIG108"), (3, "MIG108")],
            false,
        ),
    )
    .unwrap();
    let file = &tree.files[&1];
    assert_eq!(file.legacy, Legacy::ImportOnly);
    assert_eq!(lint::admitted(file, true).diagnostics.len(), 3);
}
