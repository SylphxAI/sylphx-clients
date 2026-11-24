//! One unpooled server session owns locks, execution, and ledger receipts.
use std::{collections::BTreeMap, time::Duration};

use sqlx::{
    postgres::{PgConnectOptions, PgConnection},
    Connection, Executor, PgExecutor,
};

use super::{
    history::{self, Recorded, Report, Source},
    lint::{self, Diagnostic},
    valid_id, Files, Migration, Result,
};

const LEDGER: &str = "public._sylphx_migrations";
const SQLX: &str = "public._sqlx_migrations";
const ATLAS: [&str; 2] = [
    "atlas_schema_revisions.atlas_schema_revisions",
    "public.atlas_schema_revisions",
];
// Database-local key assigned to the Sylphx migration protocol. This is a
// single stream per DB, not a per-schema lock. Do not use a transaction pooler.
const LOCK: i64 = 0x7379_6c70_6878_6d67;
const ATLAS_DEFAULT_LOCK: i64 = 2_686_470_616;

pub(super) async fn connect(url: &str) -> Result<PgConnection> {
    let options = url
        .parse::<PgConnectOptions>()
        .map_err(|_| "invalid PostgreSQL DATABASE_URL")?;
    if options.get_database().is_none() {
        return Err("DATABASE_URL must name a PostgreSQL database".into());
    }
    let mut db = tokio::time::timeout(
        Duration::from_secs(10),
        PgConnection::connect_with(&options),
    )
    .await
    .map_err(|_| "PostgreSQL connection deadline exceeded")?
    .map_err(|_| "cannot connect to PostgreSQL; verify target, role and TLS settings")?;
    db.execute(
        "SET search_path = public; SET standard_conforming_strings = on;
         SET statement_timeout = '30s'; SET lock_timeout = '2s';
         SET idle_in_transaction_session_timeout = '35s'",
    )
    .await
    .map_err(|_| "cannot establish migration session defaults")?;
    Ok(db)
}

async fn exists(db: &mut PgConnection, name: &str) -> Result<bool> {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(db)
        .await
        .map_err(|_| format!("cannot inspect migration ledger {name}"))
}

async fn history<'a>(db: &mut PgConnection, files: &'a Files) -> Result<(bool, Report<'a>)> {
    let initialized = exists(db, LEDGER).await?;
    let mut canonical = BTreeMap::new();
    if initialized {
        for (id, sha256, source, done) in sqlx::query_as::<_, (i64, String, String, bool)>(
            "SELECT id, sha256, source, finished_at IS NOT NULL FROM public._sylphx_migrations ORDER BY id",
        ).fetch_all(&mut *db).await.map_err(|_| "cannot read Sylphx migration ledger")? {
            if canonical.insert(id, Recorded { sha256, source: Source::parse(&source)?, done }).is_some() {
                return Err(format!("duplicate canonical revision {id}"));
            }
        }
    }
    let mut atlas = BTreeMap::new();
    let mut atlas_found = false;
    for name in ATLAS {
        if !exists(db, name).await? {
            continue;
        }
        if atlas_found {
            return Err(
                "multiple Atlas migration ledgers; cannot determine authoritative lineage".into(),
            );
        }
        atlas_found = true;
        let query = format!(
            "SELECT version, hash, COALESCE(type = 2 AND applied = total AND applied >= 0
                        AND NULLIF(error, '') IS NULL AND NULLIF(error_stmt, '') IS NULL, false)
                        FROM {name} ORDER BY version"
        );
        for (raw, hash, complete) in sqlx::query_as::<_, (String, String, bool)>(&query)
            .fetch_all(&mut *db)
            .await
            .map_err(|_| "cannot read Atlas migration ledger")?
        {
            let id = valid_id(&raw).ok_or("Atlas ledger contains a non-canonical migration ID")?;
            if !complete {
                return Err(format!("Atlas revision {id} is incomplete, baselined or manually resolved; reconcile it before cutover"));
            }
            if atlas.insert(id, hash).is_some() {
                return Err(format!("duplicate Atlas revision {id}"));
            }
        }
    }
    let mut sqlx_history = BTreeMap::new();
    if exists(db, SQLX).await? {
        for (id, checksum, success) in sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
            "SELECT version, checksum, success FROM public._sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut *db)
        .await
        .map_err(|_| "cannot read SQLx migration ledger")?
        {
            if !success {
                return Err(format!(
                    "SQLx revision {id} failed; reconcile it before cutover"
                ));
            }
            if sqlx_history.insert(id, checksum).is_some() {
                return Err(format!("duplicate SQLx revision {id}"));
            }
        }
    }
    Ok((
        initialized,
        history::reconcile(files, &canonical, &atlas, &sqlx_history, initialized)?,
    ))
}

pub(super) async fn inspect<'a>(db: &mut PgConnection, files: &'a Files) -> Result<Report<'a>> {
    let mut tx = db
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|_| "cannot start read-only ledger snapshot")?;
    let (_, report) = history(&mut tx, files).await?;
    tx.commit()
        .await
        .map_err(|_| "cannot close read-only ledger snapshot")?;
    Ok(report)
}

