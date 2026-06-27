//! The gate: which credentials may sync this repo, and how.
//!
//! Trust is local and per-repo. It lives in `.eg/trust`, one credential per
//! line, and is read fresh each time a peer connects so an edit (including a
//! revocation) takes effect without a restart. A credential is an endpoint id
//! for now; tokens for browser peers slot in here later.
//!
//! Line format, whitespace separated, flags in any order:
//!
//! ```text
//! # comments and blank lines are ignored
//! <endpoint-id> pull push user:alice
//! <endpoint-id> pull              # read-only
//! <endpoint-id> pull push revoked # kept on record, but refused
//! ```

use std::io;
use std::path::Path;
use std::str::FromStr;

use okayeg_net::{EndpointId, Perms};

use crate::workspace::Workspace;

/// The trust set, under the repo's `.eg/`.
const TRUST_PATH: &str = ".eg/trust";

/// A parsed trust set: who this repo will sync with, and what each may do.
pub struct Trust {
    rows: Vec<Grant>,
}

/// What one credential is granted: both a parsed `.eg/trust` row and, once
/// [`grants`](Trust::grants) collapses duplicates, the effective grant per id.
#[derive(Clone, Copy)]
pub struct Grant {
    pub id: EndpointId,
    pub perms: Perms,
    pub revoked: bool,
}

impl Trust {
    /// Load the trust set. A missing file means "trust no one", the secure
    /// default for a fresh repo.
    pub fn load(ws: &dyn Workspace) -> io::Result<Self> {
        let text = match ws.read_file(Path::new(TRUST_PATH)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut rows = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            rows.push(Grant::parse(line).map_err(|msg| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(".eg/trust line {}: {msg}", i + 1),
                )
            })?);
        }
        Ok(Self { rows })
    }

    /// What `id` is granted, or `None` if it is unknown, revoked, or granted
    /// nothing. `None` is the caller's cue to refuse the connection.
    ///
    /// The last matching row wins, so appending a later row (e.g. a revocation)
    /// overrides an earlier grant.
    pub fn perms(&self, id: EndpointId) -> Option<Perms> {
        let row = self.rows.iter().rev().find(|r| r.id == id)?;
        if row.revoked || !(row.perms.pull || row.perms.push) {
            return None;
        }
        Some(row.perms)
    }

    /// The effective grant for each credential, last row winning like
    /// [`perms`](Self::perms), in the order ids first appear.
    pub fn grants(&self) -> Vec<Grant> {
        let mut grants: Vec<Grant> = Vec::new();
        for &row in &self.rows {
            match grants.iter_mut().find(|g| g.id == row.id) {
                Some(existing) => *existing = row,
                None => grants.push(row),
            }
        }
        grants
    }
}

impl Grant {
    fn parse(line: &str) -> Result<Self, String> {
        let mut tokens = line.split_whitespace();
        let id_tok = tokens.next().ok_or("missing endpoint id")?;
        let id = EndpointId::from_str(id_tok).map_err(|e| format!("bad endpoint id: {e}"))?;
        let mut perms = Perms {
            pull: false,
            push: false,
        };
        let mut revoked = false;
        for tok in tokens {
            match tok {
                "pull" => perms.pull = true,
                "push" => perms.push = true,
                "revoked" => revoked = true,
                _ if tok.starts_with("user:") => {} // label, not used for gating yet
                other => return Err(format!("unknown flag {other:?}")),
            }
        }
        Ok(Self { id, perms, revoked })
    }
}

/// Append a grant for `id` to `.eg/trust`, creating the file if needed.
///
/// Records, not replaces: a later row overrides an earlier one (see
/// [`Trust::perms`]), so this also serves to change or revoke a grant.
pub fn add(ws: &dyn Workspace, id: EndpointId, perms: Perms) -> io::Result<()> {
    let mut text = match ws.read_file(Path::new(TRUST_PATH)) {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{id} {}\n", flags(perms)));
    ws.write_private(Path::new(TRUST_PATH), text.as_bytes())
}

/// The granted directions as space-separated flags, e.g. `pull push`.
pub fn flags(perms: Perms) -> String {
    let mut out = Vec::new();
    if perms.pull {
        out.push("pull");
    }
    if perms.push {
        out.push("push");
    }
    out.join(" ")
}
