//! `eg`, the okayeg command line.
//!
//! It parses arguments, resolves which repo a command acts on, and dispatches
//! to the worker modules: [`bridge`] turns a directory into a doc and back,
//! [`net`] syncs over iroh, and [`watch`] tracks live edits.

mod bridge;
mod config;
mod filetree;
mod ignore;
mod net;
mod trust;
mod watch;
mod workspace;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use okayeg::Perms;

/// Where a repo keeps its private state, hidden under the served directory:
/// the node key now, the doc snapshot and trust set later. Never imported as
/// doc content.
const EG_DIR: &str = ".eg";

/// The okayeg command line.
#[derive(clap::Parser)]
#[command(
    name = "eg",
    version,
    about = "Sync a directory of text files as an okayeg doc."
)]
struct Cli {
    /// Act as if run from <dir>, used by serve/pull/join/id/trust
    #[arg(short = 'C', value_name = "dir", global = true)]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Take a directory into a doc snapshot.
    Snapshot { dir: PathBuf, out: PathBuf },
    /// Write a doc snapshot back to a directory.
    Restore { input: PathBuf, dir: PathBuf },
    /// Keep a doc snapshot in sync with a directory.
    Watch { dir: PathBuf, out: PathBuf },
    /// Serve this repo over iroh, live, for peers to sync.
    Serve,
    /// Pull current state from a peer, then exit.
    Pull {
        peer: String,
        /// Seconds to wait for the dial before giving up as unreachable.
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
    /// Clone if empty, then sync live with a peer.
    Join { peer: String },
    #[command(flatten)]
    Shared(SharedCmd),
}

/// Subcommands available both from the shell and inside the `eg serve` repl.
/// One clap definition, so both surfaces share names, args, and help text; the
/// repl parses each input line with the same derive (see `net::spawn_repl`).
#[derive(clap::Subcommand)]
enum SharedCmd {
    /// Print this repo's endpoint id.
    Id,
    /// Show this repo's id, doc contents, and trust set.
    Status,
    /// Grant a peer access (default both).
    Trust {
        peer: String,
        /// Any of `pull` / `push`; empty grants both.
        access: Vec<Access>,
    },
    /// Lists the files in the specified directory in the doc.
    Ls {
        /// The path to list, relative to the doc root.
        #[arg(default_value = "")]
        path: String,
    },
    /// Print the contents of one or more files in the doc.
    Cat {
        /// The paths to print, relative to the doc root.
        paths: Vec<String>,
    },
}

impl SharedCmd {
    /// Run against the repo at `dir`, already resolved.
    fn run(self, dir: &Path) -> std::io::Result<()> {
        match self {
            SharedCmd::Id => net::id(dir),
            SharedCmd::Status => net::status(dir),
            SharedCmd::Trust { peer, access } => net::trust(dir, &peer, perms_from(&access)),
            SharedCmd::Ls { path } => filetree::ls_stdio(dir, &path),
            SharedCmd::Cat { paths } => filetree::cat_stdio(dir, &paths),
        }
    }
}

/// A capability that can be granted to a peer with `eg trust`.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Access {
    Pull,
    Push,
}

/// Turn the requested access list into a [`Perms`]; empty means full access.
fn perms_from(access: &[Access]) -> Perms {
    if access.is_empty() {
        return Perms::all();
    }
    Perms {
        pull: access.contains(&Access::Pull),
        push: access.contains(&Access::Push),
    }
}

fn main() -> ExitCode {
    let cli = <Cli as clap::Parser>::parse();
    let dir = cli.dir.as_deref();
    run(match cli.cmd {
        Cmd::Snapshot { dir, out } => bridge::snapshot(&dir, &out),
        Cmd::Restore { input, dir } => bridge::restore(&input, &dir),
        Cmd::Watch { dir, out } => watch::watch(&dir, &out),
        Cmd::Serve => with_repo(dir, net::serve),
        Cmd::Pull { peer, timeout } => with_fresh(dir, |d| net::pull(d, &peer, timeout)),
        Cmd::Join { peer } => with_fresh(dir, |d| net::join(d, &peer)),
        Cmd::Shared(cmd) => with_repo(dir, |d| cmd.run(d)),
    })
}

/// Resolve the enclosing repo, then run `f` against it.
///
/// Starts at `cdir` (or the cwd) and walks up to the nearest `.eg/`, so these
/// commands work from anywhere inside a repo. With none up the tree, the start
/// directory itself is used (and `.eg/` created there on first use).
fn with_repo<F>(cdir: Option<&Path>, f: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let start = abs(cdir)?;
    let dir = enclosing_repo(&start).unwrap_or(start);
    f(&dir)
}

/// Resolve a directory to clone into, then run `f` against it.
///
/// Unlike [`with_repo`] this does not retarget to a parent: cloning into a spot
/// nested inside an existing repo is refused, so a stray `join` from within a
/// repo cannot graft one tree onto another. Acting on the repo root itself is
/// allowed, which is how a `join` resumes a live session.
fn with_fresh<F>(cdir: Option<&Path>, f: F) -> std::io::Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    let start = abs(cdir)?;
    if let Some(root) = enclosing_repo(&start) {
        if root != start {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "inside the repo at {}; clone into a fresh directory",
                    root.display()
                ),
            ));
        }
    }
    f(&start)
}

/// Absolutize `cdir` (or the cwd), canonicalizing when the path already exists.
fn abs(cdir: Option<&Path>) -> std::io::Result<PathBuf> {
    let start = match cdir {
        Some(dir) if dir.is_absolute() => dir.to_path_buf(),
        Some(dir) => std::env::current_dir()?.join(dir),
        None => std::env::current_dir()?,
    };
    Ok(start.canonicalize().unwrap_or(start))
}

/// The nearest directory at or above `dir` that holds `.eg/`, if any.
fn enclosing_repo(dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(dir);
    while let Some(c) = cur {
        if c.join(EG_DIR).is_dir() {
            return Some(c.to_path_buf());
        }
        cur = c.parent();
    }
    None
}

fn run(result: std::io::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // An empty message means the command already reported the details
            // itself and only the exit code is left to deliver, like cat
            // failing after printing one line per unreadable path.
            let msg = err.to_string();
            if !msg.is_empty() {
                eprintln!("eg: {msg}");
            }
            ExitCode::FAILURE
        }
    }
}

fn to_io<E: std::fmt::Display>(err: E) -> std::io::Error {
    std::io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }

    #[test]
    fn perms_from_access_list() {
        assert_eq!(perms_from(&[]), Perms::all());
        assert_eq!(
            perms_from(&[Access::Pull]),
            Perms {
                pull: true,
                push: false
            }
        );
        assert_eq!(
            perms_from(&[Access::Push]),
            Perms {
                pull: false,
                push: true
            }
        );
        assert_eq!(
            perms_from(&[Access::Push, Access::Pull, Access::Pull]),
            Perms {
                pull: true,
                push: true
            }
        );
    }
}
