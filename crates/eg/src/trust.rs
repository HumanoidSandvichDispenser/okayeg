//! The gate: which credentials may sync this repo, and how.
//!
//! Trust is local and per-repo. It lives in `.eg/trust` as TOML, one table per
//! credential, and is read fresh each time a peer connects so an edit (or a
//! removal) takes effect without a restart. A credential is an endpoint id for
//! now; tokens for browser peers slot in here later.
//!
//! ```toml
//! [peers."<endpoint-id>"]
//! pull = true
//! push = true
//! label = "alice"   # optional, not used for gating
//! ```

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use okayeg_net::{EndpointId, Perms};

use crate::to_io;
use crate::workspace::{Workspace, open_repo};

/// The trust set, under the repo's `.eg/`.
const TRUST_PATH: &str = ".eg/trust";

/// The trust set: who this repo will sync with, and what each may do.
#[derive(Default)]
pub struct Trust {
    peers: HashMap<EndpointId, GrantData>,
}

/// One credential's stored grant.
#[derive(Clone)]
struct GrantData {
    perms: Perms,
    label: Option<String>,
}

/// A credential's effective grant, paired with its id for listing.
#[derive(Clone)]
pub struct Grant {
    pub id: EndpointId,
    pub perms: Perms,
    pub label: Option<String>,
}

/// A change to the trust set from `eg trust`.
#[derive(clap::Subcommand)]
pub(crate) enum TrustAction {
    /// Grant a peer access (default both).
    Add {
        peer: String,
        /// Any of `pull` / `push`; empty grants both.
        access: Vec<Access>,
    },
    /// Drop a peer's grant, or just the named perms if any are given.
    Remove {
        peer: String,
        /// Perms to drop; empty removes the peer entirely.
        access: Vec<Access>,
    },
    /// List the current grants.
    List,
}

/// A capability that can be granted to a peer with `eg trust`.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Access {
    Pull,
    Push,
}

/// Turn an access list into a [`Perms`]; empty means full access.
pub(crate) fn perms_from(access: &[Access]) -> Perms {
    if access.is_empty() {
        return Perms::all();
    }
    Perms {
        pull: access.contains(&Access::Pull),
        push: access.contains(&Access::Push),
    }
}

/// Apply a trust change against the repo at `dir`.
pub(crate) fn perform_action(dir: &Path, action: TrustAction) -> io::Result<()> {
    let ws = open_repo(dir)?;
    let mut trust = Trust::load(&ws)?;
    match action {
        TrustAction::Add { peer, access } => {
            let id = EndpointId::from_str(&peer).map_err(to_io)?;
            let perms = perms_from(&access);
            trust.set(id, perms);
            trust.save(&ws)?;
            println!("eg trust: {id} may {}", flags(perms));
        }
        TrustAction::Remove { peer, access } => {
            let id = EndpointId::from_str(&peer).map_err(to_io)?;
            let remaining = if access.is_empty() {
                trust.forget(id);
                None
            } else {
                trust.drop_perms(id, &access)
            };
            trust.save(&ws)?;
            match remaining {
                Some(perms) => println!("eg trust: {id} may {}", flags(perms)),
                None => println!("eg trust: {id} removed"),
            }
        }
        TrustAction::List => {
            let grants = trust.grants();
            if grants.is_empty() {
                println!("no peers trusted (grant access with eg trust add <id>)");
            }
            for g in grants {
                let flags = flags(g.perms);
                let flags = if flags.is_empty() { "none" } else { &flags };
                match &g.label {
                    Some(label) => println!("{}  {flags}  {label}", g.id),
                    None => println!("{}  {flags}", g.id),
                }
            }
        }
    }
    Ok(())
}

/// The on-disk shape of `.eg/trust`.
#[derive(Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    peers: BTreeMap<String, Row>,
}

/// One credential's row in the TOML file.
#[derive(Serialize, Deserialize)]
struct Row {
    #[serde(default)]
    pull: bool,
    #[serde(default)]
    push: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

impl Trust {
    /// Load the trust set from the workspace's `.eg/trust`. A missing file is treated as an empty
    /// trust set, while malformed files result in an error.
    pub fn load(ws: &dyn Workspace) -> io::Result<Self> {
        let text = match ws.read_file(Path::new(TRUST_PATH)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };

        let file: TrustFile = toml::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!(".eg/trust: {e}")))?;

        let mut peers = HashMap::new();

