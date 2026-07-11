//! The authz seam: deciding what a connecting peer may do.
//!
//! When a peer connects, [`Transport::accept`](crate::Transport::accept) asks an
//! [`Authorizer`] to resolve the peer's id to its [`Perms`], or to refuse it. The
//! trait is the pluggable hook: a closure works for in-process policy, and
//! [`CommandAuthorizer`] shells out to a script so the real decision can live
//! anywhere (a web api call, a direct db query, an ldap lookup).
//!
//! The id handed to the authorizer is whatever the transport names a peer by. For
//! the iroh transport that is the peer's ed25519 public key, which iroh has
//! already authenticated in its handshake. A future authn seam could verify a
//! master-signed certificate first and hand this trait a longer-lived principal
//! (a pqc master key, say) instead of the raw transport key. Nothing here assumes
//! the id is an ed key, so that promotion would not disturb this code.

use std::future::Future;

use okayeg::Perms;

/// The authorizer's answer for a peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Admit the peer with these rights.
    Grant(Perms),
    /// Refuse the peer, with optional text to show them.
    Deny { message: Option<String> },
}

impl From<Option<Perms>> for Decision {
    /// `Some` grants, `None` refuses with no message.
    fn from(perms: Option<Perms>) -> Self {
        match perms {
            Some(perms) => Decision::Grant(perms),
            None => Decision::Deny { message: None },
        }
    }
}

/// Resolves a connecting peer to the rights it may exercise.
///
/// Implementations decide policy: a closure for something in-process, or
/// [`CommandAuthorizer`] to defer to an external script. A [`Decision::Deny`]
/// refuses the peer; the accepting side relays its message and closes before any
/// sync runs.
#[allow(async_fn_in_trait)]
pub trait Authorizer {
    /// How the transport names the peer being authorized.
    type Id;

    /// Resolve `who` to a [`Decision`]: grant it perms or refuse it.
    async fn authorize(&self, who: Self::Id) -> Decision;
}

/// Wrap a closure as an [`Authorizer`]. Lets an in-process policy be written
/// inline, without a named type:
///
/// ```ignore
/// node.accept(&from_fn(|who| async move { trusted.get(&who).copied() })).await
/// ```
///
/// The closure returns anything that is [`Into<Decision>`], so a yes/no policy
/// can keep yielding `Option<Perms>` and rely on the [`From`] adapter; a policy
/// that wants to name a reason returns a [`Decision`] directly.
///
/// A bare closure cannot impl the trait directly (Rust cannot pin down the id and
/// future types from an `Fn` bound), so this wrapper carries them.
pub fn from_fn<Id, F, Fut, D>(f: F) -> FnAuthorizer<Id, F>
where
    F: Fn(Id) -> Fut,
    Fut: Future<Output = D>,
    D: Into<Decision>,
{
    FnAuthorizer {
        f,
        _id: std::marker::PhantomData,
    }
}

/// An [`Authorizer`] backed by a closure. Built by [`from_fn`].
pub struct FnAuthorizer<Id, F> {
    f: F,
    _id: std::marker::PhantomData<fn(Id)>,
}

impl<Id, F, Fut, D> Authorizer for FnAuthorizer<Id, F>
where
    F: Fn(Id) -> Fut,
    Fut: Future<Output = D>,
    D: Into<Decision>,
{
    type Id = Id;

    async fn authorize(&self, who: Id) -> Decision {
        (self.f)(who).await.into()
    }
}

/// An [`Authorizer`] that runs an external command to decide each peer.
///
/// Native only: it spawns a subprocess, so it is not built for wasm targets.
///
/// On every connection it spawns `program` with `args`, writes the peer id (plus
/// a trailing newline) to the command's stdin, then reads the verdict from its
/// stdout. okayeg knows nothing about what the command does; the script is free
/// to call the embedding application's api, hit a database directly, or anything
/// else.
///
/// The first line of stdout decides; any later lines are a message shown to the
/// peer. On the first line:
///
/// - `pull` grants read (the peer may learn our updates),
/// - `push` grants write (the peer may submit updates to us),
/// - both words grant both,
/// - anything else refuses.
///
/// To refuse with a message, put it on the lines after the verdict:
///
/// ```text
/// deny
/// you are not a member of this project; ask an owner to add you
/// ```
///
/// Empty output, neither grant word, or a nonzero exit refuses the peer. A
/// failure to spawn the command also refuses: a broken authorizer denies rather
/// than leaks.
#[cfg(feature = "native")]
pub struct CommandAuthorizer<Id> {
    program: std::ffi::OsString,
    args: Vec<std::ffi::OsString>,
    _id: std::marker::PhantomData<fn(Id)>,
}

#[cfg(feature = "native")]
impl<Id> CommandAuthorizer<Id> {
    /// Authorize each peer by running `program` (with no extra args). The peer id
    /// is written to the command's stdin, newline-terminated.
    pub fn new(program: impl AsRef<std::ffi::OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            _id: std::marker::PhantomData,
        }
    }

    /// Pass a fixed argument to the command. Chainable.
    pub fn arg(mut self, arg: impl AsRef<std::ffi::OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }
}

