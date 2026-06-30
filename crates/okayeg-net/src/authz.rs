//! The authz seam: deciding what a connecting peer may do.
//!
//! When a peer connects, [`Transport::accept`](crate::Transport::accept) asks an
//! [`Authorizer`] to resolve the peer's id to its [`Perms`], or to refuse it. The
//! trait is the pluggable hook: a closure works for in-process policy, and
//! [`CommandAuthorizer`] shells out to a script so the real decision can live
//! anywhere (a fossenCD api call, a direct db query, an ldap lookup).
//!
//! The id handed to the authorizer is whatever the transport names a peer by. For
//! the iroh transport that is the peer's ed25519 public key, which iroh has
//! already authenticated in its handshake. A future authn seam could verify a
//! master-signed certificate first and hand this trait a longer-lived principal
//! (a pqc master key, say) instead of the raw transport key. Nothing here assumes
//! the id is an ed key, so that promotion would not disturb this code.

use std::future::Future;

use okayeg::Perms;

/// Resolves a connecting peer to the rights it may exercise.
///
/// Implementations decide policy: a closure for something in-process, or
/// [`CommandAuthorizer`] to defer to an external script. Returning `None` refuses
/// the peer outright; the connection is dropped before any sync runs.
#[allow(async_fn_in_trait)]
pub trait Authorizer {
    /// How the transport names the peer being authorized.
    type Id;

    /// Resolve `who` to its [`Perms`], or `None` to refuse the peer.
    async fn authorize(&self, who: Self::Id) -> Option<Perms>;
}

/// Wrap a closure as an [`Authorizer`]. Lets an in-process policy be written
/// inline, without a named type:
///
/// ```ignore
/// node.accept(&from_fn(|who| async move { trusted.get(&who).copied() })).await
/// ```
///
/// A bare closure cannot impl the trait directly (Rust cannot pin down the id and
/// future types from an `Fn` bound), so this wrapper carries them.
pub fn from_fn<Id, F, Fut>(f: F) -> FnAuthorizer<Id, F>
where
    F: Fn(Id) -> Fut,
    Fut: Future<Output = Option<Perms>>,
{
    FnAuthorizer { f, _id: std::marker::PhantomData }
}

/// An [`Authorizer`] backed by a closure. Built by [`from_fn`].
pub struct FnAuthorizer<Id, F> {
    f: F,
    _id: std::marker::PhantomData<fn(Id)>,
}

impl<Id, F, Fut> Authorizer for FnAuthorizer<Id, F>
where
    F: Fn(Id) -> Fut,
    Fut: Future<Output = Option<Perms>>,
{
    type Id = Id;

    async fn authorize(&self, who: Id) -> Option<Perms> {
        (self.f)(who).await
    }
}

/// An [`Authorizer`] that runs an external command to decide each peer.
///
/// Native only: it spawns a subprocess, so it is not built for wasm targets.
///
/// On every connection it spawns `program` with `args` followed by the peer id as
/// a final argument, then reads the verdict from the command's stdout. okayeg
/// knows nothing about what the command does; the script is free to query
/// fossenCD's api, hit its database directly, or anything else.
///
/// The verdict is the whitespace-separated words on stdout:
///
/// - `pull` grants read (the peer may learn our updates),
/// - `push` grants write (the peer may submit updates to us),
/// - both words grant both.
///
/// Empty output, neither word, or a nonzero exit refuses the peer. A failure to
/// spawn the command also refuses: a broken authorizer denies rather than leaks.
#[cfg(feature = "native")]
pub struct CommandAuthorizer<Id> {
    program: std::ffi::OsString,
    args: Vec<std::ffi::OsString>,
    _id: std::marker::PhantomData<fn(Id)>,
}

#[cfg(feature = "native")]
impl<Id> CommandAuthorizer<Id> {
    /// Authorize each peer by running `program` (with no extra args). The peer id
    /// is appended as the command's only argument.
    pub fn new(program: impl AsRef<std::ffi::OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            _id: std::marker::PhantomData,
        }
    }

    /// Pass a fixed leading argument to the command, before the peer id. Chainable.
    pub fn arg(mut self, arg: impl AsRef<std::ffi::OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }
}

#[cfg(feature = "native")]
impl<Id: std::fmt::Display> Authorizer for CommandAuthorizer<Id> {
    type Id = Id;

    async fn authorize(&self, who: Id) -> Option<Perms> {
        use std::process::Stdio;
        let output = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .arg(who.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;

        if !output.status.success() {
            return None;
        }
        parse_verdict(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Read a verdict from the command's stdout. `None` (refuse) unless at least one
/// of `pull`/`push` is granted.
#[cfg(feature = "native")]
fn parse_verdict(stdout: &str) -> Option<Perms> {
    let mut perms = Perms { pull: false, push: false };
    for word in stdout.split_whitespace() {
        match word {
            "pull" => perms.pull = true,
            "push" => perms.push = true,
            _ => {}
        }
    }
    (perms.pull || perms.push).then_some(perms)
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[test]
    fn verdict_words_grant_each_right() {
        assert_eq!(parse_verdict("pull push"), Some(Perms { pull: true, push: true }));
        assert_eq!(parse_verdict("pull"), Some(Perms { pull: true, push: false }));
        assert_eq!(parse_verdict("push\n"), Some(Perms { pull: false, push: true }));
    }

    #[test]
    fn empty_or_unknown_output_refuses() {
        assert_eq!(parse_verdict(""), None);
        assert_eq!(parse_verdict("   \n"), None);
        assert_eq!(parse_verdict("deny"), None);
    }

    #[tokio::test]
    async fn command_grants_from_script_output() {
        // A "script" that prints a verdict regardless of the peer id.
        let authz = CommandAuthorizer::<u64>::new("sh").arg("-c").arg("echo pull push");
        assert_eq!(authz.authorize(7).await, Some(Perms { pull: true, push: true }));
    }

    #[tokio::test]
    async fn command_can_branch_on_the_peer_id() {
        // sh -c assigns the first arg after the script to $0, so the placeholder
        // "authz" is $0 and the appended peer id is $1, as a real script sees it.
        let authz = CommandAuthorizer::<u64>::new("sh")
            .arg("-c")
            .arg(r#"[ "$1" = "42" ] && echo pull"#)
            .arg("authz");
        assert_eq!(authz.authorize(42).await, Some(Perms { pull: true, push: false }));
        assert_eq!(authz.authorize(1).await, None);
    }

    #[tokio::test]
    async fn nonzero_exit_refuses() {
        let authz = CommandAuthorizer::<u64>::new("false");
        assert_eq!(authz.authorize(1).await, None);
    }

    #[tokio::test]
    async fn failure_to_spawn_refuses() {
        let authz = CommandAuthorizer::<u64>::new("definitely-not-a-real-program-okayeg");
        assert_eq!(authz.authorize(1).await, None);
    }
}
