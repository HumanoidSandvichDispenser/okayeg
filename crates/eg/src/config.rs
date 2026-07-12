//! `.eg/config.toml`: per-repo configuration.
//!
//! `authz.command` names a program that decides each incoming connection in
//! place of the trust file. It runs with the given argv, receives the peer's id
//! newline-terminated on stdin, and answers `pull` and/or `push` on stdout (see
//! [`CommandAuthorizer`](okayeg_net::CommandAuthorizer)). When the table is
//! absent, it instead tries to read from `.eg/trust` if it exists.
//!
//! ```toml
//! [identity]
//! name = "alice"
//! email = "alice@example.com"
//! ```
//!
//! `identity` is the name and email shared with peers in this repo's presence
//! entry, like `git config user.name`. Self-asserted; it grants nothing. A key
//! that is absent falls back to the same key in git config; a key set to `""`
//! stays blank instead.
//!
//! ```toml
//! [session]
//! command = ["/usr/local/bin/notify-host", "project-42"]
//! ```
//!
//! `session.command` names a program run once per session event on a serving
//! repo. Its stdin is two newline-terminated lines: the event (`hello` at
//! join, `bye` at leave), then the peer's endpoint id. It decides nothing:
//! its exit status is ignored and a failure only logs.
//!
//! ```toml
//! remote = "thesis"
//! key = "work"
//! ```
//!
//! `remote` binds the repo to a `[remote.<name>]` block in the global config;
//! `peer` instead binds it to an endpoint id directly, keeping the repo
//! self-contained. Setting both is an error. `key` selects the key the repo
//! runs with: a keyring name, or a path to a raw secret file.
//!
//! The global config, `$XDG_CONFIG_HOME/eg/config.toml`, holds machine-level
//! settings: default `[identity]` fields and `key`, the keyring backend in
//! `[keys]`, and the named `[remote.<name>]` blocks. [`resolve`] merges the
//! scopes field by field, repo over remote over global.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::workspace::Workspace;

const CONFIG_PATH: &str = ".eg/config.toml";

/// The keyring name of the default key.
pub const DEVICE_KEY: &str = "device";

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

    /// The `[session] command` argv, program first. `None` means session
    /// events are not reported.
    pub session_command: Option<Vec<String>>,

    /// The `[remote.<name>]` block in the global config this repo is bound to.
    pub remote: Option<String>,

    /// The endpoint id this repo dials, binding it by address instead of
    /// through a `remote` reference.
    pub peer: Option<String>,

    /// The key this repo runs with: a keyring name, or a path to a raw secret
    /// file.
    pub key: Option<String>,
}

/// The parsed global config. A missing file parses as all-defaults.
#[derive(Default, Debug, PartialEq)]
pub struct GlobalConfig {
    /// The default `[identity] name`.
    pub name: Option<String>,

    /// The default `[identity] email`.
    pub email: Option<String>,

    /// The default key name.
    pub key: Option<String>,

    /// The `[keys] backend` storing the keyring's secrets.
    pub keys_backend: KeysBackend,

    /// The `[remote.<name>]` blocks, by name.
    pub remotes: BTreeMap<String, Remote>,
}

/// The storage backing the keyring.
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeysBackend {
    /// Raw secret files in the config dir.
    #[default]
    File,
    /// The freedesktop Secret Service.
    SecretService,
}

/// One `[remote.<name>]` block: how this machine reaches a project.
#[derive(Default, Debug, PartialEq)]
pub struct Remote {
    /// The endpoint id to dial.
    pub peer: Option<String>,

    /// The key used with this remote.
    pub key: Option<String>,

    /// The `name` announced to peers on this remote.
    pub name: Option<String>,

    /// The `email` announced to peers on this remote.
    pub email: Option<String>,
}

/// The key a command runs with.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum KeySelector {
    /// A raw secret file at a path.
    Path(PathBuf),
    /// A named key in the keyring.
    Named(String),
    /// The repo's own `.eg/key`.
    RepoFile,
}

/// The settings a command sees once every scope is merged.
#[derive(Debug, PartialEq)]
pub struct Effective {
    /// The `name` announced to peers.
    pub name: Option<String>,

    /// The `email` announced to peers.
    pub email: Option<String>,

    /// The selected key.
    pub key: KeySelector,

    /// The `[authz] command` argv, program first.
    pub authz_command: Option<Vec<String>>,

    /// The `[session] command` argv, program first.
    pub session_command: Option<Vec<String>>,
}

// a bare name selects from the keyring; anything path-like points at a raw
// secret file
fn selector(s: &str) -> KeySelector {
    if s.contains('/') || s == "." || s == ".." {
        KeySelector::Path(PathBuf::from(s))
    } else {
        KeySelector::Named(s.to_owned())
    }
}