        for (id_str, row) in file.peers {
            let id = EndpointId::from_str(&id_str).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(".eg/trust: bad endpoint id {id_str:?}: {e}"),
                )
            })?;

            peers.insert(
                id,
                GrantData {
                    perms: Perms {
                        pull: row.pull,
                        push: row.push,
                    },
                    label: row.label,
                },
            );
        }

        Ok(Self { peers })
    }

    /// Write the trust set back to `.eg/trust`, owner-only. Peers are ordered by
    /// id so the file stays stable across rewrites.
    fn save(&self, ws: &dyn Workspace) -> io::Result<()> {
        let peers: BTreeMap<String, Row> = self
            .peers
            .iter()
            .map(|(id, g)| {
                (
                    id.to_string(),
                    Row {
                        pull: g.perms.pull,
                        push: g.perms.push,
                        label: g.label.clone(),
                    },
                )
            })
            .collect();

        let text = toml::to_string_pretty(&TrustFile { peers }).map_err(to_io)?;
        ws.write_private(Path::new(TRUST_PATH), text.as_bytes())
    }

    /// What `id` is granted, or `None` if it is unknown or granted nothing.
    /// `None` is the caller's cue to refuse the connection.
    pub fn perms(&self, id: EndpointId) -> Option<Perms> {
        let g = self.peers.get(&id)?;
        (g.perms.pull || g.perms.push).then_some(g.perms)
    }

    /// The effective grant for each credential, ordered by id.
    pub fn grants(&self) -> Vec<Grant> {
        let mut grants: Vec<Grant> = self
            .peers
            .iter()
            .map(|(id, g)| Grant {
                id: *id,
                perms: g.perms,
                label: g.label.clone(),
            })
            .collect();
        grants.sort_by_key(|g| g.id.to_string());
        grants
    }

    /// Set `id`'s perms, keeping any existing label.
    fn set(&mut self, id: EndpointId, perms: Perms) {
        self.peers
            .entry(id)
            .or_insert_with(|| GrantData { perms, label: None })
            .perms = perms;
    }

    /// Drop `id` entirely.
    fn forget(&mut self, id: EndpointId) {
        self.peers.remove(&id);
    }

    /// Drop the named perms from `id`; if none remain, drop the peer. Returns
    /// the remaining perms, or `None` if the peer is now gone or was absent.
    fn drop_perms(&mut self, id: EndpointId, access: &[Access]) -> Option<Perms> {
        let g = self.peers.get_mut(&id)?;
        for a in access {
            match a {
                Access::Pull => g.perms.pull = false,
                Access::Push => g.perms.push = false,
            }
        }
        if g.perms.pull || g.perms.push {
            Some(g.perms)
        } else {
            self.peers.remove(&id);
            None
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::MemWorkspace;

    #[test]
    fn empty_access_list_grants_all_perms() {
        assert_eq!(perms_from(&[]), Perms::all());
    }

    #[test]
    fn pull_access_only_grants_pull_perm() {
        assert_eq!(
            perms_from(&[Access::Pull]),
            Perms {
                pull: true,
                push: false
            }
        );
    }

    #[test]
    fn push_access_only_grants_push_perm() {
        assert_eq!(
            perms_from(&[Access::Push]),
            Perms {
                pull: false,
                push: true
            }
        );
    }

    #[test]
    fn pull_and_push_access_grants_both_perms() {
        assert_eq!(
            perms_from(&[Access::Pull, Access::Push]),
            Perms {
                pull: true,
                push: true
            }
        );
    }

    #[test]
    fn duplicate_access_list_should_merge() {
        assert_eq!(
            perms_from(&[Access::Push, Access::Pull, Access::Pull]),
            Perms {
                pull: true,
                push: true
            }
        );
    }

    #[test]
    fn given_missing_file_when_load_then_return_empty_trust_set() {
        let ws = MemWorkspace::new();

        let trust = Trust::load(&ws).unwrap();

        let grants = trust.grants();
        assert_eq!(grants.len(), 0);
    }

    #[test]
    fn given_valid_file_when_load_then_return_trust_set() {
        let ws = MemWorkspace::new();
        const TRUST_FILE: &str = r#"
            [peers."0000000000000000000000000000000000000000000000000000000000000000"]
            pull = true
            push = false
        "#;
        ws.write_file(Path::new(TRUST_PATH), TRUST_FILE.as_bytes())
            .unwrap();

        let endpoint_id = EndpointId::from_bytes(&[0u8; 32]).unwrap();

        let trust = Trust::load(&ws).unwrap();
        assert_eq!(
            trust.perms(endpoint_id),
            Some(Perms {
                pull: true,
                push: false
            })
        );
    }

    #[test]
    fn given_malformed_toml_when_load_then_throw_invaliddata() {
        let ws = MemWorkspace::new();
        const TRUST_FILE: &str = r#"
            [forsen
        "#;
        ws.write_file(Path::new(TRUST_PATH), TRUST_FILE.as_bytes())
            .unwrap();

        let trust = Trust::load(&ws);
        assert_eq!(
            trust.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn given_bad_endpoint_id_when_load_then_throw_invaliddata() {
        let ws = MemWorkspace::new();
        const TRUST_FILE: &str = r#"
            [peers."not-a-valid-id"]
            pull = true
            push = false
        "#;
        ws.write_file(Path::new(TRUST_PATH), TRUST_FILE.as_bytes())
            .unwrap();

        let trust = Trust::load(&ws);
        assert_eq!(
            trust.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn given_duplicate_endpoint_id_when_load_then_invaliddata() {
        let ws = MemWorkspace::new();
        const TRUST_FILE: &str = r#"
            [peers."0000000000000000000000000000000000000000000000000000000000000000"]
            pull = true
            push = false

            [peers."0000000000000000000000000000000000000000000000000000000000000000"]
            pull = false
            push = true
        "#;

        ws.write_file(Path::new(TRUST_PATH), TRUST_FILE.as_bytes())
            .unwrap();

        let trust = Trust::load(&ws);
        assert_eq!(
            trust.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn given_invalid_utf8_in_file_when_load_then_invaliddata() {
        let ws = MemWorkspace::new();
        ws.write_file(Path::new(TRUST_PATH), &[0xff, 0xfe, 0xfd])
            .unwrap();

        let trust = Trust::load(&ws);
        assert_eq!(
            trust.err().map(|e| e.kind()),
            Some(io::ErrorKind::InvalidData)
        );
    }

    #[test]
    fn trust_perms_should_return_none_for_unknown_id() {
        let trust = Trust::default();

        assert!(
            trust
                .perms(EndpointId::from_bytes(&[0u8; 32]).unwrap())
                .is_none()
        );
    }

    #[test]
    fn given_row_without_push_or_pull_when_perms_then_return_none() {
        let ws = MemWorkspace::new();
        const TRUST_FILE: &str = r#"
            [peers."0000000000000000000000000000000000000000000000000000000000000000"]
            pull = false
            push = false
        "#;
        ws.write_file(Path::new(TRUST_PATH), TRUST_FILE.as_bytes())
            .unwrap();

        let trust = Trust::load(&ws).unwrap();
        let perms = trust.perms(EndpointId::from_bytes(&[0u8; 32]).unwrap());

        assert!(perms.is_none());
    }

    #[test]
    fn dropping_one_perm_should_keep_the_other() {
        let mut trust = Trust::default();
        let id = EndpointId::from_bytes(&[0u8; 32]).unwrap();
        trust.set(
            id,
            Perms {
                pull: true,
                push: true,
            },
        );

        trust.drop_perms(id, &[Access::Pull]);

        let perms = trust.perms(id);
        assert_eq!(
            perms,
            Some(Perms {
                pull: false,
                push: true
            })
        );
    }

    #[test]
    fn dropping_all_perms_should_remove_peer() {
        let mut trust = Trust::default();
        let id = EndpointId::from_bytes(&[0u8; 32]).unwrap();
        trust.set(
            id,
            Perms {
                pull: true,
                push: true,
            },
        );

        trust.drop_perms(id, &[Access::Pull, Access::Push]);

        assert!(trust.grants().is_empty());
    }

    #[test]
    fn dropping_non_existing_peer_should_return_none() {
        let mut trust = Trust::default();
        let id = EndpointId::from_bytes(&[0u8; 32]).unwrap();

        let remaining = trust.drop_perms(id, &[Access::Pull]);

        assert!(remaining.is_none());
    }

    #[test]
    fn set_keeps_existing_label_when_regranting() {
        let mut trust = Trust::default();
        let id = EndpointId::from_bytes(&[0u8; 32]).unwrap();

        trust.peers.insert(
            id,
            GrantData {
                perms: Perms {
                    pull: true,
                    push: false,
                },
                label: Some("test".to_string()),
            },
        );

        trust.set(
            id,
            Perms {
                pull: false,
                push: true,
            },
        );

        let grant = trust.peers.get(&id).unwrap();
        assert_eq!(grant.label, Some("test".to_string()));
    }

    #[test]
    fn grants_are_ordered_by_id() {
        let mut trust = Trust::default();
        let id1 = EndpointId::from_bytes(&[0u8; 32]).unwrap();
        let id2 = EndpointId::from_bytes(&[1u8; 32]).unwrap();
        let id3 = EndpointId::from_bytes(&[0xFFu8; 32]).unwrap();

        trust.set(
            id2,
            Perms {
                pull: true,
                push: false,
            },
        );
        trust.set(
            id3,
            Perms {
                pull: true,
                push: false,
            },
        );
        trust.set(
            id1,
            Perms {
                pull: false,
                push: true,
            },
        );

        let grants = trust.grants();
        assert_eq!(grants[0].id, id1);
        assert_eq!(grants[1].id, id2);
        assert_eq!(grants[2].id, id3);
    }

    #[test]
    fn round_trip_save_load_should_preserve_trust_set() {
        let ws = MemWorkspace::new();
        let mut trust = Trust::default();
        let id1 = EndpointId::from_bytes(&[0u8; 32]).unwrap();
        let id2 = EndpointId::from_bytes(&[1u8; 32]).unwrap();

        trust.set(
            id1,
            Perms {
                pull: true,
                push: false,
            },
        );
        trust.set(
            id2,
            Perms {
                pull: false,
                push: true,
            },
        );

        trust.save(&ws).unwrap();

        let loaded_trust = Trust::load(&ws).unwrap();

        assert_eq!(trust.grants().len(), loaded_trust.grants().len());
        for grant in trust.grants() {
            let loaded_perms = loaded_trust.perms(grant.id);
            assert_eq!(loaded_perms, Some(grant.perms));
        }
    }
}
