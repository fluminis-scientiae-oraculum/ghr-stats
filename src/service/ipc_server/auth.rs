//! Peer identity from the kernel (`SO_PEERCRED`) and the group database, never from the wire.

use std::os::unix::net::UnixStream;

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

use crate::shared::paths::ADMIN_GROUP;

/// Peer credentials, resolved once per connection.
#[derive(Clone, Copy)]
pub(super) struct Auth {
    pub(super) uid: u32,
    pub(super) in_admin_group: bool,
}

/// Whether a peer may mutate config.
pub(super) fn authorized(uid: u32, in_admin_group: bool) -> bool {
    uid == 0 || in_admin_group
}

pub(super) fn peer_auth(stream: &UnixStream) -> Auth {
    match getsockopt(stream, PeerCredentials) {
        Ok(cred) => {
            let uid = cred.uid();
            Auth {
                uid,
                in_admin_group: uid_in_group(uid, ADMIN_GROUP),
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "ipc: peer credentials unavailable — treating as unprivileged");
            Auth {
                uid: u32::MAX,
                in_admin_group: false,
            }
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
