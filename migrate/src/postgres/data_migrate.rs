//! Read-only Postgres adapter for the data-migrate v0.1.0-canary.10 ledger.
use std::collections::{BTreeMap, BTreeSet};

use super::{history, runtime_roles, Connection, PgConnection, ReleaseTarget, Result};
use crate::{File, Report, RuntimeRoles, Source, SourceTree};

pub(crate) async fn report<'a>(
    db: &mut PgConnection,
    tree: &'a SourceTree,
    target: &ReleaseTarget,
    roles: &RuntimeRoles,
) -> Result<(Report<'a>, String, bool)> {
    let mut tx = db
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|_| "cannot start data-migrate ledger snapshot")?;
    let (owns_schema, safe_history) = sqlx::query_as::<_, (bool, Option<bool>)>(
        "SELECT n.nspowner = current_user::pg_catalog.regrole,
                c.relkind = 'r' AND NOT c.relrowsecurity AND c.relowner = n.nspowner
                AND NOT EXISTS (SELECT FROM pg_catalog.pg_inherits WHERE inhparent = c.oid)
           FROM pg_catalog.pg_namespace n LEFT JOIN pg_catalog.pg_class c
             ON c.relnamespace = n.oid AND c.relname = '_data_schema_migrations'
          WHERE n.nspname = '_data_migrate'",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| "cannot inspect data-migrate schema")?
    .ok_or("_data_migrate is missing; pre-create it for the dedicated migrator")?;
    if !owns_schema {
        return Err("_data_migrate must be owned by the dedicated migrator".into());
    }
    if safe_history == Some(false) {
        return Err(
            "_data_migrate history must be an ordinary migrator-owned table without RLS".into(),
        );
    }
    target.check_session(&mut tx).await?;
    runtime_roles::verify(&mut tx, roles).await?;
    if !roles.is_empty() && sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1 FROM pg_catalog.pg_roles r CROSS JOIN pg_catalog.pg_namespace n
            WHERE r.rolname::text = ANY($1) AND n.nspname = '_data_migrate'
              AND (pg_catalog.has_schema_privilege(r.oid, n.oid, 'CREATE') OR EXISTS (
                SELECT 1 FROM pg_catalog.pg_class c WHERE c.relnamespace = n.oid
                 AND c.relkind IN ('r', 'p')
                 AND (pg_catalog.pg_has_role(r.oid, c.relowner, 'MEMBER')
                   OR pg_catalog.has_table_privilege(r.oid, c.oid, 'INSERT, UPDATE, DELETE, TRUNCATE')
                   OR pg_catalog.has_any_column_privilege(r.oid, c.oid, 'INSERT, UPDATE')))))",
    ).bind(roles.as_slice()).fetch_one(&mut *tx).await.map_err(|_| "cannot verify data-migrate runtime role privileges")? {
        return Err("runtime role has write privileges on data-migrate metadata".into());
    }
    // Do not let a source view or RLS hide legacy rows from reconciliation.
    let legacy_oid = sqlx::query_as::<_, (i64, bool)>(
        "SELECT c.oid::bigint, c.relkind = 'r' AND NOT c.relrowsecurity
                AND NOT EXISTS (SELECT FROM pg_catalog.pg_inherits WHERE inhparent = c.oid)
                AND NOT EXISTS (SELECT FROM pg_catalog.aclexplode(c.relacl) WHERE grantee = 0)
                AND NOT EXISTS (SELECT FROM pg_catalog.pg_attribute a,
                                       pg_catalog.aclexplode(a.attacl) acl
                                  WHERE a.attrelid = c.oid AND NOT a.attisdropped AND acl.grantee = 0)
           FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
          WHERE n.nspname = 'public' AND c.relname = '_sylphx_migrations'",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| "cannot inspect Sylphx takeover source")?;
    let legacy_oid = match legacy_oid {
        Some((oid, true)) => Some(oid),
        Some((_, false)) => {
            return Err("Sylphx takeover source must be ordinary, without RLS, inherited rows or PUBLIC privileges".into())
        }
        None => None,
    };
    let (legacy_exists, mut report) = history(&mut tx, &tree.files).await?;
    if legacy_exists != legacy_oid.is_some() {
        return Err("inconsistent Sylphx takeover source visibility".into());
    }
    if !legacy_exists && !report.applied.is_empty() {
        return Err("legacy migration history must be consolidated into public._sylphx_migrations before takeover".into());
    }
    let mut receipts = BTreeMap::new();
    if safe_history == Some(true) {
        for (id, checksum, origin, complete) in sqlx::query_as::<_, (i64, String, String, bool)>(
            "SELECT version, checksum, origin, completed FROM ONLY _data_migrate._data_schema_migrations",
        ).fetch_all(&mut *tx).await.map_err(|_| "cannot read data-migrate history")? {
            if !complete {
                return Err(format!("data-migrate receipt {id} is incomplete"));
            }
            let file = tree.files.get(&id).ok_or_else(|| format!("data-migrate receipt {id} has no source file"))?;
            if file.sha256 != checksum {
                return Err(format!("data-migrate checksum mismatch for migration {id}"));
            }
            if receipts.insert(id, origin).is_some() {
                return Err(format!("duplicate data-migrate receipt {id}"));
            }
        }
    }
    let mut legacy_ids = BTreeSet::new();
    if legacy_exists {
        for (id, name) in sqlx::query_as::<_, (i64, String)>(
            "SELECT id, file FROM ONLY public._sylphx_migrations",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| "cannot read Sylphx takeover filenames")?
        {
            if tree.files.get(&id).is_none_or(|file| file.file != name) {
                return Err(format!(
                    "legacy takeover filename mismatch for migration {id}"
                ));
            }
            if !receipts.is_empty()
                && receipts
                    .get(&id)
                    .is_none_or(|origin| origin != "sha256_takeover")
            {
                return Err(format!(
                    "migration {id} is missing canonical takeover receipt"
                ));
            }
            legacy_ids.insert(id);
        }
        if receipts.is_empty() && !report.pending.is_empty() {
            return Err("initial data-migrate takeover must contain only the exact legacy baseline; ship new migrations separately".into());
        }
        if legacy_ids.is_empty() {
            return Err("data-migrate takeover requires a non-empty legacy ledger".into());
        }
    }
    for (&id, origin) in &receipts {
        let source = match origin.as_str() {
            "sha256_takeover" if legacy_ids.contains(&id) => continue,
            "sha256_takeover" => {
                return Err(format!(
                    "canonical takeover receipt {id} has no legacy receipt"
                ))
            }
            "native" => Source::Sylphx,
            "atlas_takeover" => Source::Atlas,
            "sqlx_takeover" => Source::Sqlx,
            _ => return Err(format!("unknown data-migrate origin for migration {id}")),
        };
        report
            .applied
            .push(File::new(&tree.files[&id], Some(source)));
    }
    report
        .pending
        .retain(|f| !receipts.contains_key(&f.id.parse::<i64>().expect("checked migration ID")));
    report
        .applied
        .sort_by_key(|f| f.id.parse::<i64>().expect("checked migration ID"));
    verify_takeover_evidence(&mut tx, &receipts, &legacy_ids, legacy_oid).await?;
    let seal = tree.seal_data_migrate(target, &report, roles, legacy_oid, &receipts)?;
    tx.commit()
        .await
        .map_err(|_| "cannot close data-migrate ledger snapshot")?;
    Ok((report, seal, legacy_exists))
}

