//! Credentials and the linked project (spec §3.2, §8.5): where the CLI's key
//! comes from, and the org/project/env defaults it fills into names.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sylphx::{Client, HttpTransport};

/// `~/.config/sylphx/credentials.json` (mode 0600): where the key is, not the
/// key. The key lives in the OS keychain (macOS Keychain, Secret Service /
/// libsecret, Windows Credential Manager). A machine with no keychain keeps it
/// in this file only in a directory the caller chose (`SYLPHX_CONFIG_DIR`) or
/// with `sylphx login --store-file`, and `sylphx login` says so.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// Set only when no keychain was available (`store = "file"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `keychain` or `file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// The key's Resource name, for `whoami` and the console's key list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
    /// Device-login keys, each restricted to one organization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orgs: Vec<OrgCredential>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_org: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgCredential {
    pub id: String,
    pub slug: String,
    pub key_name: String,
    pub store: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

fn org_entry(base_url: Option<&str>, id: &str) -> Option<keyring::Entry> {
    if std::env::var_os("SYLPHX_NO_KEYCHAIN").is_some() {
        return None;
    }
    keyring::Entry::new(
        KEYCHAIN_SERVICE,
        &format!("{}#{id}", keychain_account(base_url)),
    )
    .ok()
}

pub fn org_key(c: &Credentials, org: &OrgCredential) -> Option<String> {
    if org.store == "keychain" {
        org_entry(c.base_url.as_deref(), &org.id).and_then(|e| e.get_password().ok())
    } else {
        org.api_key.clone()
    }
}

fn org_id(value: &str) -> &str {
    value
        .strip_prefix("orgs/")
        .unwrap_or(value)
        .split('/')
        .next()
        .unwrap_or(value)
}

pub fn selected_org<'a>(c: &'a Credentials, org: Option<&str>) -> Option<&'a OrgCredential> {
    let selected = org.or(c.active_org.as_deref());
    match selected {
        Some(value) => c
            .orgs
            .iter()
            .find(|o| o.id == org_id(value) || o.slug == org_id(value)),
        None => c.orgs.first(),
    }
}

pub fn use_org(slug: &str) -> Result<(), String> {
    let mut c = load_credentials();
    let org = selected_org(&c, Some(slug)).ok_or_else(|| {
        format!("no stored credential for {slug}; run `sylphx login` and approve that org")
    })?;
    c.active_org = Some(org.id.clone());
    save_credentials(&c)?;
    Ok(())
}

/// Merge every approved organization without discarding other stored logins.
pub fn store_keys(
    base_url: Option<String>,
    keys: &[crate::auth::IssuedKey],
    store_file: bool,
) -> Result<(PathBuf, bool), String> {
    let mut c = load_credentials();
    c.base_url = base_url.or(c.base_url);
    let mut all_keychain = true;
    for key in keys {
        let in_keychain = org_entry(c.base_url.as_deref(), &key.org_id)
            .is_some_and(|e| e.set_password(&key.api_key).is_ok());
        if !in_keychain && !file_store_allowed(store_file) {
            return Err(
                "the OS keychain refused a key; use --store-file or SYLPHX_CONFIG_DIR".into(),
            );
        }
        all_keychain &= in_keychain;
        let credential = OrgCredential {
            id: key.org_id.clone(),
            slug: key.org_slug.clone(),
            key_name: key.key_name.clone(),
            store: if in_keychain { "keychain" } else { "file" }.into(),
            api_key: (!in_keychain).then(|| key.api_key.clone()),
        };
        if let Some(existing) = c.orgs.iter_mut().find(|o| o.id == key.org_id) {
            *existing = credential;
        } else {
            c.orgs.push(credential);
        }
    }
    if c.active_org.is_none() {
        c.active_org = keys.first().map(|k| k.org_id.clone());
    }
    Ok((save_credentials(&c)?, all_keychain))
}

