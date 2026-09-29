//! `sylphx token --scope <scope>`: one short-lived token for one scope,
//! derived from the login the CLI already holds, for tools that need a
//! credential (a cargo `credential-provider`, a deploy script).
//!
//! The command never prints the long-lived login key. It mints a child
//! Access key in the credential's own org/project/env with exactly the
//! requested scope and a 15 minute life (`POST …/api_keys`; Access only lets
//! a key grant scopes it holds, access-and-keys.md §5.2), prints its secret
//! and a newline on stdout, and caches it (mode 0600, in the config
//! directory) until 2 minutes before it expires, so a build that calls the
//! provider once per crate mints one key, not hundreds. Everything else goes
//! to stderr.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sylphx::{Client, Error, HttpRequest};

use crate::context;

/// Below the config directory; `sylphx logout` removes it.
pub const CACHE_DIR: &str = "token-cache";
/// How long a minted key lives.
const LIFETIME_SECS: u64 = 15 * 60;
/// A cached key is reused until this long before it expires.
const REUSE_MARGIN_SECS: u64 = 2 * 60;
/// `SYLPHX_API_KEY` itself is printed only if it expires within this.
const SHORT_LIVED_SECS: u64 = 60 * 60;

#[derive(Debug, Serialize, Deserialize)]
struct Cached {
    token: String,
    /// Unix seconds.
    expires_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A scope is `<service>:<action>[:<more>]` of lowercase words, or a
/// wildcard (`*:read`, `*:write`). Whether it is *registered* is Access's
/// call: an unregistered scope is refused by the server at mint time.
pub fn scope_shape_ok(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            *p == "*"
                || (!p.is_empty()
                    && p.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                    }))
        })
}

/// The cache file for one credential, base URL, and scope. The name is a
/// hash, so neither the key nor the scope is readable from the directory.
fn cache_path(key: &str, base_url: Option<&str>, scope: &str) -> Option<PathBuf> {
    Some(
        context::config_dir()?
            .join(CACHE_DIR)
            .join(cache_file_name(key, base_url, scope)),
    )
}

fn cache_file_name(key: &str, base_url: Option<&str>, scope: &str) -> String {
    let mut h = DefaultHasher::new();
    (key, crate::auth::api_root(base_url), scope).hash(&mut h);
    format!("{:016x}.json", h.finish())
}

fn cache_read(path: &Path, at: u64) -> Option<String> {
    let c: Cached = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (c.expires_at > at + REUSE_MARGIN_SECS && !c.token.is_empty()).then_some(c.token)
}

fn cache_write(path: &Path, token: &str, expires_at: u64) {
    // A cache that cannot be written only costs one mint per call.
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let c = Cached {
        token: token.to_string(),
        expires_at,
    };
    if let Ok(text) = serde_json::to_string(&c) {
        let _ = context::write_private(path, text.as_bytes());
    }
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    let r = secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        r / 3600,
        r % 3600 / 60,
        r % 60
    )
}

/// Unix seconds from an RFC 3339 UTC time (`…Z`, optional fraction).
fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let (h, mi, se): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let total = days_from_civil(y, m, day) * 86400 + h * 3600 + mi * 60 + se;
    u64::try_from(total).ok()
}

/// One line for stderr from an SDK error.
fn line(e: &Error) -> String {
    match e {
        Error::Api {
            code,
            status,
            detail,
            ..
        } => format!("{} ({status}): {detail}", code.as_str()),
        other => other.to_string(),
    }
    .replace('\n', " ")
}

fn status_of(e: &Error) -> Option<u16> {
    match e {
        Error::Api { status, .. } => Some(*status),
        _ => None,
    }
}

fn keys_path(me: &Value) -> Result<String, String> {
    let s = |k: &str| me.get(k).and_then(Value::as_str).unwrap_or_default();
    let (org, project, env) = (s("org"), s("project"), s("env"));
    let base = if !env.is_empty() {
        env
    } else if !project.is_empty() {
        return Err("the credential is bound to a project, not an environment".into());
    } else if !org.is_empty() {
        org
    } else {
        return Err("the credential names no org".into());
    };
    Ok(format!("/v1/{}/api_keys", base.trim_start_matches('/')))
}

/// Is a key with this record short-lived enough to hand out itself?
async fn short_lived(client: &Client, me: &Value, at: u64) -> bool {
    let name = me
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !name.starts_with("orgs/") {
        return false;
    }
    let rec: Result<Value, Error> = client
        .call(HttpRequest {
            method: "GET",
            path: format!("/v1/{name}"),
            query: vec![],
            body: None,
            mutation: false,
            origin: None,
            effect_ids: false,
        })
        .await;
    rec.ok()
        .and_then(|v| v["spec"]["expire_time"].as_str().and_then(parse_rfc3339))
        .is_some_and(|exp| exp > at && exp - at <= SHORT_LIVED_SECS)
}

