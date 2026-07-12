//! The machine-level config on disk: the global `config.toml` and the keyring,
//! both under the eg config directory.

use std::io;
use std::path::{Path, PathBuf};

use crate::config::{self, Config, DEVICE_KEY, Effective, GlobalConfig, KeySelector, KeysBackend};
use crate::workspace::Workspace;

use crate::EG_DIR;

/// The keyring directory, under the config dir.
const KEYS_DIR: &str = "keys";

/// The global config file, under the config dir.
const CONFIG_FILE: &str = "config.toml";

/// The eg config directory: `$XDG_CONFIG_HOME/eg`, or `~/.config/eg`.
pub fn config_dir() -> io::Result<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", &[".config"])
}

/// The eg subdirectory of the base dir `var` names, or of `fallback` under
/// `$HOME`. A blank variable counts as unset.
pub fn xdg_dir(var: &str, fallback: &[&str]) -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(var).filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("eg"));
    }

    match std::env::var_os("HOME").filter(|d| !d.is_empty()) {
        Some(home) => {
            let mut dir = PathBuf::from(home);
            dir.extend(fallback);
            Ok(dir.join("eg"))
        }
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("neither {var} nor HOME is set"),
        )),
    }
}

/// The `EG_KEY` environment variable. A blank value counts as unset.
pub fn env_key() -> Option<String> {
    std::env::var("EG_KEY").ok().filter(|s| !s.is_empty())
}

/// Resolve every config scope and read the selected key's secret.
pub fn effective(
    global: &GlobalConfig,
    cli_key: Option<&str>,
    repo: &Config,
    repo_key_file: bool,
    ws: &dyn Workspace,
) -> io::Result<(Effective, [u8; 32])> {
    let eff = config::resolve(cli_key, env_key().as_deref(), repo, global, repo_key_file)?;
    let secret = secret(global, &eff.key, ws)?;
    Ok((eff, secret))
}

/// Load the global config, or defaults if it does not exist.
pub fn load_global() -> io::Result<GlobalConfig> {
    load_global_at(&config_dir()?)
}

fn load_global_at(dir: &Path) -> io::Result<GlobalConfig> {
    match std::fs::read_to_string(dir.join(CONFIG_FILE)) {
        Ok(text) => GlobalConfig::parse(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(GlobalConfig::default()),
        Err(e) => Err(e),
    }
}

/// Read the 32-byte secret `selector` names, minting [`DEVICE_KEY`] on first
/// use.
pub fn secret(
    global: &GlobalConfig,
    selector: &KeySelector,
    ws: &dyn Workspace,
) -> io::Result<[u8; 32]> {
    match selector {
        KeySelector::Path(path) => parse_secret(&std::fs::read(path)?, &path.display()),

        KeySelector::RepoFile => {
            let path = Path::new(EG_DIR).join("key");
            parse_secret(&ws.read_file(&path)?, &path.display())
        }

        KeySelector::Named(name) => {
            if global.keys_backend == KeysBackend::SecretService {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the secret-service keys backend is not implemented yet",
                ));
            }
            named_secret(&config_dir()?, name)
        }
    }
}

fn named_secret(dir: &Path, name: &str) -> io::Result<[u8; 32]> {
    let path = dir.join(KEYS_DIR).join(name);
    match std::fs::read(&path) {
        Ok(bytes) => parse_secret(&bytes, &path.display()),

        Err(e) if e.kind() == io::ErrorKind::NotFound && name == DEVICE_KEY => {
            let secret = okayeg_net::generate_secret();
            write_new_secret(&path, &secret)?;
            Ok(secret)
        }

        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no key named \"{name}\" in {}",
                dir.join(KEYS_DIR).display()
            ),
        )),

        Err(e) => Err(e),
    }
}

fn write_new_secret(path: &Path, secret: &[u8; 32]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    if let Some(parent) = path.parent() {
        let mut dirs = std::fs::DirBuilder::new();
        dirs.recursive(true).mode(0o700);
        dirs.create(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;

    file.write_all(secret)
}

fn parse_secret(bytes: &[u8], path: &impl std::fmt::Display) -> io::Result<[u8; 32]> {
    bytes.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{path} is not 32 bytes; remove it to regenerate"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_key_mints_on_first_use_and_stays_stable() {
        let dir = tempfile::tempdir().unwrap();
        let first = named_secret(dir.path(), DEVICE_KEY).unwrap();
        let second = named_secret(dir.path(), DEVICE_KEY).unwrap();
        assert_eq!(first, second);

        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join(KEYS_DIR).join(DEVICE_KEY);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn missing_named_key_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = named_secret(dir.path(), "work").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("work"));
    }

    #[test]
    fn named_key_reads_existing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let secret = [7u8; 32];
        write_new_secret(&dir.path().join(KEYS_DIR).join("work"), &secret).unwrap();
        assert_eq!(named_secret(dir.path(), "work").unwrap(), secret);
    }

    #[test]
    fn malformed_key_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(KEYS_DIR)).unwrap();
        std::fs::write(dir.path().join(KEYS_DIR).join("bad"), b"short").unwrap();
        let err = named_secret(dir.path(), "bad").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn missing_global_config_is_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_global_at(dir.path()).unwrap(), GlobalConfig::default());

        std::fs::write(dir.path().join(CONFIG_FILE), "key = \"work\"\n").unwrap();
        let global = load_global_at(dir.path()).unwrap();
        assert_eq!(global.key.as_deref(), Some("work"));
    }
}
