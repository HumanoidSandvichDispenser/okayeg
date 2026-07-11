//! `.eg/config.toml`: hand-maintained, per-repo configuration.
//!
//! The config is private state under `.eg/`, never synced or exported. One
//! table so far:
//!
//! ```toml
//! [authz]
//! command = ["/usr/local/bin/my-authz", "project-42"]
//! ```
//!
//! `authz.command` names a program that decides each incoming connection in
//! place of the trust file. It runs with the given argv, receives the peer's id
//! newline-terminated on stdin, and answers `pull` and/or `push` on stdout (see
//! [`CommandAuthorizer`](okayeg_net::CommandAuthorizer)). When the table is
//! absent, `.eg/trust` decides as before.
//!
//! ```toml
//! [identity]
//! name = "alice"
//! email = "alice@example.com"
//! ```
//!
//! `identity` is the name and email announced to peers, like `git config
//! user.name`. Self-asserted; it grants nothing. A key that is absent falls
//! back to the same key in git config; a key set to `""` stays blank instead.

use std::io;
use std::path::Path;

use crate::workspace::Workspace;

const CONFIG_PATH: &str = ".eg/config.toml";

/// The parsed config. A missing file parses as all-defaults.
#[derive(Default, Debug, PartialEq)]
pub struct Config {
    /// The `[authz] command` argv, program first. `None` means the trust file
    /// gates connections.
    pub authz_command: Option<Vec<String>>,

    /// The `[identity] name` announced to peers. `None` when absent (git
    /// config decides); `Some("")` keeps it blank.
    pub name: Option<String>,

    /// The `[identity] email` announced to peers, with the same fallback rule
    /// as `name`.
    pub email: Option<String>,
}

impl Config {
    /// Load `.eg/config.toml`, or defaults if it does not exist.
    ///
    /// A file that exists but does not parse, or whose `authz.command` has the
    /// wrong shape, is an error, never a fallback to defaults.
    pub fn load(ws: &dyn Workspace) -> io::Result<Self> {
        let text = match ws.read_file(Path::new(CONFIG_PATH)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|e| bad(format!("not utf-8: {e}")))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        Self::parse(&text)
    }

    fn parse(text: &str) -> io::Result<Self> {
        let value: toml::Table = text.parse().map_err(|e| bad(format!("{e}")))?;
        let authz_command = match value.get("authz").and_then(|a| a.get("command")) {
            None => None,
            Some(v) => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| bad("authz.command must be an array of strings"))?;
                let cmd = arr
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| bad("authz.command must be an array of strings"))?;
                if cmd.is_empty() {
                    return Err(bad("authz.command must name a program"));
                }
                Some(cmd)
            }
        };

        let identity_field = |key: &str| match value.get("identity").and_then(|t| t.get(key)) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .map(|s| Some(s.to_owned()))
                .ok_or_else(|| bad(format!("identity.{key} must be a string"))),
        };
        let name = identity_field("name")?;
        let email = identity_field("email")?;

        Ok(Self { authz_command, name, email })
    }
}

fn bad(msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!(".eg/config.toml: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_unrelated_tables_mean_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::parse("[other]\nx = 1\n").unwrap(), Config::default());
    }

    #[test]
    fn authz_command_parses_as_argv() {
        let config = Config::parse("[authz]\ncommand = [\"/bin/authz\", \"project-42\"]\n").unwrap();
        assert_eq!(
            config.authz_command,
            Some(vec!["/bin/authz".to_owned(), "project-42".to_owned()])
        );
    }

    #[test]
    fn malformed_config_is_an_error_not_a_fallback() {
        assert!(Config::parse("[authz\n").is_err()); // toml syntax
        assert!(Config::parse("[authz]\ncommand = \"authz\"\n").is_err()); // not an array
        assert!(Config::parse("[authz]\ncommand = [1]\n").is_err()); // not strings
        assert!(Config::parse("[authz]\ncommand = []\n").is_err()); // no program
        assert!(Config::parse("[identity]\nname = 1\n").is_err()); // not a string
    }

    #[test]
    fn identity_distinguishes_absent_from_blank() {
        let config = Config::parse("[identity]\nname = \"alice\"\nemail = \"\"\n").unwrap();
        assert_eq!(config.name.as_deref(), Some("alice"));
        assert_eq!(config.email.as_deref(), Some(""));

        let config = Config::parse("").unwrap();
        assert_eq!(config.name, None);
        assert_eq!(config.email, None);
    }
}