/// Merge the config scopes into the [`Effective`] settings.
///
/// The key follows `cli_key` > `env_key` > the repo config > the repo's own
/// `.eg/key` when `repo_key_file` > the referenced remote > the global
/// config > [`DEVICE_KEY`]. Identity fields layer repo over remote over global.
/// Errors when the repo references a remote the global config does not define.
pub fn resolve(
    cli_key: Option<&str>,
    env_key: Option<&str>,
    repo: &Config,
    global: &GlobalConfig,
    repo_key_file: bool,
) -> io::Result<Effective> {
    let remote = match &repo.remote {
        Some(name) => Some(global.remotes.get(name).ok_or_else(|| {
            bad_global(format!(
                "no [remote.{name}] (referenced by .eg/config.toml)"
            ))
        })?),
        None => None,
    };

    let key = cli_key
        .or(env_key)
        .or(repo.key.as_deref())
        .map(selector)
        .or(repo_key_file.then_some(KeySelector::RepoFile))
        .or_else(|| {
            remote
                .and_then(|r| r.key.as_deref())
                .or(global.key.as_deref())
                .map(selector)
        })
        .unwrap_or(KeySelector::Named(DEVICE_KEY.to_owned()));

    let field = |repo: &Option<String>,
                 remote_field: fn(&Remote) -> &Option<String>,
                 global: &Option<String>| {
        repo.clone()
            .or_else(|| remote.and_then(|r| remote_field(r).clone()))
            .or_else(|| global.clone())
    };

    Ok(Effective {
        name: field(&repo.name, |r| &r.name, &global.name),
        email: field(&repo.email, |r| &r.email, &global.email),
        key,
        authz_command: repo.authz_command.clone(),
        session_command: repo.session_command.clone(),
    })
}

impl Config {
    /// Load `.eg/config.toml`, or defaults if it does not exist.
    ///
    /// Malformed input is an error, never a fallback: keys like the authz hook
    /// must not silently degrade.
    pub fn load(ws: &dyn Workspace) -> io::Result<Self> {
        let text = match ws.read_file(Path::new(CONFIG_PATH)) {
            Ok(bytes) => String::from_utf8(bytes).map_err(|e| bad(format!("not utf-8: {e}")))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        Self::parse(&text)
    }

    fn parse(text: &str) -> io::Result<Self> {
        let value: toml::Table = text.parse().map_err(|e| bad(format!("{e}")))?;

        if value.get("remote").is_some_and(toml::Value::is_table) {
            return Err(bad("remote must be a string, e.g. remote = \"<name>\"\n\
                 hint: [remote.<name>] blocks go in the global config"));
        }

        if value.get("keys").is_some_and(toml::Value::is_table) {
            return Err(bad(
                "unknown block [keys]\nhint: the keyring backend is configured in the global config",
            ));
        }

        let remote = string_in(&value, "remote", "", bad)?;
        let peer = string_in(&value, "peer", "", bad)?;
        let key = string_in(&value, "key", "", bad)?;

        if remote.is_some() && peer.is_some() {
            return Err(bad(
                "remote and peer are both set; a repo binds by one or the other",
            ));
        }

        let command_field = |table: &str| match value.get(table).and_then(|t| t.get("command")) {
            None => Ok(None),
            Some(v) => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| bad(format!("{table}.command must be an array of strings")))?;
                let cmd = arr
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| bad(format!("{table}.command must be an array of strings")))?;
                if cmd.is_empty() {
                    return Err(bad(format!("{table}.command must name a program")));
                }
                Ok(Some(cmd))
            }
        };
        let authz_command = command_field("authz")?;
        let session_command = command_field("session")?;

        let identity_field = |key: &str| match value.get("identity").and_then(|t| t.get(key)) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .map(|s| Some(s.to_owned()))
                .ok_or_else(|| bad(format!("identity.{key} must be a string"))),
        };
        let name = identity_field("name")?;
        let email = identity_field("email")?;

        Ok(Self {
            authz_command,
            name,
            email,
            session_command,
            remote,
            peer,
            key,
        })
    }
}

