//! Who is on the other end of a connection.
//!
//! The answer comes from the kernel (`SO_PEERCRED`) and the group database, never
//! from the wire — a client cannot claim a uid, so [`Auth`] is the one fact in
//! this module that no caller can forge. That is why the authz gate in
//! [`super::dispatch::handle`] can be a plain `if`: the hard part is establishing
//! the identity, not checking it.
//!
//! Kept apart from both siblings because it changes for its own reasons — the
//! permission model — and because its imports (`nix`, `uzers`, [`ADMIN_GROUP`])
//! appear nowhere else in the server.

use std::os::unix::net::UnixStream;

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

use crate::shared::paths::ADMIN_GROUP;

/// The authenticated peer of a connection, from `SO_PEERCRED` (kernel-provided,
/// unspoofable). Resolved once per connection.
#[derive(Clone, Copy)]
pub(super) struct Auth {
    pub(super) uid: u32,
    pub(super) in_admin_group: bool,
}

/// Whether a peer may mutate config: root, or a member of [`ADMIN_GROUP`]. Pure.
pub(super) fn authorized(uid: u32, in_admin_group: bool) -> bool {
    uid == 0 || in_admin_group
}

/// Read the connection's peer credentials and resolve group membership. Fails
/// CLOSED — an unreadable peer is treated as unprivileged, never authorized.
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

/// Whether `uid`'s group memberships (resolved from the group DB, so `usermod
/// -aG` takes effect without a re-login) include `group`.
fn uid_in_group(uid: u32, group: &str) -> bool {
    let Some(user) = uzers::get_user_by_uid(uid) else {
        return false;
    };
    uzers::get_user_groups(user.name(), user.primary_group_id())
        .into_iter()
        .flatten()
        .any(|g| g.name().to_str() == Some(group))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorized_only_for_root_or_group_member() {
        assert!(authorized(0, false)); // root
        assert!(authorized(1000, true)); // group member
        assert!(!authorized(1000, false)); // neither
    }
}
