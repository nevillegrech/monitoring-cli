//! Keycloak: the browser device flow, token refresh and revocation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Profile;
use crate::error::{Fail, Res};

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// `offline_access` asks for an offline token: it outlives the browser session and its idle
/// window restarts on every refresh.
const SCOPE: &str = "openid profile email roles offline_access";
const MAX_POLL_INTERVAL: u64 = 30;

pub struct Tokens {
    pub access_token: String,
    /// Keycloak rotates the refresh token on each refresh; the caller stores the new one.
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

#[derive(Deserialize)]
pub struct DeviceFlow {
    pub verification_uri_complete: String,
    pub device_code: String,
    #[serde(default = "five")]
    pub interval: u64,
    #[serde(default = "six_hundred")]
    pub expires_in: u64,
}

fn five() -> u64 {
    5
}

fn six_hundred() -> u64 {
    600
}

fn endpoint(profile: &Profile, tail: &str) -> String {
    format!("{}/realms/{}/protocol/openid-connect/{tail}", profile.oidc_host, profile.realm)
}

fn network(what: &str, e: reqwest::Error) -> Fail {
    Fail::Other(format!("network error {what}: {e}"))
}

pub fn start_device_flow(http: &Client, profile: &Profile) -> Res<DeviceFlow> {
    let response = http
        .post(endpoint(profile, "auth/device"))
        .form(&[("client_id", profile.client_id.as_str()), ("scope", SCOPE)])
        .send()
        .map_err(|e| network("starting the login", e))?;
    if !response.status().is_success() {
        return Err(Fail::Other(format!("the login server answered HTTP {}", response.status().as_u16())));
    }
    response.json().map_err(|e| network("reading the login response", e))
}

/// Wait until the user has approved the login in the browser. Returns the refresh token.
pub fn poll_token(http: &Client, profile: &Profile, flow: &DeviceFlow, interrupted: &AtomicBool) -> Res<String> {
    let deadline = Instant::now() + Duration::from_secs(flow.expires_in);
    let mut interval = flow.interval.max(1);
    while Instant::now() < deadline {
        let wake = Instant::now() + Duration::from_secs(interval);
        while Instant::now() < wake {
            if interrupted.load(Ordering::SeqCst) {
                return Err(Fail::Interrupted);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let response = http
            .post(endpoint(profile, "token"))
            .form(&[("grant_type", DEVICE_GRANT), ("device_code", flow.device_code.as_str()), ("client_id", profile.client_id.as_str())])
            .send()
            .map_err(|e| network("during the login", e))?;
        let ok = response.status().is_success();
        let data: Value = response.json().map_err(|e| network("reading the token response", e))?;
        if ok {
            return data["refresh_token"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| Fail::Other("the login server sent no refresh token".to_string()));
        }
        match data["error"].as_str().unwrap_or("") {
            "authorization_pending" => {}
            "slow_down" => interval = (interval + 5).min(MAX_POLL_INTERVAL),
            "expired_token" => break,
            other => {
                let text = data["error_description"].as_str().unwrap_or(other);
                return Err(Fail::Auth(format!("Authentication failed: {text}")));
            }
        }
    }
    Err(Fail::Auth("Login timed out. Please try again.".to_string()))
}

/// `None` when the server no longer accepts the refresh token.
pub fn refresh(http: &Client, profile: &Profile) -> Res<Option<Tokens>> {
    let token = profile.refresh_token.as_deref().unwrap_or("");
    let response = http
        .post(endpoint(profile, "token"))
        .form(&[("grant_type", "refresh_token"), ("refresh_token", token), ("client_id", profile.client_id.as_str())])
        .send()
        .map_err(|e| network("refreshing the access token", e))?;
    let status = response.status().as_u16();
    if status == 400 || status == 401 {
        return Ok(None);
    }
    if status >= 400 {
        return Err(Fail::Other(format!("the login server answered HTTP {status}")));
    }
    let data: Value = response.json().map_err(|e| network("reading the token response", e))?;
    let access_token =
        data["access_token"].as_str().ok_or_else(|| Fail::Other("the login server sent no access token".to_string()))?.to_string();
    Ok(Some(Tokens {
        access_token,
        refresh_token: data["refresh_token"].as_str().map(str::to_string),
        expires_in: data["expires_in"].as_u64().unwrap_or(60),
    }))
}

pub fn revoke(http: &Client, profile: &Profile) -> Res<()> {
    let token = profile.refresh_token.as_deref().unwrap_or("");
    let response = http
        .post(endpoint(profile, "revoke"))
        .form(&[("token", token), ("token_type_hint", "refresh_token"), ("client_id", profile.client_id.as_str())])
        .send()
        .map_err(|e| network("revoking the token", e))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(Fail::Other(format!("the login server answered HTTP {}", response.status().as_u16())))
    }
}

/// The unverified `typ` claim of a token, read only to choose a warning.
pub fn token_type(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims["typ"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_typ_claim() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"typ":"Offline","sub":"x"}"#);
        assert_eq!(token_type(&format!("h.{payload}.s")).as_deref(), Some("Offline"));
        assert_eq!(token_type("not-a-token"), None);
    }
}