/// Forget just one org; a selected logout must not erase sibling credentials.
pub fn forget_org(c: &Credentials, id: &str) -> Result<(), String> {
    let mut remaining = c.clone();
    if let Some(org) = c.orgs.iter().find(|o| o.id == id) {
        if org.store == "keychain" {
            if let Some(entry) = org_entry(c.base_url.as_deref(), id) {
                let _ = entry.delete_credential();
            }
        }
    }
    remaining.orgs.retain(|o| o.id != id);
    if remaining.active_org.as_deref() == Some(id) {
        remaining.active_org = remaining.orgs.first().map(|o| o.id.clone());
    }
    if let Some(dir) = config_dir() {
        let _ = std::fs::remove_dir_all(dir.join(crate::token::CACHE_DIR));
    }
    if remaining.orgs.is_empty() && remaining.api_key.is_none() && remaining.store.is_none() {
        forget(&remaining)?;
    } else {
        save_credentials(&remaining)?;
    }
    Ok(())
}

const KEYCHAIN_SERVICE: &str = "sylphx";

fn keychain_account(base_url: Option<&str>) -> String {
    crate::auth::api_root(base_url)
}

fn keychain_entry(base_url: Option<&str>) -> Option<keyring::Entry> {
    if std::env::var_os("SYLPHX_NO_KEYCHAIN").is_some() {
        return None;
    }
    keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(base_url)).ok()
}

/// Whether the OS keychain can hold the key: an entry exists or reads as
/// absent (a locked or missing Secret Service, a container, or
/// `SYLPHX_NO_KEYCHAIN` means no).
fn keychain_usable(base_url: Option<&str>) -> bool {
    keychain_entry(base_url)
        .is_some_and(|e| matches!(e.get_password(), Ok(_) | Err(keyring::Error::NoEntry)))
}

/// Where a key may be kept without a keychain: a directory the caller chose
/// (`SYLPHX_CONFIG_DIR`), or the default one only when asked (`--store-file`).
/// The default `~/.config/sylphx` is shared by every process of the user, so a
/// key written there silently becomes every session's identity.
pub fn file_store_allowed(store_file: bool) -> bool {
    store_file || std::env::var_os("SYLPHX_CONFIG_DIR").is_some_and(|d| !d.is_empty())
}

/// Checks, before any key is read or issued, that `sylphx login` can keep it.
pub fn check_key_store(base_url: Option<&str>, store_file: bool) -> Result<(), String> {
    if keychain_usable(base_url) || file_store_allowed(store_file) {
        return Ok(());
    }
    let dir = config_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "~/.config/sylphx".into());
    Err(format!(
        "no OS keychain here, and a key saved in the shared default {dir} would become \
         the identity of every process of this user. Choose one: give the key per process \
         (SYLPHX_API_KEY), keep this login's key in its own directory \
         (SYLPHX_CONFIG_DIR=<dir> sylphx login), or save it in {dir} anyway (--store-file)"
    ))
}

/// Stores `key` in the keychain, else (no keychain) in the 0600 file where
/// `file_store_allowed`; `check_key_store` refuses the rest up front.
pub fn store_key(
    base_url: Option<String>,
    key: &str,
    key_name: Option<String>,
    store_file: bool,
) -> Result<(PathBuf, bool), String> {
    let mut creds = load_credentials();
    let base_url = base_url.or(creds.base_url.clone());
    let in_keychain = keychain_entry(base_url.as_deref())
        .map(|e| e.set_password(key).is_ok())
        .unwrap_or(false);
    if !in_keychain && !file_store_allowed(store_file) {
        return Err(check_key_store(base_url.as_deref(), store_file)
            .err()
            .unwrap_or_else(|| "the OS keychain refused the key".into()));
    }
    creds.api_key = (!in_keychain).then(|| key.to_string());
    creds.base_url = base_url.or(creds.base_url);
    creds.store = Some(if in_keychain { "keychain" } else { "file" }.into());
    creds.key_name = key_name;
    // No active org means the separately supplied/legacy key is the default.
    // `org use` or the next device login selects scoped credentials again.
    creds.active_org = None;
    Ok((save_credentials(&creds)?, in_keychain))
}

/// The separately supplied or old single-login key, wherever it is.
pub fn stored_key(c: &Credentials) -> Option<String> {
    match c.store.as_deref() {
        Some("keychain") => {
            keychain_entry(c.base_url.as_deref()).and_then(|e| e.get_password().ok())
        }
        _ => c.api_key.clone(),
    }
}

