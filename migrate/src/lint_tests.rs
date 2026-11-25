use crate::{lint, SourceTree};

fn rules(sql: &str) -> Vec<&'static str> {
    SourceTree::from_blobs(vec![("1_case.sql".into(), sql.into())], None)
        .unwrap()
        .lint()
        .iter()
        .map(|d| d.rule_id)
        .collect()
}

#[test]
fn postgres_fast_defaults_do_not_allow_volatile_or_unresolved_expressions() {
    for sql in [
        "ALTER TABLE t ADD COLUMN IF NOT EXISTS x jsonb NOT NULL DEFAULT '{}'::jsonb;",
        "ALTER TABLE t ADD x pg_catalog.jsonb DEFAULT '[]'::pg_catalog.jsonb NOT NULL;",
        "ALTER TABLE t ADD x timestamptz NOT NULL DEFAULT now();",
        "ALTER TABLE t ADD x pg_catalog.timestamptz DEFAULT pg_catalog.now();",
    ] {
        assert!(rules(sql).is_empty(), "{sql}");
    }
    for sql in [
        "ALTER TABLE t ADD x jsonb NOT NULL DEFAULT '{}'::other.jsonb;",
        "ALTER TABLE t ADD x custom_domain NOT NULL DEFAULT '{}'::jsonb;",
        "ALTER TABLE t ADD x jsonb NOT NULL DEFAULT '{}'::jsonb || jsonb_build_object('x', random());",
        "ALTER TABLE t ADD x jsonb NOT NULL DEFAULT '{}'::jsonb[], ADD y int;",
        "ALTER TABLE t ADD x timestamptz NOT NULL DEFAULT clock_timestamp();",
        "ALTER TABLE t ADD x timestamptz NOT NULL DEFAULT other.now();",
        "ALTER TABLE t ADD x timestamptz NOT NULL DEFAULT now() + random() * interval '1 day';",
        "ALTER TABLE t ADD x text GENERATED ALWAYS AS (id::text) STORED;",
    ] {
        assert_eq!(rules(sql), ["MIG101"], "{sql}");
    }
    assert_eq!(
        rules("ALTER TABLE t ADD x jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (x IS NOT NULL);"),
        ["MIG103"],
        "a fast default does not waive the separate CHECK validation rule"
    );
}

#[test]
fn enum_allowance_is_append_only_and_ignores_literal_keywords() {
    assert_eq!(
        rules("ALTER TYPE public.t ADD VALUE IF NOT EXISTS 'DROP INDEX;';"),
        Vec::<&str>::new()
    );
    for sql in [
        "ALTER TYPE t RENAME VALUE 'old' TO 'new';",
        "ALTER TYPE t ADD ATTRIBUTE x int CASCADE;",
        "ALTER TYPE t ADD VALUE '_sylphx_migrations' BEFORE 'old';",
    ] {
        assert_eq!(rules(sql), ["MIG002"], "{sql}");
    }
    assert_eq!(
        rules("ALTER TYPE public._sqlx_migrations ADD VALUE 'label';"),
        ["MIG109"]
    );
}