impl GlobalConfig {
    /// Parse the global config. Malformed input is an error, never a fallback.
    pub fn parse(text: &str) -> io::Result<Self> {
        let value: toml::Table = text.parse().map_err(|e| bad_global(format!("{e}")))?;

        for table in ["authz", "session"] {
            if value.contains_key(table) {
                return Err(bad_global(format!(
                    "unknown block [{table}]\nhint: hooks go in the repo's .eg/config.toml"
                )));
            }
        }

        let string_in =
            |table: &toml::Table, key: &str, at: &str| string_in(table, key, at, bad_global);

        let key = string_in(&value, "key", "")?;

        let (name, email) = match value.get("identity") {
            None => (None, None),
            Some(v) => {
                let table = v
                    .as_table()
                    .ok_or_else(|| bad_global("identity must be a table"))?;
                (
                    string_in(table, "name", "identity.")?,
                    string_in(table, "email", "identity.")?,
                )
            }
        };

        let keys_backend = match value.get("keys").and_then(|t| t.get("backend")) {
            None => KeysBackend::default(),
            Some(v) => match v.as_str() {
                Some("file") => KeysBackend::File,
                Some("secret-service") => KeysBackend::SecretService,
                _ => {
                    return Err(bad_global(
                        "keys.backend must be \"file\" or \"secret-service\"",
                    ));
                }
            },
        };

        let mut remotes = BTreeMap::new();
        if let Some(v) = value.get("remote") {
            let table = v
                .as_table()
                .ok_or_else(|| bad_global("remote must be a table of [remote.<name>] blocks"))?;
            for (name, block) in table {
                let block = block
                    .as_table()
                    .ok_or_else(|| bad_global(format!("remote.{name} must be a table")))?;
                let at = format!("remote.{name}.");
                remotes.insert(
                    name.clone(),
                    Remote {
                        peer: string_in(block, "peer", &at)?,
                        key: string_in(block, "key", &at)?,
                        name: string_in(block, "name", &at)?,
                        email: string_in(block, "email", &at)?,
                    },
                );
            }
        }

        Ok(Self {
            name,
            email,
            key,
            keys_backend,
            remotes,
        })
    }
}

/// The optional string at `table[key]`, with `at` prefixing the key in the
/// error `err` builds.
fn string_in(
    table: &toml::Table,
    key: &str,
    at: &str,
    err: fn(String) -> io::Error,
) -> io::Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| err(format!("{at}{key} must be a string"))),
    }
}

fn bad(msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(".eg/config.toml: {msg}"),
    )
}

