use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgConnectOptions, ConnectOptions, Connection, Executor, PgConnection};
use sylphx_migrate::{connect, Apply, SourceTree};

fn forward(sql: &str, pin: Option<&[u8]>) -> sylphx_migrate::Result<SourceTree> {
    SourceTree::from_blobs(vec![("1_legacy_index.sql".into(), sql.into())], pin)
}

fn pin(sql: &str, findings: &[(usize, &str)], autocommit: bool) -> Vec<u8> {
    serde_json::to_vec(&json!([{
        "file": "1_legacy_index.sql",
        "sha256": format!("{:x}", Sha256::digest(sql.as_bytes())),
        "findings": findings.iter().map(|(line,rule_id)| json!({"line":line,"rule_id":rule_id})).collect::<Vec<_>>(),
        "justification": "Fixture for immutable Atlas-era SQL",
        "autocommit": autocommit,
    }])).unwrap()
}

#[tokio::test]
async fn apply_refuses_a_single_byte_of_baseline_drift_before_url_parsing() {
    let dir =
        std::env::temp_dir().join(format!("sylphx-baseline-preflight-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let baseline = dir.join("legacy.json");
    let source = "CREATE INDEX CONCURRENTLY i ON t (x);\nRESET lock_timeout;";
    std::fs::write(
        &baseline,
        pin(source, &[(1, "MIG107"), (2, "MIG002")], true),
    )
    .unwrap();
    std::fs::write(dir.join("1_legacy_index.sql"), source.replace("(x)", "(y)")).unwrap();
    let args = super::command()
        .try_get_matches_from([
            "migrate",
            "apply",
            "--dir",
            dir.to_str().unwrap(),
            "--legacy-lint-baseline",
            baseline.to_str().unwrap(),
        ])
        .unwrap();
    let result = super::run(&args).await;
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(matches!(result, Err(crate::Failure::Usage(e)) if e.contains("checksum drift")));
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL admin URL: SYLPHX_MIGRATE_TEST_DATABASE_URL"]
async fn legacy_autocommit_postgres_proof() {
    let project: PgConnectOptions = std::env::var("SYLPHX_MIGRATE_TEST_DATABASE_URL")
        .expect("disposable admin URL required")
        .parse()
        .unwrap();
    let mut admin = PgConnection::connect_with(&project).await.unwrap();
    let role = format!("s5_migrator_{}", std::process::id());
    admin
        .execute(format!("CREATE ROLE {role} LOGIN NOSUPERUSER NOBYPASSRLS").as_str())
        .await
        .unwrap();
    for case in ["valid", "partial", "envelope"] {
        let db = format!("{role}_{case}");
        admin
            .execute(format!("CREATE DATABASE {db} OWNER {role}").as_str())
            .await
            .unwrap();
        let options = project.clone().database(&db);
        let mut session = connect(options.to_url_lossy().as_str()).await.unwrap();
        session.execute(format!("SET ROLE {role}; CREATE TABLE t (x int); INSERT INTO t VALUES (1), (1); CREATE INDEX i ON t(x);").as_str()).await.unwrap();
        if case == "envelope" {
            let sql = "BEGIN;\nALTER TABLE t ADD y int;\nALTER TABLE missing ADD z int;\nCOMMIT;";
            let review = pin(sql, &[(1, "MIG108"), (4, "MIG108")], false);
            let pinned = forward(sql, Some(&review)).unwrap();
            assert!(pinned.apply(&mut session).await.is_err());
            let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 't'::regclass AND attname = 'y')")
                .fetch_one(&mut session).await.unwrap();
            assert!(
                !exists,
                "runner transaction must roll back earlier DDL in a legacy envelope"
            );
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM public._sylphx_migrations")
                .fetch_one(&mut session)
                .await
                .unwrap();
            assert_eq!(count, 0, "no committed migration receipt");
        } else {
            let unique = if case == "partial" { "UNIQUE " } else { "" };
            // Deliberately no RESET: the runner must restore its session settings.
            let sql = format!("SET lock_timeout = 0;\nDROP INDEX CONCURRENTLY IF EXISTS public.i;\nCREATE {unique}INDEX CONCURRENTLY i ON public.t(x);");
            let review = pin(&sql, &[(1, "MIG002"), (2, "MIG107"), (3, "MIG107")], true);
            let strict = forward(&sql, None).unwrap();
            assert!(matches!(
                strict.apply(&mut session).await.unwrap(),
                Apply::LintFailed(_)
            ));
            let no_ledger: bool =
                sqlx::query_scalar("SELECT to_regclass('public._sylphx_migrations') IS NULL")
                    .fetch_one(&mut session)
                    .await
                    .unwrap();
            assert!(no_ledger);
            // Apply gets a new, unpooled connection as the CLI does.
            session.close().await.unwrap();
            session = connect(options.to_url_lossy().as_str()).await.unwrap();
            session
                .execute(format!("SET ROLE {role}").as_str())
                .await
                .unwrap();
            let pinned = forward(&sql, Some(&review)).unwrap();
            let result = pinned.apply(&mut session).await;
            let valid: bool = sqlx::query_scalar(
                "SELECT indisvalid FROM pg_index WHERE indexrelid = 'public.i'::regclass",
            )
            .fetch_one(&mut session)
            .await
            .unwrap();
            let (done, sha): (bool, String) = sqlx::query_as(
                "SELECT finished_at IS NOT NULL, sha256 FROM public._sylphx_migrations WHERE id=1",
            )
            .fetch_one(&mut session)
            .await
            .unwrap();
            let timeout: String = sqlx::query_scalar("SHOW lock_timeout")
                .fetch_one(&mut session)
                .await
                .unwrap();
            assert_eq!(timeout, "2s");
            assert_eq!(sha, format!("{:x}", Sha256::digest(sql.as_bytes())));
            if case == "partial" {
                assert!(result.err().unwrap().contains("23505"));
                assert!(!done);
                assert!(!valid);
                assert!(pinned
                    .inspect(&mut session)
                    .await
                    .err()
                    .unwrap()
                    .contains("unconfirmed concurrent"));
            } else {
                assert!(matches!(result.unwrap(), Apply::Applied(_)));
                assert!(done && valid);
            }
        }
        session.close().await.unwrap();
        admin
            .execute(format!("DROP DATABASE {db}").as_str())
            .await
            .unwrap();
    }
    admin
        .execute(format!("DROP ROLE {role}").as_str())
        .await
        .unwrap();
}
