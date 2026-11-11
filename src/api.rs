//! An authenticated session with the Dedaub API.
//!
//! The access token, the account's entity id and the id of its scratch query are cached in the
//! state directory, so a run of many short commands logs in once and not once per command.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::keccak256;
use reqwest::Method;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth;
use crate::config::{self, Config, Profile, REFRESH_TOKEN_ENV};
use crate::error::{Fail, NOT_LOGGED_IN, Res};

/// Refresh the access token this long before it expires, so no request leaves with a dying one.
const EXPIRY_MARGIN: u64 = 30;
const GET_ATTEMPTS: u32 = 3;
const RETRY_STATUSES: [u16; 4] = [408, 502, 503, 504];
/// A query id pinned from outside, for a sandbox that cannot write the state directory.
pub const QUERY_ID_ENV: &str = "DEDAUB_MONITORING_QUERY_ID";
/// The API runs SQL only in the context of a stored query. This tool keeps one, and never
/// writes SQL into it.
const SCRATCH_FOLDER: &str = "/_cli";
const SCRATCH_NAME: &str = "scratch";
/// The server reads this path template literally.
const FOLDER_PATH: &str = "/api/folder/entity/%7Bentity_id:%20int%7D";

#[derive(Serialize, Deserialize, Default, Clone)]
struct State {
    base_url: String,
    access_token: Option<String>,
    access_expires_at: u64,
    entity_id: Option<i64>,
    username: Option<String>,
    scratch_query_id: Option<i64>,
}

pub struct Reply {
    pub status: u16,
    pub text: String,
}

