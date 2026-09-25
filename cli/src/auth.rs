//! `sylphx login` without a key: the Kernel Access device grant
//! (access-and-keys.md §7.1, RFC 8628). The CLI asks
//! `POST /v1/access/device/authorize`, the human approves at
//! `https://sylphx.com/device` on their console session, and the poll at
//! `POST /v1/access/device/token` returns an org-wide Access key bound to
//! their role in that org. The key is the one credential; there is no
//! separate session.

use std::time::Duration;

use serde_json::{json, Value};

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("sylphx-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

/// `https://api.sylphx.com` (no trailing slash, no `/v1`).
pub fn api_root(base_url: Option<&str>) -> String {
    // The same API every other command uses: `--base-url`, then
    // `SYLPHX_BASE_URL`, then production.
    let env = std::env::var("SYLPHX_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    base_url
        .or(env.as_deref())
        .unwrap_or("https://api.sylphx.com")
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string()
}

/// This machine's label in the key list (`cli:<host>`).
pub fn host_label() -> String {
    let raw = std::env::var("SYLPHX_DEVICE_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("COMPUTERNAME")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect()
}

/// The key a device approval issued.
#[derive(Debug, Clone)]
pub struct Issued {
    pub api_key: String,
    pub org_slug: String,
    pub key_name: String,
}

fn error_code(v: &Value) -> &str {
    v.get("error")
        .and_then(|e| e.as_str().or_else(|| e.get("code").and_then(Value::as_str)))
        .or_else(|| v.get("code").and_then(Value::as_str))
        .unwrap_or("error")
}

/// Starts the grant, prints where to approve, and polls until the key is
/// issued, the human denies, or the code expires.
pub async fn device_login(api: &str, org: Option<String>) -> Result<Issued, String> {
    let http = http()?;
    let mut body = json!({ "client": "sylphx-cli", "host": host_label() });
    if let Some(o) = &org {
        body["org"] = json!(o);
    }
    let r = http
        .post(format!("{api}/v1/access/device/authorize"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("device authorization: {e}"))?;
    let status = r.status();
    let started: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(format!(
            "device authorization failed ({}, HTTP {})",
            error_code(&started),
            status.as_u16()
        ));
    }
    let device_code = started["device_code"]
        .as_str()
        .ok_or("no device_code from Sylphx")?
        .to_string();
    let user_code = started["user_code"].as_str().unwrap_or("?").to_string();
    let url = started["verification_uri_complete"]
        .as_str()
        .or_else(|| started["verification_uri"].as_str())
        .ok_or("no verification_uri from Sylphx")?
        .to_string();
    let mut interval = started["interval"].as_u64().unwrap_or(5).max(1);
    let expires = started["expires_in"].as_u64().unwrap_or(600);
    eprintln!("To sign in, open this URL in a browser and approve:");
    eprintln!();
    eprintln!("  {url}");
    eprintln!();
    eprintln!("Code: {user_code}  (expires in {} min)", expires / 60);
    let _ = open_browser(&url);

    let deadline = std::time::Instant::now() + Duration::from_secs(expires);
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if std::time::Instant::now() >= deadline {
            return Err("the code expired before it was approved; run `sylphx login` again".into());
        }
        let r = http
            .post(format!("{api}/v1/access/device/token"))
            .json(&json!({ "device_code": device_code }))
            .send()
            .await
            .map_err(|e| format!("device token: {e}"))?;
        let ok = r.status().is_success();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if ok {
            let api_key = v["api_key"]
                .as_str()
                .ok_or("no api_key in the answer")?
                .to_string();
            return Ok(Issued {
                api_key,
                org_slug: v["org"]["slug"].as_str().unwrap_or_default().to_string(),
                key_name: v["key"]["name"].as_str().unwrap_or_default().to_string(),
            });
        }
        match error_code(&v) {
            "authorization_pending" => {}
            "slow_down" => interval += 5,
            "access_denied" => return Err("the sign-in was denied".into()),
            "expired_token" => return Err("the code expired; run `sylphx login` again".into()),
            other => return Err(format!("device token rejected ({other})")),
        }
    }
}

/// `sylphx logout`: the key revokes itself (best effort; the local copy is
/// removed either way).
pub async fn revoke_self(api: &str, key: &str) -> Result<(), String> {
    let r = http()?
        .post(format!("{api}/v1/access/keys/self:revoke"))
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if r.status().is_success() || r.status().as_u16() == 401 {
        Ok(())
    } else {
        Err(format!("HTTP {}", r.status().as_u16()))
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    if std::env::var_os("SYLPHX_NO_BROWSER").is_some() {
        return Ok(());
    }
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_root_strips_v1_and_slashes() {
        assert_eq!(
            api_root(Some("https://api.sylphx.com/v1/")),
            "https://api.sylphx.com"
        );
        assert_eq!(api_root(None), "https://api.sylphx.com");
    }

    #[test]
    fn error_codes_read_both_shapes() {
        assert_eq!(error_code(&json!({"error": "slow_down"})), "slow_down");
        assert_eq!(error_code(&json!({"error": {"code": "x"}})), "x");
        assert_eq!(error_code(&json!({"code": "y"})), "y");
    }
}
