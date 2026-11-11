//! The login file, shared with the earlier Python CLI, and this tool's own state directory.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Fail, Res};

pub const DEFAULT_PROFILE: &str = "prod";
pub const DEFAULT_BASE_URL: &str = "https://api.dedaub.com";
pub const DEFAULT_OIDC_HOST: &str = "https://auth.dedaub.com";
pub const DEFAULT_CLIENT_ID: &str = "watchdog-client";
pub const DEFAULT_REALM: &str = "dedaub";

/// A refresh token here replaces the stored one, so a job can run with no login file.
pub const REFRESH_TOKEN_ENV: &str = "DEDAUB_MONITORING_REFRESH_TOKEN";
pub const STATE_DIR_ENV: &str = "DEDAUB_MONITORING_STATE_DIR";

fn default_realm() -> String {
    DEFAULT_REALM.to_string()
}

/// One login. The fields and their order are the Python CLI's, which rejects any other key.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Profile {
    pub base_url: String,
    pub oidc_host: String,
    pub client_id: String,
    #[serde(default = "default_realm")]
    pub realm: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

impl Profile {
    pub fn production() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            oidc_host: DEFAULT_OIDC_HOST.to_string(),
            client_id: DEFAULT_CLIENT_ID.to_string(),
            realm: DEFAULT_REALM.to_string(),
            refresh_token: None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Config {
    pub default: String,
    pub profiles: BTreeMap<String, Profile>,
}

pub fn home() -> PathBuf {
    std::env::home_dir().expect("no home directory")
}

pub fn config_path() -> PathBuf {
    home().join(".config").join("dedaub").join("monitoring.json")
}

/// Access-token cache, scratch-query ids and the query-slot lock files.
pub fn state_dir() -> PathBuf {
    match std::env::var_os(STATE_DIR_ENV) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".cache").join("dedaub-monitoring"),
    }
}

impl Config {
    /// `None` when there is no login file.
    pub fn load() -> Res<Option<Self>> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Res<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Fail::Other(format!("cannot read {}: {e}", path.display()))),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Fail::Auth(format!("the login file {} is corrupt ({e}). Run: dedaub-monitoring login", path.display())))
    }

    pub fn save(&self) -> Res<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Res<()> {
        let dir = path.parent().expect("login file has a directory");
        fs::create_dir_all(dir).map_err(|e| Fail::Other(format!("cannot create {}: {e}", dir.display())))?;
        restrict(dir, 0o700);
        let text = serde_json::to_string_pretty(self).expect("config serializes");
        write_private(path, text.as_bytes()).map_err(|e| Fail::Other(format!("cannot write {}: {e}", path.display())))
    }
}

/// Write a file that only its owner can read, replacing the old one in one step.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn restrict(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_the_python_format() {
        let dir = std::env::temp_dir().join(format!("dm-config-{}", std::process::id()));
        let path = dir.join("monitoring.json");
        let python = r#"{
  "default": "prod",
  "profiles": {
    "prod": {
      "base_url": "https://api.dedaub.com",
      "oidc_host": "https://auth.dedaub.com",
      "client_id": "watchdog-client",
      "realm": "dedaub",
      "refresh_token": "abc"
    }
  }
}"#;
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, python).unwrap();
        let config = Config::load_from(&path).unwrap().unwrap();
        assert_eq!(config.default, "prod");
        assert_eq!(config.profiles["prod"].refresh_token.as_deref(), Some("abc"));
        config.save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), python);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_is_no_login() {
        assert_eq!(Config::load_from(Path::new("/nonexistent/dm/monitoring.json")).unwrap(), None);
    }
}
