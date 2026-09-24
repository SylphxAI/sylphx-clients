//! Human login (`sylphx login` without a key): the RFC 8628 device flow at
//! Sylphx Identity, exchanged at the platform API for a session
//! (`POST /v1/auth/identity-exchange`) that refreshes at `/v1/auth/refresh`.
//! Agents and CI use an Access key instead (`SYLPHX_API_KEY` or
//! `sylphx login --api-key -`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The public CLI client Identity serves when `/v1/auth/cli/config` is not.
const DEFAULT_ISSUER: &str = "https://api.identity.sylphx.com";

/// A stored human session: the platform's access token and its rotating
/// refresh token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub access_expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
}

impl Session {
    /// Refresh 60 s before expiry so a command never outlives its token.
    pub fn stale(&self) -> bool {
        now() + 60 >= self.access_expires_at
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("sylphx-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

/// `https://api.sylphx.com` (no trailing slash, no `/v1`).
pub fn api_root(base_url: Option<&str>) -> String {
    base_url
        .unwrap_or("https://api.sylphx.com")
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string()
}

async fn json_of(r: reqwest::Response, what: &str) -> Result<Value, String> {
    let status = r.status();
    let text = r.text().await.map_err(|e| format!("{what}: {e}"))?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(v);
    }
    let code = v
        .get("error")
        .and_then(|e| e.get("code").or(Some(e)))
        .and_then(Value::as_str)
        .or_else(|| v.get("code").and_then(Value::as_str))
        .unwrap_or("error");
    Err(format!("{what} failed ({code}, HTTP {})", status.as_u16()))
}

fn session_of(v: &Value, org: Option<String>) -> Result<Session, String> {
    let s = |k: &[&str]| {
        k.iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .map(str::to_string)
    };
    let access_token =
        s(&["access_token", "accessToken"]).ok_or("no access_token in the session")?;
    let refresh_token =
        s(&["refresh_token", "refreshToken"]).ok_or("no refresh_token in the session")?;
    let expires_in = v
        .get("expires_in")
        .or_else(|| v.get("expiresIn"))
        .and_then(Value::as_u64)
        .unwrap_or(900);
    Ok(Session {
        access_token,
        refresh_token,
        access_expires_at: now() + expires_in,
        org,
    })
}

/// The device flow, then the exchange. Prints the approval URL and code to
/// stderr and polls until approved or expired.
pub async fn device_login(api: &str, org: Option<String>) -> Result<Session, String> {
    let http = http()?;
    let config: Value = match http.get(format!("{api}/v1/auth/cli/config")).send().await {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let issuer = config["issuer"]
        .as_str()
        .unwrap_or(DEFAULT_ISSUER)
        .trim_end_matches('/')
        .to_string();
    let client_id = config["client_id"]
        .as_str()
        .ok_or("the platform names no CLI login client (GET /v1/auth/cli/config)")?
        .to_string();
    let started = json_of(
        http.post(format!("{issuer}/v1/oauth/device_authorization"))
            .form(&[
                ("client_id", client_id.as_str()),
                ("scope", "openid profile email offline_access"),
            ])
            .send()
            .await
            .map_err(|e| format!("device authorization: {e}"))?,
        "device authorization",
    )
    .await?;
    let device_code = started["device_code"]
        .as_str()
        .ok_or("no device_code from Identity")?
        .to_string();
    let user_code = started["user_code"].as_str().unwrap_or("?").to_string();
    // Identity issues the Account Portal approval page (RFC 8628 §3.3.1).
    let url = started["verification_uri_complete"]
        .as_str()
        .or_else(|| started["verification_uri"].as_str())
        .ok_or("no verification_uri from Identity")?
        .to_string();
    let interval = started["interval"].as_u64().unwrap_or(5).max(1);
    let expires = started["expires_in"].as_u64().unwrap_or(600);
    eprintln!("To sign in, open this URL in a browser and approve:");
    eprintln!();
    eprintln!("  {url}");
    eprintln!();
    eprintln!("Code: {user_code}  (expires in {} min)", expires / 60);
    let _ = open_browser(&url);

    let deadline = now() + expires;
    let mut wait = interval;
    let identity_token = loop {
        if now() > deadline {
            return Err(
                "the device code expired before it was approved; run `sylphx login` again".into(),
            );
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let r = http
            .post(format!("{issuer}/v1/oauth/token"))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id.as_str()),
                ("device_code", device_code.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("device token: {e}"))?;
        let ok = r.status().is_success();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if ok {
            break v["access_token"]
                .as_str()
                .ok_or("Identity approved but returned no access_token")?
                .to_string();
        }
        match v["error"].as_str().unwrap_or("") {
            "authorization_pending" => {}
            "slow_down" => wait += 5,
            "access_denied" => return Err("the sign-in was denied in the browser".into()),
            "expired_token" => {
                return Err("the device code expired; run `sylphx login` again".into())
            }
            other => return Err(format!("device token rejected ({other})")),
        }
    };

    let mut body = json!({ "identity_access_token": identity_token });
    if let Some(o) = &org {
        body["organization_id"] = json!(o);
    }
    let r = http
        .post(format!("{api}/v1/auth/identity-exchange"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("session exchange: {e}"))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = v["code"]
            .as_str()
            .or_else(|| v["error"]["code"].as_str())
            .or_else(|| v["error"].as_str())
            .unwrap_or("error");
        if code == "identity_org_ambiguous" {
            return Err(format!(
                "your account is in several organizations; run `sylphx login --org <id-or-slug>` (one of {})",
                v["orgs"]
            ));
        }
        return Err(format!(
            "session exchange failed ({code}, HTTP {})",
            status.as_u16()
        ));
    }
    session_of(&v, org)
}

/// Rotates a stale session at `/v1/auth/refresh`.
pub async fn refresh(api: &str, s: &Session) -> Result<Session, String> {
    let v = json_of(
        http()?
            .post(format!("{api}/v1/auth/refresh"))
            .json(&json!({ "refresh_token": s.refresh_token }))
            .send()
            .await
            .map_err(|e| format!("session refresh: {e}"))?,
        "session refresh (run `sylphx login` again)",
    )
    .await?;
    session_of(&v, s.org.clone())
}

/// Ends the session server-side (best effort).
pub async fn sign_out(api: &str, s: &Session) {
    if let Ok(h) = http() {
        let _ = h
            .post(format!("{api}/v1/auth/sign-out"))
            .bearer_auth(&s.access_token)
            .json(&json!({ "refresh_token": s.refresh_token }))
            .send()
            .await;
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    if std::env::var_os("SYLPHX_NO_BROWSER").is_some() {
        return Ok(());
    }
    let cmd = if cfg!(target_os = "macos") {
        "open"
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
    fn a_session_parses_both_casings_and_goes_stale() {
        let s = session_of(
            &json!({"accessToken": "a", "refresh_token": "r", "expires_in": 30}),
            None,
        )
        .unwrap();
        assert_eq!(s.access_token, "a");
        assert!(s.stale(), "30 s left is inside the 60 s margin");
        assert!(session_of(&json!({"access_token": "a"}), None).is_err());
    }
}
