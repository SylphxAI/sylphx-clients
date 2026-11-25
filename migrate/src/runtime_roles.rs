//! Opt-in, additive grants for dedicated application logins. Existing grants
//! and default ACLs remain operator-owned; do not grant PUBLIC execute on
//! SECURITY DEFINER routines intended to be private.
use sqlx::{PgConnection, Postgres, Transaction};

use crate::Result;

/// Canonical runtime role list shared by the grant fence and the release seal.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RuntimeRoles(Vec<String>);

impl RuntimeRoles {
    pub fn new(roles: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self> {
        let mut names = Vec::new();
        for role in roles {
            let name = role.as_ref();
            if names.len() == 16 {
                return Err("at most 16 runtime roles are supported".into());
            }
            if !(2..=63).contains(&name.len())
                || !name.as_bytes()[0].is_ascii_lowercase()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                || name.starts_with("pg_")
                || name.starts_with("sylphx_")
                || name.ends_with("_migrator")
                || name.ends_with("_desk_ro")
                || matches!(
                    name,
                    "postgres" | "streaming_replica" | "app" | "replicator" | "public"
                )
            {
                return Err("invalid or reserved runtime role name; use 2–63 lowercase PostgreSQL identifier characters".into());
            }
            names.push(name.to_owned());
        }
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("runtime role names must be distinct".into());
        }
        Ok(Self(names))
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

async fn verified_grantees(db: &mut PgConnection, roles: &RuntimeRoles) -> Result<Vec<String>> {
    let grantees = sqlx::query_scalar::<_, String>(
        "SELECT pg_catalog.quote_ident(r.rolname)
           FROM pg_catalog.pg_roles r
          WHERE r.rolname::text = ANY($1)
            AND r.rolname <> current_user AND r.rolname <> session_user
            AND r.rolcanlogin
            AND NOT pg_catalog.pg_has_role(r.oid, current_user, 'MEMBER')
            AND NOT pg_catalog.has_database_privilege(r.oid, current_database(), 'CREATE')
            AND NOT pg_catalog.has_schema_privilege(r.oid, 'public', 'CREATE')
            AND NOT EXISTS (
                SELECT 1 FROM pg_catalog.pg_roles elevated
                 WHERE (elevated.rolsuper OR elevated.rolbypassrls OR elevated.rolcreaterole
                        OR elevated.rolcreatedb OR elevated.rolreplication)
                   AND pg_catalog.pg_has_role(r.oid, elevated.oid, 'MEMBER')
            )
          ORDER BY r.rolname",
    )
    .bind(roles.as_slice())
    .fetch_all(db)
    .await
    .map_err(|_| "cannot verify PostgreSQL runtime roles")?;
    if grantees.len() != roles.as_slice().len() {
        return Err("runtime role must exist, allow login and lack elevated privileges, migrator membership or CREATE access".into());
    }
    Ok(grantees)
}

pub(super) async fn verify(db: &mut PgConnection, roles: &RuntimeRoles) -> Result<()> {
    if !roles.is_empty() {
        verified_grantees(db, roles).await?;
    }
    Ok(())
}

pub(super) async fn assert_ledger_read_only(
    tx: &mut Transaction<'_, Postgres>,
    roles: &RuntimeRoles,
) -> Result<()> {
    if roles.is_empty() {
        return Ok(());
    }
    let writable: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM pg_catalog.unnest($1::text[]) AS role_name
              WHERE pg_catalog.has_table_privilege(role_name::name, 'public._sylphx_migrations',
                                                  'INSERT, UPDATE, DELETE, TRUNCATE')
                 OR pg_catalog.has_any_column_privilege(role_name::name, 'public._sylphx_migrations',
                                                       'INSERT, UPDATE')
         )",
    )
    .bind(roles.as_slice())
    .fetch_one(&mut **tx)
    .await
    .map_err(|_| "cannot verify migration ledger privileges")?;
    if writable {
        return Err("runtime role has write privileges on the canonical migration ledger".into());
    }
    Ok(())
}

/// Execute only inside the writer transaction. The same ACL and DDL either
/// commit together, or neither does. Never revoke pre-existing application ACLs.
pub(super) async fn sync_runtime_role_grants(
    tx: &mut Transaction<'_, Postgres>,
    roles: &RuntimeRoles,
) -> Result<()> {
    if roles.is_empty() {
        return Ok(());
    }
    let grantees = verified_grantees(tx, roles).await?.join(", ");
    // PostgreSQL quotes identifiers and deparses exact overload identities;
    // fixed catalog predicates select only objects owned by this writer.
    let grants = sqlx::query_scalar::<_, String>(
        "SELECT CASE WHEN c.relkind = 'S'
                  THEN 'GRANT USAGE, SELECT ON SEQUENCE public.'
                  ELSE 'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE public.'
                END || pg_catalog.quote_ident(c.relname)
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_catalog.pg_roles r ON r.oid = c.relowner
          WHERE n.nspname = 'public' AND r.rolname = current_user
            AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
            AND c.relname NOT IN ('_sylphx_migrations', '_sqlx_migrations',
                                  'atlas_schema_revisions', 'schema_migrations')
          UNION ALL
         SELECT CASE WHEN p.prokind = 'p' THEN 'GRANT EXECUTE ON PROCEDURE public.'
                     ELSE 'GRANT EXECUTE ON FUNCTION public.' END
                || pg_catalog.quote_ident(p.proname) || '('
                || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')'
           FROM pg_catalog.pg_proc p
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
           JOIN pg_catalog.pg_roles r ON r.oid = p.proowner
          WHERE n.nspname = 'public' AND r.rolname = current_user
            AND NOT p.prosecdef AND p.prokind IN ('f', 'p')
          ORDER BY 1",
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|_| "cannot enumerate runtime grant targets")?;
    for grant in grants {
        let err = sqlx::query(&format!("{grant} TO {grantees}"))
            .persistent(false)
            .execute(&mut **tx)
            .await;
        err.map_err(|err| {
            let state = err.as_database_error().and_then(|e| e.code());
            format!(
                "cannot synchronize runtime grants (SQLSTATE {})",
                state.as_deref().unwrap_or("unknown")
            )
        })?;
    }
    assert_ledger_read_only(tx, roles).await
}

#[cfg(test)]
#[path = "runtime_roles_tests.rs"]
mod tests;
