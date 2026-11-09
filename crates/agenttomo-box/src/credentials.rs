//! Linux per-thread filesystem credentials, without changing the process UID.
//! Runtime and IO threads inherit these credentials when they are created.
use nix::unistd::{Gid, Uid, setfsgid, setfsuid};
use std::io;

pub fn filesystem_identity(uid: u32, gid: u32) -> io::Result<()> {
    setfsgid(Gid::from_raw(gid));
    setfsuid(Uid::from_raw(uid));
    // Linux silently rejects invalid filesystem identities; -1 queries them.
    if setfsgid(Gid::from_raw(u32::MAX)).as_raw() != gid
        || setfsuid(Uid::from_raw(u32::MAX)).as_raw() != uid
    {
        return Err(io::Error::other(format!(
            "cannot use recording filesystem identity {uid}:{gid}"
        )));
    }
    Ok(())
}
