//! Read-only descriptor-bound filesystem incarnation identity on macOS.
//!
//! Apple sys/attr.h specifies `PATH_FROM_ID` as persistent, non-recycled object
//! IDs, and `64BIT_OBJECT_IDS` requires `ATTR_CMN_FILEID`. This API requires both
//! capabilities valid AND supported and a nonzero volume UUID and file ID.
//! It never substitutes `st_dev`, a pathname lookup, birthtime or `st_gen`.
//! A trusted host cloning/reformatting a volume is outside this identity premise.
use std::{fs::File, io};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistentIdentity {
    volume_uuid: [u8; 16],
    file_id: u64,
}

impl PersistentIdentity {
    #[must_use]
    pub fn volume_uuid(&self) -> [u8; 16] {
        self.volume_uuid
    }
    #[must_use]
    pub fn file_id(&self) -> u64 {
        self.file_id
    }
}

/// Read the identity of the opened object, not its current pathname.
///
/// # Errors
/// Rejects unsupported platforms/filesystems, invalid or truncated attributes,
/// missing capability bits, zero identities and native syscall failures.
pub fn read(file: &File) -> io::Result<PersistentIdentity> {
    #[cfg(target_os = "macos")]
    {
        native::read(file)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable host-source identity requires macOS filesystem PATH_FROM_ID and 64-bit object IDs",
        ))
    }
}

/// Clone `source` (relative to `source_dir`) to `target` (relative to
/// `target_dir`) without following a symlink at either final component
/// (`clonefileat(..., CLONE_NOFOLLOW)`; a directory clones recursively, and
/// symlinks inside it are cloned as symlinks). The target must not exist.
///
/// # Errors
/// Returns the native error, or `Unsupported` off macOS.
pub fn clone_at(
    source_dir: &impl std::os::fd::AsFd,
    source: &std::ffi::OsStr,
    target_dir: &impl std::os::fd::AsFd,
    target: &std::ffi::OsStr,
) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        native::clone_at(source_dir.as_fd(), source, target_dir.as_fd(), target)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (source_dir, source, target_dir, target);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "clonefileat requires macOS",
        ))
    }
}

#[cfg(any(target_os = "macos", test))]
fn decode(volume: &[u8; 52], object: &[u8; 12]) -> io::Result<PersistentIdentity> {
    const REQUIRED: u32 = 0x0000_4000 | 0x0002_0000;
    let word =
        |offset| u32::from_ne_bytes(volume[offset..offset + 4].try_into().expect("fixed layout"));
    let uuid: [u8; 16] = volume[36..52].try_into().expect("fixed layout");
    let file_id = u64::from_ne_bytes(object[4..12].try_into().expect("fixed layout"));
    if word(0) != 52 || u32::from_ne_bytes(object[..4].try_into().expect("fixed layout")) != 12 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated or unexpected native attribute layout",
        ));
    }
    if word(4) & REQUIRED != REQUIRED
        || word(20) & REQUIRED != REQUIRED
        || uuid == [0; 16]
        || file_id == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "volume does not establish persistent non-recycled 64-bit object identity",
        ));
    }
    Ok(PersistentIdentity {
        volume_uuid: uuid,
        file_id,
    })
}

#[cfg(target_os = "macos")]
mod native {
    use super::{PersistentIdentity, decode};
    use std::{fs::File, io, os::fd::AsRawFd};

    /// `sys/clonefile.h`: do not follow a symlink at the source.
    const CLONE_NOFOLLOW: u32 = 0x0001;