async fn verify_takeover_evidence(
    db: &mut PgConnection,
    receipts: &BTreeMap<i64, String>,
    legacy_ids: &BTreeSet<i64>,
    legacy_oid: Option<i64>,
) -> Result<()> {
    let imported = receipts.values().any(|origin| origin == "sha256_takeover");
    let evidence = sqlx::query_scalar::<_, bool>(
        "SELECT c.relkind = 'r' AND NOT c.relrowsecurity AND c.relowner = n.nspowner
           FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON c.relnamespace = n.oid
          WHERE n.nspname = '_data_migrate' AND c.relname = 'sha256_takeover_evidence'",
    )
    .fetch_optional(&mut *db)
    .await
    .map_err(|_| "cannot inspect data-migrate takeover evidence")?;
    match (imported, evidence) {
        (false, None) => return Ok(()),
        (true, Some(true)) => (),
        _ => return Err("data-migrate takeover evidence is missing, orphaned or unsafe".into()),
    }
    let versions = sqlx::query_scalar::<_, i64>(
        "SELECT version FROM ONLY _data_migrate.sha256_takeover_evidence
          WHERE source_schema = 'public' AND source_table = '_sylphx_migrations'
            AND source_oid::bigint = $1 ORDER BY version",
    )
    .bind(legacy_oid)
    .fetch_all(&mut *db)
    .await
    .map_err(|_| "cannot read data-migrate takeover evidence")?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ONLY _data_migrate.sha256_takeover_evidence")
            .fetch_one(db)
            .await
            .map_err(|_| "cannot count data-migrate takeover evidence")?;
    if count != versions.len() as i64 || !versions.iter().eq(legacy_ids) {
        return Err("data-migrate takeover evidence does not match the current Sylphx source identity and receipts".into());
    }
    Ok(())
}
