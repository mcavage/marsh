//! Descriptor-bound rename authority checks. Root/current host UID are trusted;
//! other UIDs and every guest are not. Guest reachability is fenced separately
//! against complete stock exports, including inode aliases.
use crate::SbxError;
use std::{fs::File, os::unix::fs::MetadataExt, path::Path};

fn denied(path: &Path, reason: &'static str) -> SbxError {
    SbxError::UnsafeSourceAncestor {
        path: path.to_owned(),
        reason,
    }
}

pub(super) fn verify(file: &File, path: &Path, ancestor: bool) -> Result<(), SbxError> {
    let metadata = file.metadata()?;
    let uid = metadata.uid();
    if uid != 0 && uid != rustix::process::geteuid().as_raw() {
        return Err(denied(
            path,
            "another host UID owns a pathname component and can change its permissions",
        ));
    }
    // Every next component is independently checked for trusted ownership. A
    // sticky parent therefore protects its trusted child from other UIDs; an
    // ordinary writable parent does not, even when that child is mode 0700.
    if ancestor && metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0 {
        return Err(denied(
            path,
            "group/other-writable nonsticky ancestor permits pathname replacement",
        ));
    }
    #[cfg(target_os = "macos")]
    verify_acl(file, path, ancestor)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_acl(file: &File, path: &Path, ancestor: bool) -> Result<(), SbxError> {
    use calcifer_macos_acl::{FLAG_INHERITED, TAG_ALLOW, TAG_DENY, read_acl};
    use std::os::fd::AsFd;
    // Native Darwin kauth vnode rights 1..13. Other rights (including generic
    // WRITE/ALL), ACL-level flags and unhandled inheritance behavior fail closed.
    // These are native masks, not acl_get_perm_np's pathname-oriented interface.
    const KNOWN_RIGHTS: u32 = 0x3ffe;
    const DELETE: u32 = 1 << 4;
    const WRITE_SECURITY: u32 = 1 << 12;
    const CHANGE_OWNER: u32 = 1 << 13;
    const DIRECTORY_MUTATION: u32 = (1 << 2) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10);
    let acl = read_acl(file.as_fd())
        .map_err(|_| denied(path, "descriptor ACL could not be read completely"))?;
    if acl.flags != 0 {
        return Err(denied(path, "unknown/private ACL-level flags"));
    }
    for entry in acl.entries {
        if !matches!(entry.tag, TAG_ALLOW | TAG_DENY)
            || entry.flags & !FLAG_INHERITED != 0
            || entry.permissions & !KNOWN_RIGHTS != 0
        {
            return Err(denied(
                path,
                "unknown ACL tag/rights or unsupported inheritance flags",
            ));
        }
        let forbidden =
            DELETE | WRITE_SECURITY | CHANGE_OWNER | if ancestor { DIRECTORY_MUTATION } else { 0 };
        if entry.tag == TAG_ALLOW && entry.permissions & forbidden != 0 {
            return Err(denied(
                path,
                "ACL grants pathname/security mutation authority; sticky mode is not sufficient",
            ));
        }
        // Known DENY entries, including normal HOME everyone-deny-delete, add
        // no authority. Exported leaf content writes remain ordinary writes.
    }
    Ok(())
}
