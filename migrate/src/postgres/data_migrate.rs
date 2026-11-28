//! Read-only Postgres adapter for the data-migrate v0.1.0-canary.10 ledger.
use super::{history, runtime_roles, BTreeMap, Connection, PgConnection, ReleaseTarget, Result};
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
    let (legacy_exists, mut report) = history(&mut tx, &tree.files).await?;
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
    let mut legacy_ids = std::collections::BTreeSet::new();
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
    let seal = tree.seal_data_migrate(target, &report, roles, legacy_exists, &receipts)?;
    tx.commit()
        .await
        .map_err(|_| "cannot close data-migrate ledger snapshot")?;
    Ok((report, seal, legacy_exists))
}
