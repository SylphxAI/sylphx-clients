//! Run the PostgreSQL case only with an explicitly disposable cluster.
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha384};
use sqlx::{
    postgres::{PgConnectOptions, PgConnection},
    ConnectOptions, Connection, Executor,
};

struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sylphx-migrate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("sql")).unwrap();
        fs::write(
            dir.join("sylphx.toml"),
            "[database.migrations]\nengine = 'sylphx'\ndir = 'sql'\n",
        )
        .unwrap();
        Self(dir)
    }
    fn write(&self, name: &str, sql: &str) {
        fs::write(self.0.join("sql").join(name), sql).unwrap();
    }
    fn run(&self, cmd: &str, url: Option<&str>) -> Output {
        let mut process = Command::new(env!("CARGO_BIN_EXE_sylphx"));
        process
            .current_dir(&self.0)
            .args(["migrate", cmd])
            .env_remove("DATABASE_URL");
        if let Some(url) = url {
            process.env("DATABASE_URL", url);
        }
        process.output().unwrap()
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn read(out: Output, code: i32) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn lint_rule_ids_and_json_remediation() {
    for (unsafe_sql, expected, safe_sql) in [
        ("ALTER TABLE t ADD COLUMN x int NOT NULL;", vec!["MIG101"], "ALTER TABLE t ADD COLUMN x int;"),
        ("ALTER TABLE t ADD x float NOT NULL DEFAULT random();", vec!["MIG101"], "ALTER TABLE t ADD x int NOT NULL DEFAULT 0;"),
        ("ALTER TABLE t ADD x float DEFAULT random();", vec!["MIG101"], "ALTER TABLE t ADD x float;"),
        ("ALTER TABLE t ALTER COLUMN x SET DATA TYPE bigint;", vec!["MIG102"], "ALTER TABLE t ADD y bigint;"),
        ("ALTER TABLE t ADD CONSTRAINT positive CHECK (x > 0);", vec!["MIG103"], "ALTER TABLE t ADD CONSTRAINT positive CHECK (x > 0) NOT VALID;"),
        ("CREATE UNIQUE INDEX i ON t (x);", vec!["MIG104"], "CREATE UNIQUE INDEX CONCURRENTLY i ON t (x);"),
        ("DROP INDEX i;", vec!["MIG104"], "DROP INDEX CONCURRENTLY i;"),
        ("DROP TABLE t;", vec!["MIG105"], "-- sylphx:allow-destructive DB-123: old readers retired\nDROP TABLE t;"),
        ("ALTER TABLE t DROP COLUMN x;", vec!["MIG105"], "-- sylphx:allow-destructive DB-123: readers retired\nALTER TABLE t DROP COLUMN x;"),
        ("TRUNCATE TABLE t;", vec!["MIG105"], "-- sylphx:allow-destructive DB-123: empty test table\nTRUNCATE TABLE t;"),
        ("CREATE TABLE \"Tenant\" (\"org_id\" uuid); ALTER TABLE \"Tenant\" ENABLE ROW LEVEL SECURITY;", vec!["MIG106"], "CREATE TABLE \"Tenant\" (\"org_id\" uuid); ALTER TABLE public.\"Tenant\" ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;"),
        ("BEGIN;
CREATE INDEX CONCURRENTLY i ON t (x);
COMMIT;", vec!["MIG108", "MIG107", "MIG108"], "CREATE TABLE t (x int); CREATE INDEX i ON t(x); DROP INDEX i;"),
        ("CREATE TABLE copy (id) AS TABLE old_table;", vec!["MIG002"], "CREATE TABLE copy (id int);"),
        ("UPDATE t SET x = 0;", vec!["MIG002"], "COMMENT ON TABLE t IS $note$DROP TABLE t; 'BEGIN'$note$;"),
        ("ALTER TABLE public._sylphx_migrations DROP CONSTRAINT key;", vec!["MIG109"], "/* nested /* fake SQL; */ comment */ ALTER TABLE t VALIDATE CONSTRAINT positive;"),
        ("/* unclosed", vec!["MIG001"], "CREATE TABLE t (id int);")
    ] {
        let project = Project::new();
        project.write("1_example.sql", &format!("-- description\n{unsafe_sql}"));
        let report = read(project.run("lint", None), 1);
        let diagnostics = report["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.iter().map(|d|d["rule_id"].as_str().unwrap()).collect::<Vec<_>>(), expected, "{unsafe_sql}: {report}");
        assert_eq!(diagnostics[0]["line"], 2);
        for item in diagnostics { assert_eq!(item["file"], "1_example.sql"); assert!(!item["hint"].as_str().unwrap().is_empty()); }
        project.write("1_example.sql", safe_sql);
        assert_eq!(read(project.run("lint", None), 0), json!({"diagnostics": []}), "{safe_sql}");
    }
}

#[test]
fn legacy_baseline_pins_exact_bytes_and_findings_without_weakening_new_sql() {
    let project = Project::new();
    let file = "1_reviewed.sql";
    let sql = "DO $body$ BEGIN NULL; END $body$;";
    let sha = |sql: &str| format!("{:x}", Sha256::digest(sql.as_bytes()));
    project.write(file, sql);
    let config = project.0.join("sylphx.toml");
    let baseline = project.0.join("review.json");
    let pin = |sql, line, rule| {
        fs::write(
            &baseline,
            json!([{"file":file, "sha256":sha(sql), "findings":[{"line":line, "rule_id":rule}]}])
                .to_string(),
        )
        .unwrap();
    };
    let lint = || {
        Command::new(env!("CARGO_BIN_EXE_sylphx"))
            .args(["migrate", "lint", "--config"])
            .arg(&config)
            .arg("--legacy-lint-baseline")
            .arg(&baseline)
            .env_remove("DATABASE_URL")
            .output()
            .unwrap()
    };
    assert_eq!(
        read(project.run("lint", None), 1)["diagnostics"][0]["rule_id"],
        "MIG002"
    );
    pin(sql, 1, "MIG002");
    assert_eq!(read(lint(), 0), json!({"diagnostics": []}));
    project.write("2_unreviewed.sql", sql);
    let report = read(lint(), 1);
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 1);
    assert_eq!(report["diagnostics"][0]["file"], "2_unreviewed.sql");
    project.write(file, &format!("{sql} -- changed"));
    let drift = lint();
    assert_eq!(drift.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&drift.stderr).contains("checksum drift"));
    project.write(file, sql);
    pin(sql, 2, "MIG002");
    let mismatch = lint();
    assert_eq!(mismatch.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("findings differ"));
    let destructive = "DROP TABLE example;";
    project.write(file, destructive);
    pin(destructive, 1, "MIG105");
    let unsupported = lint();
    assert_eq!(unsupported.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unsupported.stderr).contains("not eligible"));
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL admin URL: SYLPHX_MIGRATE_TEST_DATABASE_URL"]
async fn postgres_migration_proof() {
    let opts: PgConnectOptions = std::env::var("SYLPHX_MIGRATE_TEST_DATABASE_URL")
        .expect("explicit disposable PostgreSQL URL required")
        .parse()
        .expect("invalid fixture URL");
    let mut admin = PgConnection::connect_with(&opts)
        .await
        .expect("fixture connection");
    let role = format!("slice_migrator_{}", std::process::id());
    // A test-specific role and DBs, using no production catalog or data.
    admin
        .execute(
            format!(
                "CREATE ROLE {role} LOGIN NOSUPERUSER NOBYPASSRLS PASSWORD 'slice-fixture-only'"
            )
            .as_str(),
        )
        .await
        .unwrap();
    for (case, atlas, sqlx) in [
        ("native", false, false),
        ("atlas", true, false),
        ("sqlx", false, true),
        ("union", true, true),
    ] {
        let name = format!("{role}_{case}");
        admin
            .execute(format!("CREATE DATABASE {name} OWNER {role}").as_str())
            .await
            .unwrap();
        let connection = opts
            .clone()
            .database(&name)
            .username(&role)
            .password("slice-fixture-only");
        let url = connection.to_url_lossy();
        let mut pg = PgConnection::connect_with(&connection)
            .await
            .expect("migrator connection");
        let project = Project::new();
        let file = "200_old.sql";
        let old = "CREATE TABLE old_table (id integer PRIMARY KEY);";
        project.write(file, old);
        if atlas || sqlx {
            pg.execute(old).await.unwrap();
        }
        if atlas {
            pg.execute(
                "CREATE SCHEMA atlas_schema_revisions;
                CREATE TABLE atlas_schema_revisions.atlas_schema_revisions (
                version text PRIMARY KEY, hash text NOT NULL, type integer NOT NULL,
                applied integer NOT NULL, total integer NOT NULL, error text, error_stmt text)",
            )
            .await
            .unwrap();
            let mut hash = Sha256::new();
            hash.update(file.as_bytes());
            hash.update(old.as_bytes());
            sqlx::query("INSERT INTO atlas_schema_revisions.atlas_schema_revisions VALUES ('200',$1,2,1,1,NULL,NULL)")
                .bind(STANDARD.encode(hash.finalize())).execute(&mut pg).await.unwrap();
        }
        if sqlx {
            pg.execute("CREATE TABLE _sqlx_migrations (version bigint PRIMARY KEY, checksum bytea NOT NULL, success boolean NOT NULL)").await.unwrap();
            sqlx::query("INSERT INTO _sqlx_migrations VALUES (200,$1,true)")
                .bind(Sha384::digest(old.as_bytes()).to_vec())
                .execute(&mut pg)
                .await
                .unwrap();
        }
        let state = read(project.run("status", Some(url.as_str())), 0);
        assert_eq!(
            state["applied"].as_array().unwrap().len(),
            usize::from(atlas || sqlx)
        );
        let no_ledger: bool =
            sqlx::query_scalar("SELECT to_regclass('public._sylphx_migrations') IS NULL")
                .fetch_one(&mut pg)
                .await
                .unwrap();
        assert!(no_ledger, "status must not write");
        if !atlas && !sqlx {
            read(project.run("apply", Some(url.as_str())), 0);
        }
        project.write("100_lower.sql", "CREATE TABLE lower_arrival (id int);");
        project.write(
            "300_index.sql",
            "CREATE INDEX CONCURRENTLY idx_lower ON lower_arrival (id);",
        );
        let plan = read(project.run("plan", Some(url.as_str())), 0);
        assert_eq!(
            plan.as_array()
                .unwrap()
                .iter()
                .map(|v| v["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["100", "300"]
        );
        let state = read(project.run("apply", Some(url.as_str())), 0);
        assert_eq!(
            state["applied"][1]["source"],
            if atlas && sqlx {
                "atlas_and_sqlx"
            } else if atlas {
                "atlas"
            } else if sqlx {
                "sqlx"
            } else {
                "sylphx"
            }
        );
        assert_eq!(
            state["applied"][1]["sha256"],
            format!("{:x}", Sha256::digest(old.as_bytes()))
        );
        let valid_index: bool = sqlx::query_scalar(
            "SELECT indisvalid FROM pg_index WHERE indexrelid = 'idx_lower'::regclass",
        )
        .fetch_one(&mut pg)
        .await
        .unwrap();
        assert!(valid_index);
        read(project.run("apply", Some(url.as_str())), 0);
        project.write(file, "CREATE TABLE changed (id integer);");
        let out = project.run("apply", Some(url.as_str()));
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr)
            .contains("checksum drift for applied migration 200"));
        pg.close().await.unwrap();
        admin
            .execute(format!("DROP DATABASE {name}").as_str())
            .await
            .unwrap();
    }
    admin
        .execute(format!("DROP ROLE {role}").as_str())
        .await
        .unwrap();
}