fn bad_global(msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("eg config.toml: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_unrelated_tables_mean_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(
            Config::parse("[other]\nx = 1\n").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn authz_command_parses_as_argv() {
        let config =
            Config::parse("[authz]\ncommand = [\"/bin/authz\", \"project-42\"]\n").unwrap();
        assert_eq!(
            config.authz_command,
            Some(vec!["/bin/authz".to_owned(), "project-42".to_owned()])
        );
    }

    #[test]
    fn session_command_parses_like_authz() {
        let config = Config::parse("[session]\ncommand = [\"/bin/notify\"]\n").unwrap();
        assert_eq!(config.session_command, Some(vec!["/bin/notify".to_owned()]));
        assert!(Config::parse("[session]\ncommand = []\n").is_err());
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
    fn repo_config_parses_remote_and_key() {
        let config = Config::parse("remote = \"thesis\"\nkey = \"work\"\n").unwrap();
        assert_eq!(config.remote.as_deref(), Some("thesis"));
        assert_eq!(config.key.as_deref(), Some("work"));

        assert!(Config::parse("remote = 1\n").is_err());
        assert!(Config::parse("key = []\n").is_err());
    }

    #[test]
    fn remote_and_peer_bindings_are_mutually_exclusive() {
        let config = Config::parse("peer = \"abcd\"\n").unwrap();
        assert_eq!(config.peer.as_deref(), Some("abcd"));

        assert!(Config::parse("remote = \"thesis\"\npeer = \"abcd\"\n").is_err());
    }

    #[test]
    fn global_scope_tables_are_rejected_in_repo_config() {
        assert!(Config::parse("[remote.thesis]\npeer = \"x\"\n").is_err());
        assert!(Config::parse("[keys]\nbackend = \"file\"\n").is_err());
    }

    #[test]
    fn global_config_parses_all_fields() {
        let config = GlobalConfig::parse(
            "key = \"work\"\n\
             [identity]\nname = \"alice\"\nemail = \"a@b.c\"\n\
             [keys]\nbackend = \"secret-service\"\n\
             [remote.thesis]\npeer = \"abcd\"\nkey = \"uni\"\nname = \"al\"\n",
        )
        .unwrap();
        assert_eq!(config.key.as_deref(), Some("work"));
        assert_eq!(config.name.as_deref(), Some("alice"));
        assert_eq!(config.email.as_deref(), Some("a@b.c"));
        assert_eq!(config.keys_backend, KeysBackend::SecretService);

        let remote = &config.remotes["thesis"];
        assert_eq!(remote.peer.as_deref(), Some("abcd"));
        assert_eq!(remote.key.as_deref(), Some("uni"));
        assert_eq!(remote.name.as_deref(), Some("al"));
        assert_eq!(remote.email, None);
    }

    #[test]
    fn empty_global_config_is_all_defaults() {
        assert_eq!(GlobalConfig::parse("").unwrap(), GlobalConfig::default());
        assert_eq!(GlobalConfig::default().keys_backend, KeysBackend::File);
    }

    #[test]
    fn repo_scope_tables_are_rejected_in_global_config() {
        assert!(GlobalConfig::parse("[authz]\ncommand = [\"x\"]\n").is_err());
        assert!(GlobalConfig::parse("[session]\ncommand = [\"x\"]\n").is_err());
    }

    #[test]
    fn malformed_global_config_is_an_error_not_a_fallback() {
        assert!(GlobalConfig::parse("[remote\n").is_err()); // toml syntax
        assert!(GlobalConfig::parse("remote = \"x\"\n").is_err()); // not a table
        assert!(GlobalConfig::parse("[remote.a]\npeer = 1\n").is_err()); // not a string
        assert!(GlobalConfig::parse("[keys]\nbackend = \"vault\"\n").is_err()); // unknown backend
        assert!(GlobalConfig::parse("identity = \"alice\"\n").is_err()); // not a table
    }

    #[test]
    fn key_selection_follows_the_precedence_chain() {
        let mut repo = Config::default();
        let mut global = GlobalConfig::default();
        global.remotes.insert(
            "thesis".to_owned(),
            Remote {
                key: Some("uni".to_owned()),
                ..Remote::default()
            },
        );
        global.key = Some("work".to_owned());
        repo.remote = Some("thesis".to_owned());
        repo.key = Some("repo".to_owned());

        fn key(
            cli: Option<&str>,
            env: Option<&str>,
            repo: &Config,
            global: &GlobalConfig,
            file: bool,
        ) -> KeySelector {
            resolve(cli, env, repo, global, file).unwrap().key
        }

        assert_eq!(
            key(Some("cli"), Some("env"), &repo, &global, true),
            KeySelector::Named("cli".to_owned())
        );
        assert_eq!(
            key(None, Some("env"), &repo, &global, true),
            KeySelector::Named("env".to_owned())
        );
        assert_eq!(
            key(None, None, &repo, &global, true),
            KeySelector::Named("repo".to_owned())
        );

        repo.key = None;
        assert_eq!(key(None, None, &repo, &global, true), KeySelector::RepoFile);
        assert_eq!(
            key(None, None, &repo, &global, false),
            KeySelector::Named("uni".to_owned())
        );

        repo.remote = None;
        assert_eq!(
            key(None, None, &repo, &global, false),
            KeySelector::Named("work".to_owned())
        );

        global.key = None;
        assert_eq!(
            key(None, None, &repo, &global, false),
            KeySelector::Named(DEVICE_KEY.to_owned())
        );
    }

    #[test]
    fn path_like_key_values_select_a_secret_file() {
        let repo = Config::default();
        let global = GlobalConfig::default();
        let resolved = resolve(Some("./keys/x"), None, &repo, &global, false).unwrap();
        assert_eq!(resolved.key, KeySelector::Path(PathBuf::from("./keys/x")));

        let resolved = resolve(Some("work"), None, &repo, &global, false).unwrap();
        assert_eq!(resolved.key, KeySelector::Named("work".to_owned()));
    }

    #[test]
    fn unknown_remote_reference_is_an_error() {
        let repo = Config {
            remote: Some("nope".to_owned()),
            ..Config::default()
        };
        assert!(resolve(None, None, &repo, &GlobalConfig::default(), false).is_err());
    }

    #[test]
    fn identity_layers_repo_over_remote_over_global() {
        let mut global = GlobalConfig {
            name: Some("global".to_owned()),
            email: Some("global@x".to_owned()),
            ..GlobalConfig::default()
        };
        global.remotes.insert(
            "r".to_owned(),
            Remote {
                name: Some("remote".to_owned()),
                ..Remote::default()
            },
        );
        let repo = Config {
            remote: Some("r".to_owned()),
            email: Some("".to_owned()),
            ..Config::default()
        };

        let resolved = resolve(None, None, &repo, &global, false).unwrap();
        // name: unset in repo, the remote block wins over global
        assert_eq!(resolved.name.as_deref(), Some("remote"));
        // email: the repo's explicit blank shadows both lower layers
        assert_eq!(resolved.email.as_deref(), Some(""));

        let repo = Config::default();
        let resolved = resolve(None, None, &repo, &global, false).unwrap();
        assert_eq!(resolved.name.as_deref(), Some("global"));
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
