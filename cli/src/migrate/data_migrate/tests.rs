use std::{cell::Cell, ffi::OsStr};

use super::*;

const TLS_URL: &str = "postgresql://fixture_writer:sample%3B%24word@db.example.invalid:6432/fixture?sslmode=verify-full";

#[test]
fn missing_malformed_or_mismatched_seal_never_reaches_the_launcher() {
    for seal in [None, Some("not-a-seal")] {
        assert!(matches!(required_seal(seal), Err(Failure::Refused(_))));
    }
    let called = Cell::new(false);
    let result = after_seal(
        Err("native migration plan changed before apply; no writes made".into()),
        |_| {
            called.set(true);
            Ok(())
        },
    );
    assert!(matches!(result, Err(Failure::Refused(_))));
    assert!(!called.get());
}

#[test]
fn tls_url_maps_to_argv_and_password_is_in_child_env_only() {
    let conn = ConnectionArgs::parse(TLS_URL).unwrap();
    let cmd = conn.command(BINARY, "apply", Path::new("/release dir"), &[]);
    assert_eq!(cmd.get_program(), OsStr::new(BINARY));
    assert_eq!(
        cmd.get_args().collect::<Vec<_>>(),
        [
            "apply",
            "--dir",
            "/release dir",
            "--addr",
            "db.example.invalid:6432",
            "--user",
            "fixture_writer",
            "--database",
            "fixture",
            "--password-env",
            PASSWORD_ENV,
            "--tls-ca-cert",
            SYSTEM_CA,
            "--tls-server-name",
            "db.example.invalid",
        ]
    );
    assert_eq!(
        cmd.get_envs()
            .find(|(key, _)| *key == PASSWORD_ENV)
            .unwrap()
            .1,
        Some(OsStr::new("sample;$word"))
    );
    for inherited in [JOB_URL, "DATABASE_URL"] {
        assert_eq!(
            cmd.get_envs().find(|(key, _)| *key == inherited).unwrap().1,
            None
        );
    }
}

#[test]
fn explicit_ca_is_forwarded_and_non_verified_urls_are_refused() {
    let fixture = tempfile::tempdir().unwrap();
    let ca = fixture.path().join("ca file.pem");
    fs::write(&ca, "test path only\n").unwrap();
    let mut url = Url::parse(TLS_URL).unwrap();
    url.query_pairs_mut()
        .append_pair("sslrootcert", ca.to_str().unwrap());
    assert!(ReleaseTarget::parse(url.as_str()).is_err());
    assert!(ReleaseTarget::parse_for_data_migrate(url.as_str()).is_ok());
    let conn = ConnectionArgs::parse(url.as_str()).unwrap();
    let cmd = conn.command(BINARY, "import-sha256", Path::new("/migrations"), &[]);
    assert!(cmd.get_args().any(|arg| arg == ca.as_os_str()));
    assert_eq!(
        cmd.get_args().skip(3).take(2).collect::<Vec<_>>(),
        ["--source-table", "public._sylphx_migrations"]
    );
    for url in [
        TLS_URL.replace("verify-full", "require"),
        TLS_URL.replace("verify-full", "disable"),
        format!("{TLS_URL}&sslrootcert=relative.pem"),
        format!("{TLS_URL}&sslrootcert=/missing/file.pem"),
    ] {
        assert!(ConnectionArgs::parse(&url).is_err());
    }
}

#[test]
fn roles_are_repeated_with_no_shell_interpolation() {
    let conn = ConnectionArgs::parse(TLS_URL).unwrap();
    let roles = ["alpha_role".into(), "$(touch /tmp/not-executed);'\"".into()];
    let cmd = conn.command(BINARY, "apply", Path::new("/migrations"), &roles);
    assert_eq!(
        cmd.get_args().skip(3).take(4).collect::<Vec<_>>(),
        [
            "--runtime-role",
            "alpha_role",
            "--runtime-role",
            "$(touch /tmp/not-executed);'\"",
        ]
    );
    // The public CLI additionally refuses hostile role names in runtime_roles().
    assert!(RuntimeRoles::new(&roles).is_err());
}