    #[allow(unsafe_code)]
    pub(super) fn clone_at(
        source_dir: std::os::fd::BorrowedFd<'_>,
        source: &std::ffi::OsStr,
        target_dir: std::os::fd::BorrowedFd<'_>,
        target: &std::ffi::OsStr,
    ) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let source = std::ffi::CString::new(source.as_bytes())?;
        let target = std::ffi::CString::new(target.as_bytes())?;
        // SAFETY: both descriptors are borrowed live for the synchronous call
        // and both names are NUL-terminated strings owned for its duration.
        let result = unsafe {
            libc::clonefileat(
                source_dir.as_raw_fd(),
                source.as_ptr(),
                target_dir.as_raw_fd(),
                target.as_ptr(),
                CLONE_NOFOLLOW,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn read(file: &File) -> io::Result<PersistentIdentity> {
        let mut volume = [0u8; 52];
        let mut object = [0u8; 12];
        attributes(
            file,
            0,
            libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES | libc::ATTR_VOL_UUID,
            &mut volume,
        )?;
        // Volume and FILEID attributes cannot be combined in one request.
        attributes(file, libc::ATTR_CMN_FILEID, 0, &mut object)?;
        decode(&volume, &object)
    }

    #[allow(unsafe_code)]
    fn attributes(file: &File, commonattr: u32, volattr: u32, buffer: &mut [u8]) -> io::Result<()> {
        let mut request = libc::attrlist {
            bitmapcount: 5,
            reserved: 0,
            commonattr,
            volattr,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        // SAFETY: File owns a live borrowed descriptor for the synchronous call.
        // Both pointers reference initialized writable storage of the exact
        // declared sizes for the entire call. The kernel copies at most len;
        // no returned pointers or unaligned typed loads are used. options=0.
        let result = unsafe {
            libc::fgetattrlist(
                file.as_raw_fd(),
                std::ptr::from_mut(&mut request).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_layout_requires_complete_capabilities_and_nonzero_identity() {
        let mut volume = [0u8; 52];
        volume[..4].copy_from_slice(&52_u32.to_ne_bytes());
        volume[4..8].copy_from_slice(&0x0002_4000_u32.to_ne_bytes());
        volume[20..24].copy_from_slice(&0x0002_4000_u32.to_ne_bytes());
        volume[36..52].copy_from_slice(&[7; 16]);
        let mut object = [0u8; 12];
        object[..4].copy_from_slice(&12_u32.to_ne_bytes());
        object[4..].copy_from_slice(&9_u64.to_ne_bytes());
        assert_eq!(decode(&volume, &object).unwrap().file_id(), 9);
        for offset in [0, 4, 20, 36] {
            let mut bad = volume;
            bad[offset..offset + if offset == 36 { 16 } else { 4 }].fill(0);
            assert!(decode(&bad, &object).is_err());
        }
        for bit in [0x0000_4000_u32, 0x0002_0000_u32] {
            for offset in [4, 20] {
                let mut bad = volume;
                bad[offset..offset + 4].copy_from_slice(&bit.to_ne_bytes());
                assert!(decode(&bad, &object).is_err());
            }
        }
        let mut bad = object;
        bad[4..].fill(0);
        assert!(decode(&volume, &bad).is_err());
        bad = object;
        bad[..4].copy_from_slice(&8_u32.to_ne_bytes());
        assert!(decode(&volume, &bad).is_err());
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn unsupported_host_is_not_mistaken_for_persistent_identity() {
        let file = tempfile::tempfile().unwrap();
        assert_eq!(read(&file).unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn real_descriptors_preserve_identity_across_rename_not_replacement() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("original");
        let moved = root.path().join("moved");
        std::fs::create_dir(&old).unwrap();
        let pin = File::open(&old).unwrap();
        let identity = read(&pin).expect("this native test requires a supported volume");
        std::fs::rename(&old, &moved).unwrap();
        std::fs::create_dir(&old).unwrap();
        assert_eq!(read(&pin).unwrap(), identity);
        assert_eq!(read(&File::open(&moved).unwrap()).unwrap(), identity);
        assert_ne!(read(&File::open(&old).unwrap()).unwrap(), identity);
        eprintln!("native persistent identity: {identity:?}");
    }
}