pub struct Session {
    http: Client,
    pub profile_name: String,
    pub profile: Profile,
    from_env: bool,
    state: State,
    state_path: PathBuf,
    pub interrupted: Arc<AtomicBool>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn http_client() -> Client {
    Client::builder()
        .user_agent(concat!("dedaub-monitoring/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .expect("HTTP client")
}

impl Session {
    pub fn open(profile_arg: Option<&str>, interrupted: Arc<AtomicBool>) -> Res<Self> {
        let config = Config::load()?;
        let env_token = std::env::var(REFRESH_TOKEN_ENV).ok().filter(|t| !t.trim().is_empty());
        let (profile_name, mut profile) = match (&config, profile_arg) {
            (Some(config), name) => {
                let name = name.unwrap_or(&config.default).to_string();
                match config.profiles.get(&name) {
                    Some(profile) => (name, profile.clone()),
                    None if env_token.is_some() && profile_arg.is_none() => (name, Profile::production()),
                    None if profile_arg.is_some() => {
                        return Err(Fail::Auth(format!("Profile '{name}' not found. Run: dedaub-monitoring login")));
                    }
                    None => return Err(Fail::Auth(NOT_LOGGED_IN.to_string())),
                }
            }
            (None, _) if env_token.is_some() => (config::DEFAULT_PROFILE.to_string(), Profile::production()),
            (None, _) => return Err(Fail::Auth(NOT_LOGGED_IN.to_string())),
        };
        let from_env = env_token.is_some();
        if let Some(token) = env_token {
            profile.refresh_token = Some(token.trim().to_string());
        }
        let Some(refresh_token) = profile.refresh_token.clone() else {
            return Err(Fail::Auth(NOT_LOGGED_IN.to_string()));
        };
        // A token from the environment may belong to another account than the stored profile.
        let state_name = if from_env {
            format!("env-{}.json", &alloy_primitives::hex::encode(keccak256(refresh_token.as_bytes()))[..16])
        } else {
            format!("{profile_name}.json")
        };
        let state_path = config::state_dir().join(state_name);
        let mut session = Self { http: http_client(), profile_name, profile, from_env, state: State::default(), state_path, interrupted };
        session.state = session.read_state();
        Ok(session)
    }

    fn read_state(&self) -> State {
        fs::read_to_string(&self.state_path)
            .ok()
            .and_then(|text| serde_json::from_str::<State>(&text).ok())
            .filter(|state| state.base_url == self.profile.base_url)
            .unwrap_or_else(|| State { base_url: self.profile.base_url.clone(), ..State::default() })
    }

    /// Best effort: a sandbox may forbid writing here, and then every command starts cold.
    fn write_state(&self) {
        if let Some(dir) = self.state_path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let text = serde_json::to_vec(&self.state).expect("state serializes");
        let _ = config::write_private(&self.state_path, &text);
    }

    pub fn forget_state(profile_name: &str) {
        let _ = fs::remove_file(config::state_dir().join(format!("{profile_name}.json")));
    }

    fn expired(&self) -> Fail {
        if self.from_env {
            Fail::Auth(format!(
                "Session expired: the token in {REFRESH_TOKEN_ENV} is no longer valid. Run `dedaub-monitoring token` \
                 where you are logged in and set {REFRESH_TOKEN_ENV} to the new value."
            ))
        } else {
            Fail::Auth("Session expired. Run: dedaub-monitoring login".to_string())
        }
    }

    fn bearer(&mut self) -> Res<String> {
        if let Some(token) = self.fresh_token() {
            return Ok(token);
        }
        // One refresh at a time per machine: commands that start together wait here and then
        // find the token the first one fetched.
        let lock = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state_path.with_extension("lock"))
            .ok()
            .filter(|file| file.lock().is_ok());
        let cached = self.read_state();
        if cached.access_token.is_some() && cached.access_expires_at > now() + EXPIRY_MARGIN {
            self.state = cached;
        } else {
            let tokens = auth::refresh(&self.http, &self.profile)?.ok_or_else(|| self.expired())?;
            self.state.access_token = Some(tokens.access_token);
            self.state.access_expires_at = now() + tokens.expires_in;
            if let Some(rotated) = tokens.refresh_token.filter(|t| Some(t) != self.profile.refresh_token.as_ref()) {
                self.profile.refresh_token = Some(rotated);
                if !self.from_env {
                    self.store_refresh_token();
                }
            }
            self.write_state();
        }
        drop(lock);
        Ok(self.state.access_token.clone().expect("access token"))
    }

    fn fresh_token(&self) -> Option<String> {
        self.state.access_token.clone().filter(|_| self.state.access_expires_at > now() + EXPIRY_MARGIN)
    }

    /// Keep the rotated token, or the login would end at the first token's expiry. The file is
    /// read again first so that a login to another profile made meanwhile is kept.
    fn store_refresh_token(&self) {
        let Ok(Some(mut config)) = Config::load() else { return };
        let Some(stored) = config.profiles.get_mut(&self.profile_name) else { return };
        stored.refresh_token = self.profile.refresh_token.clone();
        if let Err(e) = config.save() {
            // Silent when the file is read-only by design (a sandbox): the stored token stays valid.
            let denied = fs::metadata(config::config_path()).map(|m| m.permissions().readonly()).unwrap_or(false);
            if !denied && !e.to_string().contains("ermission denied") {
                eprintln!("warning: could not save the renewed login token ({e}); the login expires at its original time");
            }
        }
    }

    pub fn call(&mut self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, timeout: Duration) -> Res<Reply> {
        if self.interrupted.load(Ordering::SeqCst) {
            return Err(Fail::Interrupted);
        }
        self.call_through(method, path, query, body, timeout)
    }

    fn call_through(
        &mut self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Res<Reply> {
        let attempts = if method == Method::GET { GET_ATTEMPTS } else { 1 };
        let mut reauthenticated = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let token = self.bearer()?;
            let url = format!("{}{}", self.profile.base_url, path);
            let mut request = self.http.request(method.clone(), &url).bearer_auth(token).timeout(timeout);
            if !query.is_empty() {
                request = request.query(query);
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let last = attempt >= attempts;
            match request.send() {
                Err(e) if last || !(e.is_timeout() || e.is_connect()) => {
                    return Err(Fail::Other(format!("request to {path} failed: {e}")));
                }
                Err(_) => {}
                Ok(response) => {
                    let status = response.status().as_u16();
                    if status == 401 && !reauthenticated {
                        // The cached access token was revoked or belongs to an older login.
                        reauthenticated = true;
                        attempt -= 1;
                        self.state.access_token = None;
                        self.write_state();
                        continue;
                    }
                    if last || !RETRY_STATUSES.contains(&status) {
                        let text = response.text().map_err(|e| Fail::Other(format!("reading {path}: {e}")))?;
                        return Ok(Reply { status, text });
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
        }
    }

    fn ok(&self, reply: Reply, path: &str) -> Res<Value> {
        match reply.status {
            200..=299 => {
                if reply.text.trim().is_empty() {
                    Ok(Value::Null)
                } else {
                    serde_json::from_str(&reply.text).map_err(|e| Fail::Other(format!("{path} did not answer with JSON: {e}")))
                }
            }
            401 => Err(self.expired()),
            403 => Err(Fail::Other(format!("permission denied on {path}"))),
            404 => Err(Fail::Query(format!("not found: {}", detail(&reply.text)))),
            status if status >= 500 => Err(Fail::Other(format!("the server failed (HTTP {status} on {path}): {}", detail(&reply.text)))),
            _ => Err(Fail::Query(detail(&reply.text))),
        }
    }

    pub fn get(&mut self, path: &str, query: &[(&str, String)]) -> Res<Value> {
        let reply = self.call(Method::GET, path, query, None, Duration::from_secs(60))?;
        self.ok(reply, path)
    }

    pub fn get_slow(&mut self, path: &str, query: &[(&str, String)], seconds: u64) -> Res<Value> {
        let reply = self.call(Method::GET, path, query, None, Duration::from_secs(seconds))?;
        self.ok(reply, path)
    }

    pub fn post(&mut self, path: &str, query: &[(&str, String)], body: &Value) -> Res<Value> {
        let reply = self.call(Method::POST, path, query, Some(body), Duration::from_secs(60))?;
        self.ok(reply, path)
    }

    // --- account ---

    pub fn me(&mut self) -> Res<Value> {
        let me = self.get("/api/auth/me", &[])?;
        self.state.entity_id = me["entity_id"].as_i64();
        self.state.username = me["username"].as_str().map(str::to_string);
        self.write_state();
        Ok(me)
    }

    pub fn entity_id(&mut self) -> Res<i64> {
        if let Some(id) = self.state.entity_id {
            return Ok(id);
        }
        self.me()?;
        self.state.entity_id.ok_or_else(|| Fail::Other("the account has no entity id".to_string()))
    }

    pub fn username(&self) -> Option<&str> {
        self.state.username.as_deref()
    }

    // --- the scratch query ---

    pub fn scratch_query_id(&mut self) -> Res<i64> {
        if let Some(id) = std::env::var(QUERY_ID_ENV).ok().and_then(|v| v.trim().parse().ok()) {
            return Ok(id);
        }
        if let Some(id) = self.state.scratch_query_id {
            return Ok(id);
        }
        let id = self.find_or_create_scratch()?;
        self.state.scratch_query_id = Some(id);
        self.write_state();
        Ok(id)
    }

    /// The cached id no longer resolves (the query was deleted in the web app).
    pub fn forget_scratch(&mut self) {
        self.state.scratch_query_id = None;
        self.write_state();
    }

    fn find_or_create_scratch(&mut self) -> Res<i64> {
        let entity = self.entity_id()?;
        let by_entity = [("entity_id", entity.to_string())];
        let folder_of = |folders: &Value| {
            folders.as_array().into_iter().flatten().find(|f| f["path"] == SCRATCH_FOLDER).and_then(|f| f["folder_id"].as_i64())
        };
        let folders = self.get(FOLDER_PATH, &by_entity)?;
        let folder = match folder_of(&folders) {
            Some(id) => id,
            None => {
                let created = self.post(FOLDER_PATH, &by_entity, &json!(SCRATCH_FOLDER))?;
                match folder_of(&created) {
                    Some(id) => id,
                    None => folder_of(&self.get(FOLDER_PATH, &by_entity)?)
                        .ok_or_else(|| Fail::Other(format!("could not create the folder {SCRATCH_FOLDER}")))?,
                }
            }
        };
        // Ask for the folder's queries only: the unfiltered listing stops at 500.
        let in_folder = [("entity_id", entity.to_string()), ("folder_id", folder.to_string())];
        let queries = self.get("/api/profile/tsql/query", &in_folder)?;
        let existing = queries
            .as_array()
            .into_iter()
            .flatten()
            .find(|q| q["folder_id"].as_i64() == Some(folder) && q["query_name"] == SCRATCH_NAME)
            .and_then(|q| q["query_id"].as_i64());
        if let Some(id) = existing {
            return Ok(id);
        }
        let body = json!({
            "folder_id": folder,
            "query_name": SCRATCH_NAME,
            "query_text": "",
            "alert_template": "",
            "visibility": "PRIVATE",
            "is_unlisted": false,
            "is_template": false,
        });
        self.post("/api/profile/tsql/query", &by_entity, &body)?
            .as_i64()
            .ok_or_else(|| Fail::Other("the server did not return the new query's id".to_string()))
    }

    // --- SQL ---

    fn sql_body(&mut self, sql: &str, network: &str) -> Res<Value> {
        Ok(json!({
            "query": sql,
            "query_id": self.scratch_query_id()?,
            "query_entity_id": self.entity_id()?,
            "network": network,
        }))
    }

    fn windowed_body(&mut self, sql: &str, network: &str, window: Option<&Window>) -> Res<Value> {
        let mut body = self.sql_body(sql, network)?;
        if let Some(window) = window {
            body["default_duration"] = json!(window.last);
            body["default_start_time"] = json!(window.until);
        }
        Ok(body)
    }

    /// Run one of the compile-only endpoints. `preview` and `explain` take the window that
    /// macros without a `duration` fall back to; `metadata` has none.
    pub fn compile(&mut self, kind: &str, sql: &str, network: &str, window: Option<&Window>) -> Res<Value> {
        let path = format!("/api/tsql/query/{kind}");
        let body = self.windowed_body(sql, network, window)?;
        match self.post(&path, &[], &body) {
            Err(Fail::Query(message)) if message.contains("Folder not found") => {
                self.forget_scratch();
                let body = self.windowed_body(sql, network, window)?;
                self.post(&path, &[], &body)
            }
            other => other,
        }
    }

    /// Start a query. `Ok(None)` means the account's concurrent-query limit is reached.
    pub fn execute_async(&mut self, sql: &str, network: &str, window: &Window, limit: u32, offset: u32) -> Res<Option<String>> {
        let mut body = self.windowed_body(sql, network, Some(window))?;
        body["limit"] = json!(limit);
        body["offset"] = json!(offset);
        let path = "/api/tsql/query/execute_async";
        let reply = self.call(Method::POST, path, &[], Some(&body), Duration::from_secs(60))?;
        if reply.status == 429 {
            return Ok(None);
        }
        let task = self.ok(reply, path)?;
        task.as_str().map(|t| Some(t.to_string())).ok_or_else(|| Fail::Other("the server returned no task id".to_string()))
    }

    pub fn task(&mut self, task: &str) -> Res<Value> {
        self.get(&format!("/api/tsql/query/execute_async/{task}"), &[])
    }

    /// Revoke a running query, also after an interrupt. Best effort: the caller is already
    /// failing for another reason.
    pub fn cancel(&mut self, task: &str) {
        let path = format!("/api/tsql/query/execute_async/{task}");
        let _ = self.call_through(Method::DELETE, &path, &[], None, Duration::from_secs(20));
    }

    pub fn active_tasks(&mut self) -> Res<Vec<String>> {
        let tasks = self.get("/api/tsql/query/execute_async", &[])?;
        Ok(tasks.as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)).collect())
    }
}

/// The time window of macros that name no `duration` of their own.
pub struct Window {
    pub last: String,
    pub until: Option<String>,
}

/// The server's error text without its blank lines and position boilerplate.
pub fn detail(text: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let detail = parsed.as_ref().map(|v| &v["detail"]);
    let message = match detail {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let field = item["loc"].as_array().and_then(|loc| loc.last()).and_then(Value::as_str).unwrap_or("");
                format!("{field}: {}", item["msg"].as_str().unwrap_or("invalid"))
            })
            .collect::<Vec<_>>()
            .join("; "),
        _ => text.to_string(),
    };
    clean(&message)
}

pub fn clean(message: &str) -> String {
    message
        .lines()
        .map(|line| if line.trim() == "^" { line.trim_end() } else { line.trim() })
        .filter(|line| !line.is_empty() && !line.starts_with("at <top level query>"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_the_server_error() {
        let raw = r#"{"detail":"\n\n            at <top level query>, line 1\n\n            function ts(integer) does not exist\nLINE 1: SELECT ts(26391604) AS t\n               ^\nHINT:  No function matches.\n"}"#;
        assert_eq!(
            detail(raw),
            "function ts(integer) does not exist\nLINE 1: SELECT ts(26391604) AS t\n               ^\nHINT:  No function matches."
        );
        let validation = r#"{"detail":[{"type":"less_than_equal","loc":["body","limit"],"msg":"Input should be less than or equal to 500","input":5000}]}"#;
        assert_eq!(detail(validation), "limit: Input should be less than or equal to 500");
        assert_eq!(detail("plain"), "plain");
    }
}
