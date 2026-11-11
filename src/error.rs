//! One failure type; each kind has its own exit code so a caller can branch on it.

use std::fmt;

pub type Res<T> = Result<T, Fail>;

#[derive(Debug, Clone, PartialEq)]
pub enum Fail {
    /// Bad arguments or SQL that this tool refuses before it reaches the server.
    Usage(String),
    /// No login, or the login has expired.
    Auth(String),
    /// Every query slot stayed taken for the whole `--wait`.
    Busy(String),
    /// The query ran past `--timeout` and was revoked on the server.
    Timeout(String),
    /// Ctrl-C or SIGTERM; a running query was revoked.
    Interrupted,
    /// The server rejected the SQL or failed while running it.
    Query(String),
    Other(String),
}

impl Fail {
    pub fn exit_code(&self) -> i32 {
        match self {
            Fail::Query(_) | Fail::Other(_) => 1,
            Fail::Usage(_) => 2,
            Fail::Auth(_) => 3,
            Fail::Busy(_) => 4,
            Fail::Timeout(_) => 5,
            Fail::Interrupted => 130,
        }
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fail::Usage(m) | Fail::Auth(m) | Fail::Busy(m) | Fail::Timeout(m) | Fail::Query(m) | Fail::Other(m) => f.write_str(m),
            Fail::Interrupted => f.write_str("interrupted"),
        }
    }
}

pub const NOT_LOGGED_IN: &str = "Not logged in. Run: dedaub-monitoring login";
