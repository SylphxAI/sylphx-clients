//! A Release seal binds the exact source and reconciled pending set to the
//! target. The Job re-derives it under the native writer lock before DDL.
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgConnectOptions, PgConnection};
use url::Url;

use crate::{postgres, runtime_roles, Apply, Report, Result, RuntimeRoles, SourceTree};

#[derive(Serialize)]
pub struct ReleaseTarget {
    host: String,
    port: u16,
    database: String,
    role: String,
    verified_network: bool,
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl ReleaseTarget {
    /// No query overrides of host, user, database or session parameters. For
    /// a Job on a network, require the explicit direct, verified TLS URL.
    pub fn parse(url: &str) -> Result<Self> {
        Self::parse_for(url, false)
    }

    /// The data-migrate Job accepts just one extra connection input: a CA path.
    pub fn parse_for_data_migrate(url: &str) -> Result<Self> {
        Self::parse_for(url, true)
    }

    fn parse_for(url: &str, data_migrate: bool) -> Result<Self> {
        let bad = || {
            "native migration needs an explicit PostgreSQL role, database and direct verify-full URL".to_string()
        };
        let parsed = Url::parse(url).map_err(|_| bad())?;
        let host = parsed
            .host_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        let database = parsed
            .path()
            .strip_prefix('/')
            .filter(|s| identifier(s))
            .ok_or_else(bad)?;
        let role = parsed.username();
        if !matches!(parsed.scheme(), "postgres" | "postgresql")
            || !identifier(role)
            || parsed.fragment().is_some()
        {
            return Err(bad());
        }
        let pairs: Vec<_> = parsed.query_pairs().collect();
        let socket = host == "localhost"
            && matches!(pairs.as_slice(), [(key, path), (ssl, mode)] if key == "host" && path.starts_with('/') && ssl == "sslmode" && mode == "disable");
        let loopback = host == "127.0.0.1"
            && matches!(pairs.as_slice(), [(ssl, mode)] if ssl == "sslmode" && mode == "disable");
        let verified_tls = matches!(pairs.as_slice(), [(ssl, mode)] if ssl == "sslmode" && mode == "verify-full")
            || data_migrate
                && matches!(pairs.as_slice(), [(ssl, mode), (ca, _)] | [(ca, _), (ssl, mode)]
                    if ssl == "sslmode" && mode == "verify-full" && ca == "sslrootcert");
        if !(socket && !data_migrate
            || loopback && !data_migrate
            || (parsed.password().is_some_and(|p| !p.is_empty()) && verified_tls))
        {
            return Err(bad());
        }
        let options = url.parse::<PgConnectOptions>().map_err(|_| bad())?;
        if options.get_port() != parsed.port().unwrap_or(5432)
            || options.get_database() != Some(database)
            || options.get_username() != role
            || options.get_socket().is_some() != socket
            || std::env::var_os("PGOPTIONS").is_some()
        {
            return Err(bad());
        }
        Ok(Self {
            host: if socket {
                pairs[0].1.to_string()
            } else {
                host.to_ascii_lowercase()
            },
            port: parsed.port().unwrap_or(5432),
            database: database.into(),
            role: role.into(),
            verified_network: verified_tls,
        })
    }

    pub fn verified_network(&self) -> bool {
        self.verified_network
    }

    pub fn matches(&self, host: &str, database: &str, role: &str) -> bool {
        self.host == host && self.database == database && self.role == role
    }

    // Only an owned _data_migrate may have CREATE outside public; any foreign
    // _data_migrate is refused. Keep in sync with managed_resource/pg_migrator_role.rs
    // (Identity::safe_catalog).
    pub async fn check_session(&self, conn: &mut PgConnection) -> Result<()> {
        let safe: Option<bool> = sqlx::query_scalar(
            "SELECT current_database() = $1 AND current_user = $2 AND session_user = $2
                 AND NOT pg_is_in_recovery() AND NOT rolsuper AND NOT rolbypassrls
                 AND NOT rolcreaterole AND NOT rolcreatedb
                 AND NOT has_database_privilege(current_database(), 'CREATE')
                 AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace n
                     WHERE n.nspname <> 'public'
                       AND CASE WHEN n.nspname = '_data_migrate' THEN n.nspowner <> r.oid
                                ELSE has_schema_privilege(r.oid, n.oid, 'CREATE') END)
               FROM pg_catalog.pg_roles r WHERE rolname = current_user",
        )
        .bind(&self.database)
        .bind(&self.role)
        .fetch_optional(conn)
        .await
        .map_err(|_| "cannot verify native migration session identity")?;
        if safe != Some(true) {
            return Err(
                "native migration target or dedicated migrator privileges do not match".into(),
            );
        }
        Ok(())
    }
}

pub struct ReleasePlan {
    pub seal: String,
    pub pending: Vec<(String, String)>,
}

impl SourceTree {
    pub fn seal(&self, target: &ReleaseTarget, report: &Report<'_>) -> Result<String> {
        self.seal_with_runtime_roles(target, report, &self.runtime_roles)
    }

