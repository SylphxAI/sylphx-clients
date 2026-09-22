//! Credentials and the linked project (spec §3.2, §8.5): where the CLI's key
//! comes from, and the org/project/env defaults it fills into names.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sylphx::{Client, HttpTransport};

/// `~/.config/sylphx/credentials.json` (mode 0600).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
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
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// The nearest `.sylphx/project.json` at or above `start`.
pub fn find_link(start: &Path) -> Option<(PathBuf, Link)> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let p = d.join(".sylphx").join("project.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(link) = serde_json::from_str(&text) {
                return Some((p, link));
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

/// Resolves the key and base URL: flag, then environment, then the stored
/// login.
pub fn client(api_key: Option<String>, base_url: Option<String>) -> Result<Option<Client>, String> {
    let stored = load_credentials();
    let key = api_key
        .or_else(|| {
            std::env::var("SYLPHX_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
        })
        .or(stored.api_key);
    let url = base_url
        .or_else(|| {
            std::env::var("SYLPHX_BASE_URL")
                .ok()
                .filter(|u| !u.is_empty())
        })
        .or(stored.base_url);
    let Some(key) = key else {
        return Ok(None);
    };
    let mut b = HttpTransport::builder().api_key(key);
    if let Some(u) = url {
        b = b.base_url(u);
    }
    if let Some(v) = std::env::var("SYLPHX_API_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
    {
        b = b.api_version(v);
    }
    Ok(Some(Client::new(b.build().map_err(|e| e.to_string())?)))
}