#[cfg(unix)]
#[test]
fn preserves_nonzero_exit_code() {
    use std::os::unix::process::ExitStatusExt;
    assert!(child_result(ExitStatus::from_raw(0)).is_ok());
    assert!(matches!(
        child_result(ExitStatus::from_raw(23 << 8)),
        Err(Failure::Exit(23))
    ));
    assert!(matches!(
        child_result(ExitStatus::from_raw(15)),
        Err(Failure::Exit(143))
    ));
}

#[tokio::test]
#[ignore = "requires dedicated disposable PostgreSQL 18 TLS and verified canonical binary"]
async fn postgres_takeover_twice() {
    use sqlx::{Connection as _, Executor as _, PgConnection};

    let admin = std::env::var("SYLPHX_MIGRATE_TEST_DATABASE_URL")
        .expect("explicit disposable PostgreSQL fixture URL");
    let admin_url = Url::parse(&admin).unwrap();
    assert_eq!(admin_url.host_str(), Some("127.0.0.1"));
    assert_eq!(admin_url.path(), "/postgres");
    assert!(admin_url.port().is_some());
    let ca = std::env::var("SYLPHX_MIGRATE_TEST_TLS_CA").expect("fixture CA path");
    let binary = PathBuf::from(
        std::env::var_os("SYLPHX_MIGRATE_TEST_DATA_MIGRATE")
            .expect("verified canonical binary path"),
    );
    assert!(binary.is_absolute() && binary.is_file());
    let mut admin_db = PgConnection::connect(&admin).await.unwrap();
    let suffix = std::process::id();
    let db = format!("adapter_fixture_{suffix}");
    let writer = format!("fixture_writer_{suffix}");
    let role = format!("fixture_runtime_{suffix}");
    admin_db.execute(format!("CREATE ROLE {writer} LOGIN PASSWORD 'migration-fixture-only';\nCREATE ROLE {role} LOGIN;").as_str()).await.unwrap();
    admin_db
        .execute(format!("CREATE DATABASE {db}").as_str())
        .await
        .unwrap();
    let mut admin_target = admin_url.clone();
    admin_target.set_path(&format!("/{db}"));
    let mut pg = PgConnection::connect(admin_target.as_str()).await.unwrap();
    let project = tempfile::tempdir().unwrap();
    let baseline_dir = tempfile::tempdir().unwrap();
    let baseline = baseline_dir.path().join("baseline.json");
    fs::write(&baseline, "[]\n").unwrap();
    fs::write(
        project.path().join("202611260001_example.sql"),
        "CREATE TABLE public.example (id bigint PRIMARY KEY);\n",
    )
    .unwrap();
    let mut writer_url = admin_url.clone();
    writer_url.set_username(&writer).unwrap();
    writer_url
        .set_password(Some("migration-fixture-only"))
        .unwrap();
    writer_url.set_path(&format!("/{db}"));
    writer_url.set_query(Some("sslmode=verify-full"));
    writer_url.query_pairs_mut().append_pair("sslrootcert", &ca);
    pg.execute(format!("GRANT USAGE, CREATE ON SCHEMA public TO {writer}").as_str())
        .await
        .unwrap();
    let tree = SourceTree::load(project.path(), Some(&baseline))
        .unwrap()
        .with_runtime_roles(RuntimeRoles::new([&role]).unwrap());
    let mut legacy = sylphx_migrate::connect(writer_url.as_str()).await.unwrap();
    assert!(matches!(
        tree.apply(&mut legacy).await.unwrap(),
        sylphx_migrate::Apply::Applied(_)
    ));
    drop(legacy);
    pg.execute(format!("CREATE SCHEMA _data_migrate AUTHORIZATION {writer}").as_str())
        .await
        .unwrap();
    let target = ReleaseTarget::parse_for_data_migrate(writer_url.as_str()).unwrap();
    let invalid = run_verified(
        &"0".repeat(64),
        writer_url.as_str(),
        project.path(),
        Some(&baseline),
        RuntimeRoles::new([&role]).unwrap(),
        Path::new("/definitely-absent/data-migrate"),
    )
    .await;
    assert!(matches!(invalid, Err(Failure::Refused(ref msg)) if msg.contains("plan changed")));
    let no_ledger: bool =
        sqlx::query_scalar("SELECT to_regclass('_data_migrate._data_schema_migrations') IS NULL")
            .fetch_one(&mut pg)
            .await
            .unwrap();
    assert!(no_ledger, "invalid seal must not start the child");

    for run in 0..2 {
        let mut planner = sylphx_migrate::connect(writer_url.as_str()).await.unwrap();
        let plan = tree
            .release_plan_for_data_migrate(
                &mut planner,
                &target,
                &RuntimeRoles::new([&role]).unwrap(),
            )
            .await
            .unwrap();
        assert!(plan.pending.is_empty());
        drop(planner);
        let legacy_before: String = sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_array(to_jsonb(t), xmin::text) ORDER BY id)::text FROM public._sylphx_migrations t").fetch_one(&mut pg).await.unwrap();
        let snapshots = "SELECT (SELECT jsonb_agg(jsonb_build_array(to_jsonb(t), xmin::text) ORDER BY version) FROM _data_migrate._data_schema_migrations t)::text, (SELECT jsonb_agg(jsonb_build_array(to_jsonb(t), xmin::text) ORDER BY version) FROM _data_migrate.sha256_takeover_evidence t)::text";
        let native_before: Option<(String, String)> = if run == 1 {
            Some(sqlx::query_as(snapshots).fetch_one(&mut pg).await.unwrap())
        } else {
            None
        };
        run_verified(
            &plan.seal,
            writer_url.as_str(),
            project.path(),
            Some(&baseline),
            RuntimeRoles::new([&role]).unwrap(),
            &binary,
        )
        .await
        .unwrap();
        let native_after: (String, String) =
            sqlx::query_as(snapshots).fetch_one(&mut pg).await.unwrap();
        if let Some(native_before) = native_before {
            assert_eq!(
                native_before, native_after,
                "takeover and apply must not change the native or evidence rows"
            );
        }
        let legacy_after: String = sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_array(to_jsonb(t), xmin::text) ORDER BY id)::text FROM public._sylphx_migrations t").fetch_one(&mut pg).await.unwrap();
        assert_eq!(
            legacy_before, legacy_after,
            "legacy ledger stays byte-identical"
        );
        let executable = ConnectionArgs::parse(writer_url.as_str()).unwrap();
        let checked = executable
            .command(&binary, "plan", project.path(), std::slice::from_ref(&role))
            .output()
            .unwrap();
        assert!(checked.status.success(), "canonical plan refused");
        let output = String::from_utf8(checked.stdout).unwrap();
        assert!(output.contains("already_applied=1 pending=0"), "{output}");
        print!("adapter run {}: {output}", run + 1);
    }
    fs::write(
        project.path().join("202611260002_second.sql"),
        "ALTER TABLE public.example ADD COLUMN label text;\n",
    )
    .unwrap();
    let roles = RuntimeRoles::new([&role]).unwrap();
    let next = SourceTree::load(project.path(), Some(&baseline)).unwrap();
    let mut planner = sylphx_migrate::connect(writer_url.as_str()).await.unwrap();
    let plan = next
        .release_plan_for_data_migrate(&mut planner, &target, &roles)
        .await
        .unwrap();
    assert_eq!(
        plan.pending
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["202611260002_second.sql"]
    );
    run_verified(
        &plan.seal,
        writer_url.as_str(),
        project.path(),
        Some(&baseline),
        RuntimeRoles::new([&role]).unwrap(),
        &binary,
    )
    .await
    .unwrap();
    assert!(next
        .release_plan_for_data_migrate(&mut planner, &target, &roles)
        .await
        .unwrap()
        .pending
        .is_empty());
    let written: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM public._sylphx_migrations), (SELECT count(*) FROM _data_migrate._data_schema_migrations)").fetch_one(&mut pg).await.unwrap();
    assert_eq!(written, (1, 2));
    drop(planner);
    drop(pg);
    admin_db
        .execute(format!("DROP DATABASE {db}").as_str())
        .await
        .unwrap();
    admin_db
        .execute(format!("DROP ROLE {writer}, {role}").as_str())
        .await
        .unwrap();
}
