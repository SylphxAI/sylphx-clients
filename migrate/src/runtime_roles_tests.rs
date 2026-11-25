use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgConnectOptions, Connection, Executor};

use super::*;
use crate::{Apply, ReleaseTarget, SourceTree};

#[test]
fn canonical_role_names_are_bounded_and_never_normalized_silently() {
    for invalid in [
        "",
        "a",
        "_migrator",
        "pg_foo",
        "sylphx_foo",
        "postgres",
        "streaming_replica",
        "app",
        "replicator",
        "public",
        "foo_migrator",
        "foo_desk_ro",
        "9writer",
        "A_role",
        "a-é",
        "a_role ",
        "a;role",
        r#"a"role"#,
        &"a".repeat(64),
    ] {
        assert!(
            RuntimeRoles::new([invalid]).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(RuntimeRoles::new(["api_runtime", "api_runtime"]).is_err());
    assert!(RuntimeRoles::new((1..=16).map(|i| format!("r{i}"))).is_ok());
    assert!(RuntimeRoles::new((1..=17).map(|i| format!("r{i}"))).is_err());
    assert_eq!(
        RuntimeRoles::new([&"a".repeat(63)]).unwrap().as_slice()[0].len(),
        63
    );
    assert_eq!(
        RuntimeRoles::new(["worker_runtime", "api_runtime"])
            .unwrap()
            .as_slice(),
        ["api_runtime", "worker_runtime"]
    );
}

const FIRST: &str = r#"CREATE TABLE public."Sample Rows" (tenant_id text, payload text);
ALTER TABLE public."Sample Rows" ENABLE ROW LEVEL SECURITY;
ALTER TABLE public."Sample Rows" FORCE ROW LEVEL SECURITY;
CREATE POLICY scoped_rows ON public."Sample Rows" USING (tenant_id = current_setting('fixture.part', true));
CREATE SEQUENCE public."Sample Id";
CREATE FUNCTION public.routine(integer) RETURNS integer LANGUAGE sql SECURITY INVOKER AS $$ SELECT $1 + 1 $$;
CREATE FUNCTION public.routine(text) RETURNS text LANGUAGE sql SECURITY DEFINER AS $$ SELECT $1 $$;
CREATE PROCEDURE public.proc(IN val integer) LANGUAGE sql SECURITY INVOKER AS $$ SELECT val $$;"#;
const CONCURRENT: &str =
    r#"CREATE INDEX CONCURRENTLY sample_rows_tenant ON public."Sample Rows" (tenant_id);"#;

fn source(next: Option<&str>) -> SourceTree {
    let mut blobs = vec![("1_sample.sql".into(), FIRST.into())];
    if let Some(sql) = next {
        blobs.push(("2_next.sql".into(), sql.into()));
    }
    // Current strict lint intentionally excludes explicit sequences/routines.
    // Pin only those exact MIG002 findings; there is no GRANT or REVOKE in SQL.
    let pin = serde_json::to_vec(&json!([{
        "file": "1_sample.sql",
        "sha256": format!("{:x}", Sha256::digest(FIRST.as_bytes())),
        "findings": (5..=8).map(|line| json!({"line": line, "rule_id": "MIG002"})).collect::<Vec<_>>()
    }])).unwrap();
    SourceTree::from_blobs(blobs, Some(&pin)).unwrap()
}

struct Fixture {
    options: PgConnectOptions,
    database: String,
    writer: String,
    api: String,
    worker: String,
    admin: PgConnection,
}

impl Fixture {
    async fn new() -> Self {
        let options: PgConnectOptions = std::env::var("SYLPHX_MIGRATE_TEST_DATABASE_URL")
            .expect("set a local disposable PostgreSQL admin URL with trust authentication")
            .parse()
            .unwrap();
        assert!(
            options.get_socket().is_some() || options.get_host() == "127.0.0.1",
            "fixture requires a local scratch PostgreSQL cluster"
        );
        let prefix = format!("fence_{}", std::process::id());
        let database = format!("{prefix}_db");
        let writer = format!("{prefix}_writer");
        let api = format!("{prefix}_api_runtime");
        let worker = format!("{prefix}_worker_runtime");
        let mut admin = PgConnection::connect_with(&options).await.unwrap();
        for (suffix, flags) in [
            ("writer", "LOGIN"),
            ("api_runtime", "LOGIN"),
            ("worker_runtime", "LOGIN"),
            ("bypass", "LOGIN BYPASSRLS"),
            ("role_admin", "LOGIN CREATEROLE"),
            ("no_login", "NOLOGIN"),
            ("member", "LOGIN"),
            ("elevated_member", "LOGIN"),
            ("db_creator", "LOGIN"),
            ("schema_creator", "LOGIN"),
        ] {
            admin
                .execute(format!("CREATE ROLE {prefix}_{suffix} {flags}").as_str())
                .await
                .unwrap();
        }
        admin
            .execute(format!("CREATE DATABASE {database}").as_str())
            .await
            .unwrap();
        admin
            .execute(
                format!(
                    "GRANT {writer} TO {prefix}_member;
            GRANT {prefix}_bypass TO {prefix}_elevated_member;
            GRANT CREATE ON DATABASE {database} TO {prefix}_db_creator"
                )
                .as_str(),
            )
            .await
            .unwrap();
        let options = options.database(&database);
        let mut setup = PgConnection::connect_with(&options).await.unwrap();
        setup
            .execute(
                format!(
                    "GRANT CREATE ON SCHEMA public TO {writer}, {prefix}_schema_creator;
            CREATE TABLE public.foreign_owner_only (id bigint)"
                )
                .as_str(),
            )
            .await
            .unwrap();
        let mut writer_db = PgConnection::connect_with(&options.clone().username(&writer))
            .await
            .unwrap();
        // The release writer cannot CREATE schemas. Prepare an owned table in a
        // different schema using the admin fixture connection instead.
        setup
            .execute(
                format!(
                    "CREATE SCHEMA private_fixture;
            GRANT CREATE ON SCHEMA private_fixture TO {writer};
            CREATE TABLE private_fixture.writer_only (id bigint);
            ALTER TABLE private_fixture.writer_only OWNER TO {writer};
            REVOKE CREATE ON SCHEMA private_fixture FROM {writer};
            GRANT USAGE ON SCHEMA private_fixture TO {api}"
                )
                .as_str(),
            )
            .await
            .unwrap();
        writer_db
            .execute(
                "ALTER DEFAULT PRIVILEGES REVOKE EXECUTE ON ROUTINES FROM PUBLIC;
            CREATE TABLE public.schema_migrations (id bigint)",
            )
            .await
            .unwrap();
        Self {
            options,
            database,
            writer,
            api,
            worker,
            admin,
        }
    }

    async fn session(&self, role: &str) -> PgConnection {
        let mut session = PgConnection::connect_with(&self.options.clone().username(role))
            .await
            .unwrap();
        session
            .execute(
                "SET search_path = public; SET lock_timeout = '2s'; SET statement_timeout = '10s'",
            )
            .await
            .unwrap();
        session
    }

    fn target(&self) -> ReleaseTarget {
        let mut url = url::Url::parse(&format!(
            "postgresql://{}@127.0.0.1:{}/{}",
            self.writer,
            self.options.get_port(),
            self.database
        ))
        .unwrap();
        if let Some(socket) = self.options.get_socket() {
            url.set_host(Some("localhost")).unwrap();
            url.query_pairs_mut()
                .append_pair("host", socket.to_str().unwrap());
        }
        url.query_pairs_mut().append_pair("sslmode", "disable");
        ReleaseTarget::parse(url.as_str()).unwrap()
    }

    async fn finish(mut self) {
        self.admin
            .execute(format!("DROP DATABASE {} WITH (FORCE)", self.database).as_str())
            .await
            .unwrap();
        let prefix = self.database.trim_end_matches("_db");
        for suffix in [
            "writer",
            "api_runtime",
            "worker_runtime",
            "bypass",
            "role_admin",
            "no_login",
            "member",
            "elevated_member",
            "db_creator",
            "schema_creator",
        ] {
            self.admin
                .execute(format!("DROP ROLE {prefix}_{suffix}").as_str())
                .await
                .unwrap();
        }
    }
}

async fn denied(db: &mut PgConnection, sql: &str) {
    let err = db.execute(sql).await.unwrap_err();
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501"),
        "{sql}: {err}"
    );
}

async fn missing(db: &mut PgConnection, name: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NULL")
        .bind(name)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn prove_preflight(f: &Fixture) {
    let prefix = f.database.trim_end_matches("_db");
    let mut reader = f.session(&f.api).await;
    for suffix in [
        "missing",
        "writer",
        "bypass",
        "role_admin",
        "no_login",
        "member",
        "elevated_member",
        "db_creator",
        "schema_creator",
    ] {
        let roles = RuntimeRoles::new([f.api.clone(), format!("{prefix}_{suffix}")]).unwrap();
        assert!(
            source(None)
                .with_runtime_roles(roles)
                .apply(&mut f.session(&f.writer).await)
                .await
                .err()
                .unwrap()
                .contains("runtime role must exist"),
            "{suffix}"
        );
        assert!(missing(&mut reader, "public._sylphx_migrations").await);
        assert!(missing(&mut reader, r#"public."Sample Rows""#).await);
    }
    let mut writer = f.session(&f.writer).await;
    writer
        .execute(
            format!(
                "ALTER DEFAULT PRIVILEGES GRANT INSERT ON TABLES TO {}",
                f.api
            )
            .as_str(),
        )
        .await
        .unwrap();
    let roles = RuntimeRoles::new([&f.api]).unwrap();
    assert!(source(None)
        .with_runtime_roles(roles)
        .apply(&mut writer)
        .await
        .err()
        .unwrap()
        .contains("write privileges on the canonical migration ledger"));
    assert!(missing(&mut reader, "public._sylphx_migrations").await);
    writer
        .execute(
            format!(
                "ALTER DEFAULT PRIVILEGES REVOKE INSERT ON TABLES FROM {}",
                f.api
            )
            .as_str(),
        )
        .await
        .unwrap();
}

async fn prove_first_commit(f: &Fixture) {
    const FAILS: &str = "CREATE TABLE public.must_rollback (id bigint); ALTER TABLE public.missing ADD COLUMN x int;";
    let target = f.target();
    let tree = source(Some(FAILS)).with_runtime_roles(RuntimeRoles::new([&f.api]).unwrap());
    let mut writer = f.session(&f.writer).await;
    let plan = tree.release_plan(&mut writer, &target).await.unwrap();
    for roles in [
        RuntimeRoles::default(),
        RuntimeRoles::new([&f.worker]).unwrap(),
        RuntimeRoles::new([&f.api, &f.worker]).unwrap(),
    ] {
        let changed = source(Some(FAILS)).with_runtime_roles(roles);
        let mut other = f.session(&f.writer).await;
        assert!(changed
            .apply_release(&mut other, &target, &plan.seal)
            .await
            .err()
            .unwrap()
            .contains("plan changed before apply"));
        assert!(missing(&mut other, "public._sylphx_migrations").await);
    }
    // The later migration fails. The first commit must already have runtime
    // grants; no successful end-of-run reconciliation could have supplied them.
    assert!(tree
        .apply_release(&mut writer, &target, &plan.seal)
        .await
        .err()
        .unwrap()
        .contains("42P01"));
    assert!(missing(&mut writer, "public.must_rollback").await);
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public._sylphx_migrations WHERE finished_at IS NOT NULL",
    )
    .fetch_one(&mut writer)
    .await
    .unwrap();
    assert_eq!(receipts, 1);
}

async fn prove_runtime_access(f: &Fixture) {
    let mut api = f.session(&f.api).await;
    let table = r#"public."Sample Rows""#;
    api.execute(
        format!(
            "SET fixture.part = 'left'; INSERT INTO {table} VALUES ('left', 'kept');
        SET fixture.part = 'right'; INSERT INTO {table} VALUES ('right', 'hidden');
        SET fixture.part = 'left'"
        )
        .as_str(),
    )
    .await
    .unwrap();
    let actual: Vec<String> = sqlx::query_scalar(&format!("SELECT payload FROM {table}"))
        .fetch_all(&mut api)
        .await
        .unwrap();
    assert_eq!(actual, ["kept"]);
    assert_eq!(
        api.execute(format!("UPDATE {table} SET payload = 'x' WHERE tenant_id = 'right'").as_str())
            .await
            .unwrap()
            .rows_affected(),
        0
    );
    assert_eq!(
        api.execute(format!("DELETE FROM {table} WHERE tenant_id = 'right'").as_str())
            .await
            .unwrap()
            .rows_affected(),
        0
    );
    denied(
        &mut api,
        &format!("INSERT INTO {table} VALUES ('right', 'refused')"),
    )
    .await;
    denied(
        &mut api,
        &format!("UPDATE {table} SET tenant_id='right' WHERE tenant_id='left'"),
    )
    .await;
    let mut writer = f.session(&f.writer).await;
    let forced_count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&mut writer)
        .await
        .unwrap();
    assert_eq!(forced_count, 0, "FORCE RLS applies even to the owner");
    let next: i64 = sqlx::query_scalar(r#"SELECT nextval('public."Sample Id"')"#)
        .fetch_one(&mut api)
        .await
        .unwrap();
    assert_eq!(next, 1);
    let value: i32 = sqlx::query_scalar("SELECT public.routine(1)")
        .fetch_one(&mut api)
        .await
        .unwrap();
    assert_eq!(value, 2);
    api.execute("CALL public.proc(1)").await.unwrap();
    for sql in [
        "SELECT * FROM public._sylphx_migrations",
        "INSERT INTO public._sylphx_migrations (id) VALUES (9)",
        "UPDATE public._sylphx_migrations SET id = 8",
        "DELETE FROM public._sylphx_migrations",
        "TRUNCATE public._sylphx_migrations",
        "CREATE TABLE public.denied (id bigint)",
        "SELECT public.routine('safe'::text)",
        "SELECT * FROM public.schema_migrations",
        "SELECT * FROM public.foreign_owner_only",
        "SELECT * FROM private_fixture.writer_only",
    ] {
        denied(&mut api, sql).await;
    }
    denied(&mut api, &format!("TRUNCATE {table}")).await;
}

async fn prove_reconciliation(f: &Fixture) {
    let mut worker = f.session(&f.worker).await;
    denied(
        &mut worker,
        r#"INSERT INTO public."Sample Rows" VALUES ('left', 'worker')"#,
    )
    .await;
    // Zero pending; a new login gets DML without re-running unguarded CREATE SQL.
    for _ in 0..2 {
        let roles = RuntimeRoles::new([&f.api, &f.worker]).unwrap();
        let tree = source(None).with_runtime_roles(roles);
        let mut writer = f.session(&f.writer).await;
        let Apply::Applied(report) = tree.apply(&mut writer).await.unwrap() else {
            panic!("reconciliation linted unexpectedly")
        };
        assert_eq!(report.applied.len(), 1);
        assert!(report.pending.is_empty());
    }
    worker.execute(r#"SET fixture.part = 'left'; INSERT INTO public."Sample Rows" VALUES ('left', 'worker');"#).await.unwrap();
    let mut writer = f.session(&f.writer).await;
    // Explicit reader grants survive reconciliation; column writes do not pass
    // the effective ledger check. Keep the prior application ACLs unchanged.
    writer
        .execute(
            format!(
                "GRANT SELECT, UPDATE(file) ON public._sylphx_migrations TO {}",
                f.worker
            )
            .as_str(),
        )
        .await
        .unwrap();
    let roles = RuntimeRoles::new([&f.worker]).unwrap();
    assert!(source(None)
        .with_runtime_roles(roles)
        .apply(&mut writer)
        .await
        .err()
        .unwrap()
        .contains("write privileges on the canonical migration ledger"));
    writer
        .execute(
            format!(
                "REVOKE UPDATE(file) ON public._sylphx_migrations FROM {}",
                f.worker
            )
            .as_str(),
        )
        .await
        .unwrap();
    writer.close().await.unwrap();
    let mut writer = f.session(&f.writer).await;
    writer
        .execute(
            format!(
                r#"REVOKE INSERT ON TABLE public."Sample Rows" FROM {}"#,
                f.worker
            )
            .as_str(),
        )
        .await
        .unwrap();
    let roles = RuntimeRoles::new([&f.worker]).unwrap();
    let tree = source(Some(CONCURRENT)).with_runtime_roles(roles);
    let Apply::Applied(report) = tree.apply(&mut writer).await.unwrap() else {
        panic!("concurrent migration linted unexpectedly")
    };
    assert_eq!(report.applied.len(), 2);
    assert!(report.pending.is_empty());
    worker.execute(r#"INSERT INTO public."Sample Rows" VALUES ('left', 'after concurrent'); SELECT id FROM public._sylphx_migrations"#).await.unwrap();
}

#[test]
#[ignore = "requires local disposable PostgreSQL admin URL: SYLPHX_MIGRATE_TEST_DATABASE_URL"]
fn runtime_role_postgres_proof() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let f = Fixture::new().await;
            prove_preflight(&f).await;
            prove_first_commit(&f).await;
            prove_runtime_access(&f).await;
            prove_reconciliation(&f).await;
            f.finish().await;
        });
}