#[test]
fn unicode_strings_preserve_statement_boundaries_but_do_not_guess_identifier_identity() {
    let tree = SourceTree::from_blobs(
        vec![(
            "1_case.sql".into(),
            r"COMMENT ON TABLE t IS U&'\0061''; DROP TABLE _sylphx_migrations';
ALTER TABLE t VALIDATE CONSTRAINT y;"
                .into(),
        )],
        None,
    )
    .unwrap();
    let file = &tree.files[&1];
    let spans = lint::check(file).spans;
    assert_eq!(spans.len(), 2);
    assert_eq!(
        &file.sql[spans[1].clone()],
        "ALTER TABLE t VALIDATE CONSTRAINT y"
    );
    assert!(tree.lint().is_empty());
    assert_eq!(
        rules(r"COMMENT ON TABLE t IS U&'\0061'; DROP TABLE t;"),
        ["MIG105"]
    );
    assert_eq!(rules(r#"ALTER TABLE U&"\0074" DROP COLUMN x;"#), ["MIG001"]);
    assert_eq!(rules(r"COMMENT ON TABLE t IS U&'unfinished"), ["MIG001"]);
    assert_eq!(
        rules("DROP TABLE public._sylphx_migrations;\n/* unfinished"),
        ["MIG109", "MIG001"]
    );
}

#[test]
fn existing_index_targets_remain_strict_even_with_guarded_create() {
    assert_eq!(
        rules("CREATE TABLE public.t (x int); CREATE INDEX i ON t(x);"),
        Vec::<&str>::new()
    );
    assert_eq!(
        rules("CREATE TABLE IF NOT EXISTS t (x int); CREATE INDEX i ON t(x);"),
        ["MIG104"]
    );
    assert_eq!(
        rules("CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t(x);"),
        ["MIG107"]
    );
}

#[test]
fn drop_policy_requires_a_statement_local_destructive_review() {
    for sql in [
        "DROP POLICY p ON t;",
        "drop policy if exists p on app.t restrict;",
        "DROP POLICY p ON db.app.select;",
        r#"DROP POLICY "on" ON "App"."table" CASCADE;"#,
    ] {
        assert_eq!(rules(sql), ["MIG105"], "{sql}");
    }
    let sql = "-- sylphx:allow-destructive TICKET-1: replace policy atomically\n\
               DROP POLICY IF EXISTS p ON app.t;\n\
               CREATE POLICY p ON app.t AS RESTRICTIVE FOR ALL USING (true);";
    let tree = SourceTree::from_blobs(vec![("1_case.sql".into(), sql.into())], None).unwrap();
    let checked = lint::check(&tree.files[&1]);
    assert!(checked.diagnostics.is_empty());
    assert!(!checked.concurrent);
    assert_eq!(checked.spans.len(), 2);
    assert_eq!(checked.overrides, ["TICKET-1: replace policy atomically"]);
    assert_eq!(
        rules("-- sylphx:allow-destructive TICKET-1: replace policy\nDROP POLICY p ON t; DROP POLICY q ON t;"),
        ["MIG105"]
    );
    assert_eq!(
        rules("-- sylphx:allow-destructive TICKET-1:\nDROP POLICY p ON t;"),
        ["MIG105"]
    );
}

#[test]
fn drop_policy_does_not_waive_other_lint_rules() {
    for sql in [
        "DROP POLICY;",
        "DROP POLICY IF p ON t;",
        "DROP POLICY p t;",
        "DROP POLICY p ON;",
        "DROP POLICY app.p ON app.t;",
        "DROP POLICY p ON app.;",
        "DROP POLICY p ON a.b.c.d;",
        "DROP POLICY p ON 't';",
        r#"DROP POLICY "" ON t;"#,
        "DROP POLICY p ON SELECT;",
        "DROP POLICY ON ON t;",
        "DROP POLICY p ON t CASCADE RESTRICT;",
        "DROP POLICY p ON t CASCADE DROP TABLE t;",
        "DROP POLICY p ON t, other;",
    ] {
        assert_eq!(rules(sql), ["MIG002"], "{sql}");
        assert_eq!(
            rules(&format!(
                "-- sylphx:allow-destructive TICKET-1: reviewed\n{sql}"
            )),
            ["MIG002"],
            "{sql}"
        );
    }
    assert_eq!(
        rules("-- sylphx:allow-destructive TICKET-1: reviewed\nDROP POLICY p ON t; SELECT 1;"),
        ["MIG002"]
    );
    assert_eq!(
        rules(
            r#"-- sylphx:allow-destructive TICKET-1: reviewed
DROP POLICY p ON public."_sylphx_migrations";"#
        ),
        ["MIG109"]
    );
    assert!(
        rules("ALTER POLICY p ON app.t TO app_reader USING (true) WITH CHECK (true);").is_empty()
    );
}