#[cfg(feature = "native")]
impl<Id: std::fmt::Display> Authorizer for CommandAuthorizer<Id> {
    type Id = Id;

    async fn authorize(&self, who: Id) -> Decision {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;
        let deny = || Decision::Deny { message: None };
        let Ok(mut child) = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            return deny();
        };

        // A command that decides without reading stdin (say, a blanket grant) may
        // exit before we write; the resulting EPIPE is not a refusal, so the write
        // error is ignored and the exit status alone decides.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(format!("{who}\n").as_bytes()).await;
            // dropping stdin closes it, so a `read`-ing script sees EOF
        }
        let Ok(output) = child.wait_with_output().await else {
            return deny();
        };

        if !output.status.success() {
            return deny();
        }
        parse_verdict(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Parse the command's stdout: the first line grants or denies, later lines are
/// the message. Anything that is not a grant refuses.
#[cfg(feature = "native")]
fn parse_verdict(stdout: &str) -> Decision {
    let mut lines = stdout.lines();
    let first = lines.next().unwrap_or("");

    let mut perms = Perms {
        pull: false,
        push: false,
    };
    for word in first.split_whitespace() {
        match word {
            "pull" => perms.pull = true,
            "push" => perms.push = true,
            _ => {}
        }
    }

    if perms.pull || perms.push {
        return Decision::Grant(perms);
    }

    // Only a denial carries a message, so gather the later lines here rather than
    // on the grant path this walks away from.
    let rest = lines.collect::<Vec<_>>().join("\n");
    let rest = rest.trim();
    let message = (!rest.is_empty()).then(|| rest.to_string());
    Decision::Deny { message }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    fn grant(pull: bool, push: bool) -> Decision {
        Decision::Grant(Perms { pull, push })
    }

    #[test]
    fn verdict_words_grant_each_right() {
        assert_eq!(parse_verdict("pull push"), grant(true, true));
        assert_eq!(parse_verdict("pull"), grant(true, false));
        assert_eq!(parse_verdict("push\n"), grant(false, true));
    }

    #[test]
    fn empty_or_unknown_output_refuses() {
        let no_msg = Decision::Deny { message: None };
        assert_eq!(parse_verdict(""), no_msg);
        assert_eq!(parse_verdict("   \n"), no_msg);
        assert_eq!(parse_verdict("deny"), no_msg);
        // A word that is not a grant still refuses.
        assert_eq!(parse_verdict("nope"), no_msg);
    }

    #[test]
    fn deny_carries_its_message() {
        assert_eq!(
            parse_verdict("deny\nregister at https://host/enroll?key=abc"),
            Decision::Deny {
                message: Some("register at https://host/enroll?key=abc".into())
            }
        );
        // A multi-line message is joined and trimmed.
        assert_eq!(
            parse_verdict("deny\nline one\nline two\n"),
            Decision::Deny {
                message: Some("line one\nline two".into())
            }
        );
    }

    #[test]
    fn option_perms_adapts_to_a_decision() {
        assert_eq!(
            Decision::from(Some(Perms::all())),
            Decision::Grant(Perms::all())
        );
        assert_eq!(Decision::from(None), Decision::Deny { message: None });
    }

    #[tokio::test]
    async fn command_grants_from_script_output() {
        // A "script" that prints a verdict regardless of the peer id.
        let authz = CommandAuthorizer::<u64>::new("sh")
            .arg("-c")
            .arg("echo pull push");
        assert_eq!(authz.authorize(7).await, grant(true, true));
    }

    #[tokio::test]
    async fn command_can_branch_on_the_peer_id() {
        // The id arrives newline-terminated on stdin, as a real script reads it.
        let authz = CommandAuthorizer::<u64>::new("sh")
            .arg("-c")
            .arg(r#"read id; [ "$id" = "42" ] && echo pull || echo deny"#);
        assert_eq!(authz.authorize(42).await, grant(true, false));
        assert_eq!(authz.authorize(1).await, Decision::Deny { message: None });
    }

    #[tokio::test]
    async fn command_that_ignores_stdin_still_decides() {
        // `false` exits without reading; the EPIPE on our stdin write must not
        // mask the exit status, and a fast blanket grant must still land.
        let authz = CommandAuthorizer::<u64>::new("sh")
            .arg("-c")
            .arg("exec echo pull");
        assert_eq!(authz.authorize(7).await, grant(true, false));
    }

    #[tokio::test]
    async fn nonzero_exit_refuses() {
        let authz = CommandAuthorizer::<u64>::new("false");
        assert_eq!(authz.authorize(1).await, Decision::Deny { message: None });
    }

    #[tokio::test]
    async fn failure_to_spawn_refuses() {
        let authz = CommandAuthorizer::<u64>::new("definitely-not-a-real-program-okayeg");
        assert_eq!(authz.authorize(1).await, Decision::Deny { message: None });
    }
}
