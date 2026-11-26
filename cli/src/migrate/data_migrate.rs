//! Use the sealed legacy inputs; canonical data-migrate owns ledger writes and DDL.
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use percent_encoding::percent_decode_str;
use sylphx_migrate::{ReleaseTarget, RuntimeRoles, SourceTree};
use url::Url;

use crate::Failure;

const BINARY: &str = "/usr/local/bin/data-migrate";
const JOB_URL: &str = "MIGRATOR_DATABASE_URL";
const PASSWORD_ENV: &str = "SYLPHX_DATA_MIGRATE_PASSWORD";
const SYSTEM_CA: &str = "/etc/ssl/certs/ca-certificates.crt";

type Result<T> = std::result::Result<T, Failure>;

pub(super) async fn run(
    seal: Option<&str>,
    dir: &Path,
    baseline: Option<&Path>,
    roles: RuntimeRoles,
) -> Result<()> {
    let seal = required_seal(seal)?;
    let url = std::env::var(JOB_URL)
        .map_err(|_| Failure::Usage(format!("set {JOB_URL} for the intended PostgreSQL target")))?;
    // PGSSLROOTCERT can override SQLx's default; client certificates also
    // change its authentication. The adapter accepts only URL-configured roots.
    if ["PGSSLROOTCERT", "PGSSLCERT", "PGSSLKEY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        return Err(Failure::Refused(
            "data-migrate requires TLS settings in MIGRATOR_DATABASE_URL, not PGSSL environment overrides".into(),
        ));
    }
    run_verified(seal, &url, dir, baseline, roles, Path::new(BINARY)).await
}

async fn run_verified(
    seal: &str,
    url: &str,
    dir: &Path,
    baseline: Option<&Path>,
    roles: RuntimeRoles,
    binary: &Path,
) -> Result<()> {
    let target = ReleaseTarget::parse_for_data_migrate(url).map_err(Failure::Refused)?;
    let connection = ConnectionArgs::parse(url)?;
    let cli_roles = roles.as_slice().to_vec();
    let tree = SourceTree::load(dir, baseline)
        .map_err(Failure::Usage)?
        .with_runtime_roles(roles);
    let mut db = sylphx_migrate::connect(url)
        .await
        .map_err(Failure::Refused)?;
    let verified = tree
        .verify_data_migrate_release(&mut db, &target, seal)
        .await;
    let result = after_seal(verified, |legacy_exists| {
        if !binary.is_file() {
            return Err(Failure::Refused(format!(
                "missing required migration binary {}",
                binary.display()
            )));
        }
        if baseline.is_some() {
            eprintln!(
                "--legacy-lint-baseline is bound by the release seal but ignored by data-migrate"
            );
        }
        // Pass the exact forward SQL bytes loaded for the seal, not a second
        // read of a potentially changed source directory.
        let snapshot = tempfile::tempdir()
            .map_err(|_| Failure::Refused("cannot stage sealed migration files".into()))?;
        for (name, sql) in tree.forward_sql() {
            fs::write(snapshot.path().join(name), sql)
                .map_err(|_| Failure::Refused("cannot write sealed migration files".into()))?;
        }
        if legacy_exists {
            run_child(connection.command(binary, "import-sha256", snapshot.path(), &[]))?;
        }
        run_child(connection.command(binary, "apply", snapshot.path(), &cli_roles))
    });
    // The SQLx session owns both legacy advisory locks until both child
    // processes finish. The canonical engine takes its own writer claim.
    drop(db);
    result
}