pub(super) enum Apply<'a> {
    Applied(Report<'a>),
    LintFailed(Vec<Diagnostic<'a>>),
}

async fn record<'e>(
    db: impl PgExecutor<'e>,
    file: &Migration,
    source: Source,
    done: bool,
    overrides: &[&str],
) -> Result<()> {
    sqlx::query("INSERT INTO public._sylphx_migrations (id, file, sha256, source, finished_at, destructive_overrides)
                 VALUES ($1, $2, $3, $4, CASE WHEN $5 THEN clock_timestamp() END, $6)")
        .bind(file.id).bind(&file.file).bind(&file.sha256).bind(source.as_str()).bind(done).bind(overrides)
        .execute(db).await.map_err(|_| format!("cannot record migration {} in the canonical ledger", file.id))?;
    Ok(())
}

pub(super) async fn apply<'a>(db: &mut PgConnection, files: &'a Files) -> Result<Apply<'a>> {
    tokio::time::timeout(Duration::from_secs(600), apply_locked(db, files))
        .await
        .map_err(|_| "migration run exceeded 10m; inspect the ledger before retrying".to_string())?
}

async fn apply_locked<'a>(db: &mut PgConnection, files: &'a Files) -> Result<Apply<'a>> {
    // A legacy writer must be fenced before cutover (it doesn't read this
    // ledger). Additionally refuse overlap with a currently running default
    // Atlas process, as well as all Sylphx runner sessions.
    for key in [LOCK, ATLAS_DEFAULT_LOCK] {
        let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(key)
            .fetch_one(&mut *db)
            .await
            .map_err(|_| "cannot acquire migration advisory lock")?;
        if !held {
            return Err(
                "another migration writer holds the advisory lock; retry after it exits".into(),
            );
        }
    }
    let role_safe: bool = sqlx::query_scalar(
        "SELECT NOT rolsuper AND NOT rolbypassrls FROM pg_catalog.pg_roles WHERE rolname = current_user",
    ).fetch_one(&mut *db).await.map_err(|_| "cannot verify PostgreSQL migrator role")?;
    if !role_safe {
        return Err(
            "apply requires a dedicated migrator role without SUPERUSER or BYPASSRLS".into(),
        );
    }
    let mut tx = db
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .map_err(|_| "cannot start migration ledger transaction")?;
    let (initialized, report) = history(&mut tx, files).await?;
    let mut diagnostics = Vec::new();
    let mut pending = Vec::new();
    for m in files
        .values()
        .filter(|m| report.pending.iter().any(|p| p.file == m.file))
    {
        let mut checked = lint::check(m);
        diagnostics.append(&mut checked.diagnostics);
        pending.push((m, checked));
    }
    if !diagnostics.is_empty() {
        return Ok(Apply::LintFailed(diagnostics));
    }
    if !initialized {
        tx.execute(
            "CREATE TABLE public._sylphx_migrations (
            id bigint PRIMARY KEY CHECK (id > 0),
            file text NOT NULL,
            sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
            source text NOT NULL CHECK (source IN ('sylphx','atlas','sqlx','atlas_and_sqlx')),
            started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
            finished_at timestamptz,
            destructive_overrides text[] NOT NULL DEFAULT '{}'
        )",
        )
        .await
        .map_err(|_| "cannot create canonical migration ledger in public")?;
        for (id, source) in report.imports {
            record(&mut *tx, &files[&id], source, true, &[]).await?;
        }
    }
    tx.commit()
        .await
        .map_err(|_| "cannot commit canonical ledger import")?;
    for (m, checked) in pending {
        if checked.concurrent {
            // A concurrent build/drop cannot share a commit with its receipt.
            // Write durable intent *first*, fail closed on ambiguous outcomes,
            // and never auto-retry a potentially invalid/partially built index.
            record(&mut *db, m, Source::Sylphx, false, &checked.overrides).await?;
            for span in checked.spans {
                execute(&mut *db, m, &m.sql[span]).await?;
            }
            let updated = sqlx::query(
                "UPDATE public._sylphx_migrations SET finished_at = clock_timestamp()
                                       WHERE id = $1 AND sha256 = $2 AND finished_at IS NULL",
            )
            .bind(m.id)
            .bind(&m.sha256)
            .execute(&mut *db)
            .await
            .map_err(|_| format!("cannot confirm concurrent migration {}", m.id))?;
            if updated.rows_affected() != 1 {
                return Err(format!(
                    "could not confirm exact receipt for migration {}",
                    m.id
                ));
            }
        } else {
            let mut tx = db
                .begin()
                .await
                .map_err(|_| "cannot start migration transaction")?;
            for span in checked.spans {
                execute(&mut *tx, m, &m.sql[span]).await?;
            }
            record(&mut *tx, m, Source::Sylphx, true, &checked.overrides).await?;
            tx.commit().await.map_err(|_| {
                format!(
                    "cannot confirm commit for migration {}; inspect ledger before retrying",
                    m.id
                )
            })?;
        }
    }
    let (_, report) = history(db, files).await?;
    if !report.pending.is_empty() {
        return Err(
            "migration run finished with pending entries; inspect the target ledger".into(),
        );
    }
    Ok(Apply::Applied(report))
}

async fn execute<'e>(db: impl PgExecutor<'e>, migration: &Migration, sql: &str) -> Result<()> {
    // Extended protocol refuses more than one command per prepared statement.
    // Emit only SQLSTATE: server detail/context may contain customer data.
    sqlx::query(sql)
        .persistent(false)
        .execute(db)
        .await
        .map_err(|err| {
            let state = err.as_database_error().and_then(|e| e.code());
            format!(
                "migration {} ({}) failed (SQLSTATE {})",
                migration.id,
                migration.file,
                state.as_deref().unwrap_or("unknown")
            )
        })?;
    Ok(())
}