/// Remove the separately supplied/legacy key without losing scoped logins.
pub fn forget_legacy(c: &Credentials) -> Result<(), String> {
    if c.store.as_deref() == Some("keychain") {
        if let Some(e) = keychain_entry(c.base_url.as_deref()) {
            let _ = e.delete_credential();
        }
    }
    let mut remaining = c.clone();
    remaining.api_key = None;
    remaining.store = None;
    remaining.key_name = None;
    remaining.active_org = remaining.orgs.first().map(|o| o.id.clone());
    if let Some(dir) = config_dir() {
        let _ = std::fs::remove_dir_all(dir.join(crate::token::CACHE_DIR));
    }
    save_credentials(&remaining)?;
    Ok(())
}

/// Removes every stored credential, token cache, and the credentials file.
pub fn forget(c: &Credentials) -> Result<Option<PathBuf>, String> {
    for org in &c.orgs {
        if org.store == "keychain" {
            if let Some(e) = org_entry(c.base_url.as_deref(), &org.id) {
                let _ = e.delete_credential();
            }
        }
    }
    if c.store.as_deref() == Some("keychain") {
        if let Some(e) = keychain_entry(c.base_url.as_deref()) {
            let _ = e.delete_credential();
        }
    }
    if let Some(d) = config_dir() {
        let _ = std::fs::remove_dir_all(d.join(crate::token::CACHE_DIR));
    }
    match credentials_path().filter(|p| p.exists()) {
        Some(p) => {
            std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            Ok(Some(p))
        }
        None => Ok(None),
    }
}

/// `.sylphx/project.json`: full resource names.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub org: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub env: String,
}

impl Link {
    /// The default for a parent at `level` (1 org, 2 project, 3 env).
    pub fn at(&self, level: usize) -> Option<&str> {
        let v = match level {
            1 => &self.org,
            2 => &self.project,
            3 => &self.env,
            _ => return None,
        };
        (!v.is_empty()).then_some(v.as_str())
    }

    /// From a `whoami` answer.
    pub fn from_whoami(me: &Value) -> Self {
        let s = |k: &str| {
            me.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            org: s("org"),
            project: s("project"),
            env: s("env"),
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("SYLPHX_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("sylphx"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("sylphx"))
}

pub fn credentials_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("credentials.json"))
}

pub fn load_credentials() -> Credentials {
    credentials_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_credentials(c: &Credentials) -> Result<PathBuf, String> {
    let path = credentials_path().ok_or("no home directory: set SYLPHX_CONFIG_DIR")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(c).map_err(|e| e.to_string())?;
    write_private(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(unix)]
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// The variable that names the default environment for this process,
/// before any `.sylphx/project.json`.
pub const ENV_VAR: &str = "SYLPHX_ENVIRONMENT";

impl Link {
    /// The link a full environment name implies:
    /// `orgs/{org}/projects/{project}/envs/{env}`, every segment non-empty.
    pub fn from_env_name(name: &str) -> Option<Self> {
        let parts: Vec<&str> = name.split('/').collect();
        match parts.as_slice() {
            ["orgs", o, "projects", p, "envs", e]
                if !o.is_empty() && !p.is_empty() && !e.is_empty() =>
            {
                Some(Self {
                    org: format!("orgs/{o}"),
                    project: format!("orgs/{o}/projects/{p}"),
                    env: name.to_string(),
                })
            }
            _ => None,
        }
    }
}

/// The default link for commands run in `start`: `SYLPHX_ENVIRONMENT` when it is set
/// (the Vercel CLI model, where `VERCEL_ORG_ID` and `VERCEL_PROJECT_ID`
/// override `.vercel/project.json`, so a CI job or a build wrapper never
/// depends on the checkout's link), else the nearest link file. `None` means
/// neither: the caller falls back to the key's own scope. A set variable of
/// another shape is an error, never silently ignored.
pub fn default_link(start: &Path) -> Result<Option<Link>, String> {
    match std::env::var(ENV_VAR) {
        Ok(v) if !v.trim().is_empty() => Link::from_env_name(v.trim()).map(Some).ok_or_else(|| {
            format!("{ENV_VAR} must be a full environment name, orgs/{{org}}/projects/{{project}}/envs/{{env}}")
        }),
        _ => Ok(find_link(start).map(|(_, l)| l)),
    }
}

/// Keep a linked environment only when it belongs to the command's org.
pub fn default_link_for(start: &Path, org: Option<&str>) -> Result<Option<Link>, String> {
    let link = default_link(start)?;
    let Some(selected) = org else {
        return Ok(link);
    };
    let credentials = load_credentials();
    let selected = selected_org(&credentials, Some(selected))
        .map(|o| o.id.as_str())
        .unwrap_or_else(|| org_id(selected));
    Ok(link.filter(|l| l.org.is_empty() || org_id(&l.org) == selected))
}

/// The nearest `.sylphx/project.json` at or above `start` that names an org,
/// project or env.
///
/// A file in another shape (the older `{orgId, projectId}` link) parses as an
/// empty link; it is not a link, so the walk goes on upward and, with none
/// found, the key's own scope (`whoami`) applies. One warning names the file.
pub fn find_link(start: &Path) -> Option<(PathBuf, Link)> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let p = d.join(".sylphx").join("project.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            match serde_json::from_str::<Link>(&text) {
                Ok(link) if link != Link::default() => return Some((p, link)),
                _ => eprintln!(
                    "warning: ignoring {}: not a link (expected org, project or env)",
                    p.display()
                ),
            }
        }
        dir = d.parent();
    }
    None
}