fn required_seal(seal: Option<&str>) -> Result<&str> {
    seal.filter(|v| v.len() == 64 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .ok_or_else(|| {
            Failure::Refused("data-migrate requires a valid --release-seal; no writes made".into())
        })
}

fn after_seal<T>(
    verified: std::result::Result<bool, String>,
    execute: impl FnOnce(bool) -> Result<T>,
) -> Result<T> {
    execute(verified.map_err(Failure::Refused)?)
}

struct ConnectionArgs {
    host: String,
    addr: String,
    user: String,
    database: String,
    ca: PathBuf,
    password: String,
}

impl ConnectionArgs {
    fn parse(url: &str) -> Result<Self> {
        let invalid = || Failure::Usage("invalid data-migrate PostgreSQL connection URL".into());
        let url = Url::parse(url).map_err(|_| invalid())?;
        let address_host = url.host_str().ok_or_else(invalid)?;
        let host = address_host.trim_start_matches('[').trim_end_matches(']');
        let password = percent_decode_str(url.password().ok_or_else(invalid)?)
            .decode_utf8()
            .map_err(|_| invalid())?;
        if password.is_empty() || password.contains('\0') {
            return Err(invalid());
        }
        let ca = ca_path(&url)?;
        let port = url.port().unwrap_or(5432);
        if port == 0 {
            return Err(invalid());
        }
        Ok(Self {
            host: host.into(),
            addr: format!("{address_host}:{port}"),
            user: url.username().into(),
            database: url.path().strip_prefix('/').ok_or_else(invalid)?.into(),
            ca,
            password: password.into_owned(),
        })
    }

    fn command(
        &self,
        binary: impl AsRef<Path>,
        action: &str,
        dir: &Path,
        roles: &[String],
    ) -> Command {
        let mut args: Vec<OsString> = vec![action.into(), "--dir".into(), dir.into()];
        if action == "import-sha256" {
            args.extend(["--source-table".into(), "public._sylphx_migrations".into()]);
        } else {
            for role in roles {
                args.extend(["--runtime-role".into(), role.into()]);
            }
        }
        args.extend([
            "--addr".into(),
            self.addr.as_str().into(),
            "--user".into(),
            self.user.as_str().into(),
            "--database".into(),
            self.database.as_str().into(),
            "--password-env".into(),
            PASSWORD_ENV.into(),
            "--tls-ca-cert".into(),
            self.ca.as_os_str().into(),
            "--tls-server-name".into(),
            self.host.as_str().into(),
        ]);
        let mut command = Command::new(binary.as_ref());
        command
            .args(args)
            .env(PASSWORD_ENV, &self.password)
            .env_remove(JOB_URL)
            .env_remove("DATABASE_URL");
        command
    }
}

fn ca_path(url: &Url) -> Result<PathBuf> {
    // Both SQLx webpki-roots and the Debian runtime bundle derive their public
    // roots from Mozilla. The bundles have independent publication cadences.
    let mut ca = PathBuf::from(SYSTEM_CA);
    let mut verify_full = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "sslmode" if value == "verify-full" => verify_full = true,
            "sslrootcert" => ca = PathBuf::from(value.as_ref()),
            _ => {
                return Err(Failure::Usage(
                    "data-migrate requires sslmode=verify-full and only an optional sslrootcert"
                        .into(),
                ))
            }
        }
    }
    if !verify_full {
        return Err(Failure::Usage(
            "data-migrate requires sslmode=verify-full".into(),
        ));
    }
    if !ca.is_absolute() || !ca.is_file() || fs::File::open(&ca).is_err() {
        return Err(Failure::Refused(
            "data-migrate CA must be an absolute path to a readable regular file".into(),
        ));
    }
    Ok(ca)
}

fn run_child(mut command: Command) -> Result<()> {
    let status = command
        .status()
        .map_err(|e| Failure::Refused(format!("cannot execute data-migrate: {e}")))?;
    child_result(status)
}

fn child_result(status: ExitStatus) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    if let Some(code) = status.code().and_then(|code| u8::try_from(code).ok()) {
        return Err(Failure::Exit(code));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(code) = status.signal().and_then(|sig| u8::try_from(128 + sig).ok()) {
            return Err(Failure::Exit(code));
        }
    }
    Err(Failure::Refused(
        "data-migrate exited without a usable status".into(),
    ))
}

#[cfg(test)]
mod tests;
