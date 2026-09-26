//! Peer identity from the kernel (`SO_PEERCRED`) and the group database, never from the wire.

use std::os::unix::net::UnixStream;

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

use crate::shared::paths::ADMIN_GROUP;

/// Who is on the other end of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Peer {
    Known {
        uid: u32,
        admin: bool,
    },
    /// `SO_PEERCRED` failed; never authorized.
    Unknown,
}

/// Proof a peer may change config: root or a member of the admin group. Built only by
/// [`Peer::admin`].
pub(super) struct Admin {
    uid: u32,
}

impl Admin {
    pub(super) fn uid(&self) -> u32 {
        self.uid
    }
}

impl Peer {
    pub(super) fn of(stream: &UnixStream) -> Peer {
        match getsockopt(stream, PeerCredentials) {
            Ok(cred) => {
                let uid = cred.uid();
                Peer::Known {
                    uid,
                    admin: uid == 0 || uid_in_group(uid, ADMIN_GROUP),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "ipc: peer credentials unavailable");
                Peer::Unknown
            }
        }
    }

    pub(super) fn admin(&self) -> Option<Admin> {
        match *self {
            Peer::Known { uid, admin: true } => Some(Admin { uid }),
            Peer::Known { admin: false, .. } | Peer::Unknown => None,
        }
    }

    pub(super) fn uid(&self) -> Option<u32> {
        match *self {
            Peer::Known { uid, .. } => Some(uid),
            Peer::Unknown => None,
        }
    }
}

/// Reads the group DB rather than the peer's process groups, so `usermod -aG` applies
/// without re-login.
fn uid_in_group(uid: u32, group: &str) -> bool {
    let Some(user) = uzers::get_user_by_uid(uid) else {
        return false;
    };
    uzers::get_user_groups(user.name(), user.primary_group_id())
        .into_iter()
        .flatten()
        .any(|g| g.name().to_str() == Some(group))
}