pub fn write_link(dir: &Path, link: &Link) -> Result<PathBuf, String> {
    let d = dir.join(".sylphx");
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let p = d.join("project.json");
    let text = serde_json::to_string_pretty(link).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

/// The credential a command runs as, and where it came from.
pub struct Resolved {
    pub key: String,
    pub base_url: Option<String>,
    /// Given for this process (`--api-key`, `SYLPHX_API_KEY`), not read from
    /// the stored login.
    pub explicit: bool,
}

/// Explicit credentials override stored logins. Otherwise choose `--org`,
/// the linked/environment org, then the active organization. Legacy single-key
/// credentials keep their original behavior until a device login replaces them.
pub fn resolve(
    api_key: Option<String>,
    base_url: Option<String>,
    org: Option<&str>,
) -> Option<Resolved> {
    let stored = load_credentials();
    let base_url = base_url
        .or_else(|| {
            std::env::var("SYLPHX_BASE_URL")
                .ok()
                .filter(|u| !u.is_empty())
        })
        .or(stored.base_url.clone());
    let given = api_key.or_else(|| {
        std::env::var("SYLPHX_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
    });
    let explicit = given.is_some();
    let key = given.or_else(|| {
        if stored.orgs.is_empty()
            || (org.is_none() && stored.active_org.is_none() && stored.store.is_some())
        {
            return stored_key(&stored);
        }
        let link = if org.is_none() {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| default_link(&cwd).ok().flatten())
        } else {
            None
        };
        let selected = selected_org(&stored, org.or_else(|| link.as_ref().and_then(|l| l.at(1))))?;
        org_key(&stored, selected)
    })?;
    Some(Resolved {
        key,
        base_url,
        explicit,
    })
}

/// A client for a resolved credential.
pub fn client_for(r: &Resolved) -> Result<Client, String> {
    let mut b = HttpTransport::builder().api_key(r.key.clone());
    if let Some(u) = &r.base_url {
        b = b.base_url(u.clone());
    }
    if let Some(v) = std::env::var("SYLPHX_API_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
    {
        b = b.api_version(v);
    }
    Ok(Client::new(b.build().map_err(|e| e.to_string())?))
}

/// Resolves the credential and builds its client (`None` when signed out).
pub async fn client(
    api_key: Option<String>,
    base_url: Option<String>,
    org: Option<&str>,
) -> Result<Option<Client>, String> {
    resolve(api_key, base_url, org)
        .map(|r| client_for(&r))
        .transpose()
}