/// Prints the token for `scope`; the caller writes it to stdout. Every error
/// is one line for stderr.
pub async fn token(
    scope: &str,
    api_key: Option<String>,
    base_url: Option<String>,
) -> Result<String, String> {
    if !scope_shape_ok(scope) {
        return Err(format!(
            "`{scope}` is not a scope: expected <service>:<action>, e.g. hosting:deploy"
        ));
    }
    let cred = context::resolve(api_key, base_url).ok_or(
        "not signed in: run `sylphx login`, or set SYLPHX_API_KEY (a key from the CI exchange)",
    )?;
    let at = now();
    let path = cache_path(&cred.key, cred.base_url.as_deref(), scope);
    if let Some(t) = path.as_ref().and_then(|p| cache_read(p, at)) {
        return Ok(t);
    }
    let client = context::client_for(&cred)?;
    let me = client
        .invoke("access.whoami", json!({}))
        .await
        .map_err(|e| format!("whoami: {}", line(&e)))?;
    let keys = keys_path(&me)?;
    let expires_at = at + LIFETIME_SECS;
    let made: Result<Value, Error> = client
        .call(HttpRequest {
            method: "POST",
            path: keys,
            query: vec![],
            body: Some(json!({"spec": {
                "scopes": [scope],
                "label": format!("token:{scope}"),
                "expire_time": rfc3339(expires_at),
            }})),
            mutation: true,
            origin: None,
            effect_ids: false,
        })
        .await;
    match made {
        Ok(v) => {
            let secret = v["status"]["secret"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("Access returned no key secret")?
                .to_string();
            if let Some(p) = &path {
                cache_write(p, &secret, expires_at);
            }
            Ok(secret)
        }
        Err(e) if matches!(status_of(&e), Some(400)) && line(&e).contains("unregistered scope") => {
            Err(format!("`{scope}` is not a registered scope"))
        }
        Err(e) if matches!(status_of(&e), Some(401 | 403)) => {
            // A key that cannot mint may still be the short-lived key a CI
            // exchange issued for exactly this scope.
            if cred.explicit
                && me["scopes"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|s| s.as_str() == Some(scope)))
                && short_lived(&client, &me, at).await
            {
                return Ok(cred.key);
            }
            Err(if cred.explicit {
                format!(
                    "this key cannot issue a {scope} token ({}); it is printed as-is only if it \
                     carries {scope} and expires within 60 minutes",
                    line(&e)
                )
            } else {
                format!(
                    "this login may not grant {scope} ({}); ask an org admin for a role that \
                     includes it, then run `sylphx login` again (a per-scope step-up login is \
                     not available yet)",
                    line(&e)
                )
            })
        }
        Err(e) => Err(format!("could not mint a {scope} token: {}", line(&e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_shapes() {
        for ok in [
            "hosting:deploy",
            "ai:inference",
            "access:keys:write",
            "*:read",
        ] {
            assert!(scope_shape_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "hosting",
            "Hosting:Deploy",
            "a b:c",
            "hosting:",
            ":x",
            "a:b c",
        ] {
            assert!(!scope_shape_ok(bad), "{bad}");
        }
    }

    #[test]
    fn times_round_trip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_rfc3339("1970-01-02T00:00:01.5Z"), Some(86_401));
        for t in [1_000_000_000u64, 1_790_000_000, 4_102_444_800] {
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t));
        }
        assert_eq!(parse_rfc3339("nonsense"), None);
    }

    #[test]
    fn cache_is_reused_until_two_minutes_before_expiry() {
        let dir = std::env::temp_dir().join(format!("sylphx-token-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("t.json");
        cache_write(&p, "tok", 1_000);
        assert_eq!(
            cache_read(&p, 1_000 - REUSE_MARGIN_SECS - 1).as_deref(),
            Some("tok")
        );
        assert_eq!(cache_read(&p, 1_000 - REUSE_MARGIN_SECS), None);
        assert_eq!(cache_read(&p, 2_000), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_live_in_the_credentials_own_binding() {
        assert_eq!(
            keys_path(&json!({"org": "orgs/o", "project": "orgs/o/projects/p", "env": "orgs/o/projects/p/envs/e"})).unwrap(),
            "/v1/orgs/o/projects/p/envs/e/api_keys"
        );
        assert_eq!(
            keys_path(&json!({"org": "orgs/o"})).unwrap(),
            "/v1/orgs/o/api_keys"
        );
        assert!(keys_path(&json!({})).is_err());
    }

    #[test]
    fn the_cache_name_hides_key_and_scope() {
        let a = cache_file_name("sylphx_sk_a", None, "hosting:deploy");
        let b = cache_file_name("sylphx_sk_b", None, "hosting:deploy");
        assert_ne!(a, b);
        assert!(!a.contains("sylphx") && !a.contains("hosting"));
    }
}