    fn seal_with_runtime_roles(
        &self,
        target: &ReleaseTarget,
        report: &Report<'_>,
        roles: &RuntimeRoles,
    ) -> Result<String> {
        let bytes = if roles.is_empty() {
            serde_json::to_vec(&(
                "sylphx-release-migrate-v1",
                target,
                &self.baseline_sha,
                &report.applied,
                &report.pending,
            ))
        } else {
            serde_json::to_vec(&(
                "sylphx-release-migrate-v2",
                target,
                &self.baseline_sha,
                &report.applied,
                &report.pending,
                roles.as_slice(),
            ))
        }
        .map_err(|_| "cannot encode migration plan")?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    /// Reconcile on the authenticated target before classifying pending SQL.
    pub async fn release_plan(
        &self,
        conn: &mut PgConnection,
        target: &ReleaseTarget,
    ) -> Result<ReleasePlan> {
        self.release_plan_with_runtime_roles(conn, target, &self.runtime_roles)
            .await
    }

    /// Plan against an authoritative role list without rebuilding the SQL source tree.
    pub async fn release_plan_with_runtime_roles(
        &self,
        conn: &mut PgConnection,
        target: &ReleaseTarget,
        roles: &RuntimeRoles,
    ) -> Result<ReleasePlan> {
        if self.files.is_empty() {
            return Err("native migration source is empty".into());
        }
        target.check_session(conn).await?;
        runtime_roles::verify(conn, roles).await?;
        let report = self.inspect(conn).await?;
        let pending = self.release_pending(&report)?;
        Ok(ReleasePlan {
            seal: self.seal_with_runtime_roles(target, &report, roles)?,
            pending,
        })
    }

    /// Plan a sealed Release on the canonical PostgreSQL ledger.
    pub async fn release_plan_for_data_migrate(
        &self,
        conn: &mut PgConnection,
        target: &ReleaseTarget,
        roles: &RuntimeRoles,
    ) -> Result<ReleasePlan> {
        if self.files.is_empty() {
            return Err("native migration source is empty".into());
        }
        let (report, seal, _) = postgres::data_migrate::report(conn, self, target, roles).await?;
        Ok(ReleasePlan {
            seal,
            pending: self.release_pending(&report)?,
        })
    }

    pub(super) fn seal_data_migrate(
        &self,
        target: &ReleaseTarget,
        report: &Report<'_>,
        roles: &RuntimeRoles,
        legacy_exists: bool,
        receipts: &std::collections::BTreeMap<i64, String>,
    ) -> Result<String> {
        let seal = self.seal_with_runtime_roles(target, report, roles)?;
        let bytes = serde_json::to_vec(&(
            "sylphx-release-data-migrate-v1",
            seal,
            legacy_exists,
            receipts,
        ))
        .map_err(|_| "cannot encode data-migrate plan")?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn release_pending(&self, report: &Report<'_>) -> Result<Vec<(String, String)>> {
        // Pending SQL must pass the runner's lint before a release can roll.
        report
            .pending
            .iter()
            .map(|file| {
                let m = &self.files[&file.id.parse::<i64>().expect("checked migration ID")];
                if let Some(d) = crate::lint::admitted(m, true).diagnostics.first() {
                    return Err(format!(
                        "native migration lint failed: {} ({})",
                        file.file, d.rule_id
                    ));
                }
                Ok((m.file.clone(), m.sql.clone()))
            })
            .collect()
    }

    /// Revalidate the exact legacy seal without writing. The caller must retain
    /// this dedicated session (and its writer locks) through delegated execution.
    pub async fn verify_data_migrate_release(
        &self,
        db: &mut PgConnection,
        target: &ReleaseTarget,
        expected: &str,
    ) -> Result<bool> {
        postgres::verify_data_migrate_release(db, self, target, expected).await
    }

    pub async fn apply_release(
        &self,
        db: &mut PgConnection,
        target: &ReleaseTarget,
        expected: &str,
    ) -> Result<Apply<'_>> {
        postgres::apply(db, self, Some((target, expected))).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_roles_version_and_bind_the_seal_without_changing_v1() {
        let target = ReleaseTarget {
            host: "127.0.0.1".into(),
            port: 5432,
            database: "fixture".into(),
            role: "writer".into(),
            verified_network: false,
        };
        let mut tree = SourceTree::from_blobs(
            vec![(
                "1_fixture.sql".into(),
                "CREATE TABLE fixture (id bigint);".into(),
            )],
            None,
        )
        .unwrap();
        let report = Report {
            applied: vec![],
            pending: vec![crate::File {
                id: "1".into(),
                file: "1_fixture.sql",
                sha256: &tree.files[&1].sha256,
                source: None,
            }],
            imports: vec![],
        };
        let legacy = tree.seal(&target, &report).unwrap();
        assert_eq!(
            legacy,
            "c9ab6c8a146e5e401448e295004cd20a0ac43e3ace4a4c7b50577e8ce2c29c51"
        );
        tree.runtime_roles = crate::RuntimeRoles::new(["api_runtime"]).unwrap();
        let one = tree.seal(&target, &report).unwrap();
        assert_eq!(
            one,
            "2184ecef12120b20c3d8983678a348d9de250576bf62aae2d900d64d3a49b99d"
        );
        tree.runtime_roles = crate::RuntimeRoles::new(["worker_runtime", "api_runtime"]).unwrap();
        let two = tree.seal(&target, &report).unwrap();
        assert_ne!(one, two);
        tree.runtime_roles = crate::RuntimeRoles::new(["api_runtime", "worker_runtime"]).unwrap();
        assert_eq!(two, tree.seal(&target, &report).unwrap());
    }

    #[test]
    fn target_url_requires_an_unambiguous_verified_network_identity() {
        let url = "postgresql://migrator:fixture@db.example.invalid/build?sslmode=verify-full";
        let target = ReleaseTarget::parse(url).unwrap();
        assert!(target.verified_network());
        assert!(target.matches("db.example.invalid", "build", "migrator"));
        assert!(!target.matches("other.example.invalid", "build", "migrator"));
        assert!(!target.matches("db.example.invalid", "other", "migrator"));
        for invalid in [
            format!("{url}&dbname=other"),
            format!("{url}&hostaddr=127.0.0.1"),
            format!("{url}&sslmode=disable"),
            url.replace("verify-full", "require"),
        ] {
            assert!(ReleaseTarget::parse(&invalid).is_err());
        }
    }
}
