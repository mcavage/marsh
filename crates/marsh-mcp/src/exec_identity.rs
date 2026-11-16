//! Identity of the pinned executables and managed directories, captured when
//! they were checked so a later swap can be detected.

use super::{
    Deserialize, Digest, MetadataExt, OpenOptionsExt, Path, PermissionsExt, Read, Serialize,
    Sha256, fs, validate_executable_metadata, validate_trusted_parent_chain,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExecutableIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    pub(crate) modified_seconds: i64,
    pub(crate) modified_nanoseconds: i64,
    pub(crate) changed_seconds: i64,
    pub(crate) changed_nanoseconds: i64,
    pub(crate) sha256: [u8; 32],
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DirectoryIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) uid: u32,
    pub(crate) mode: u32,
}

/// Where a test can change the world while an identity is captured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureStage {
    /// The executable is open and checked; hashing has not started.
    Opened,
    /// Hashed and re-`fstat`ed; the path has not been looked at again.
    Hashed,
}

impl ExecutableIdentity {
    pub(crate) fn capture(path: &Path) -> Result<Self, String> {
        Self::capture_with_parent_policy(path, true)
    }

    pub(crate) fn capture_external(path: &Path) -> Result<Self, String> {
        Self::capture_with_parent_policy(path, false)
    }

    pub(crate) fn capture_with_parent_policy(
        path: &Path,
        require_trusted_parents: bool,
    ) -> Result<Self, String> {
        Self::capture_observed(path, require_trusted_parents, &mut |_| {})
    }

    /// Opens the executable once, without following a symlink, and takes the
    /// metadata, the checks and the hash from that descriptor, so they describe
    /// one file. `hook` runs at each point a swap of the path could land (see [`CaptureStage`]).
    pub(crate) fn capture_observed(
        path: &Path,
        require_trusted_parents: bool,
        hook: &mut dyn FnMut(CaptureStage),
    ) -> Result<Self, String> {
        let not_regular = || {
            format!(
                "configured executable is no longer a regular file: {}",
                path.display()
            )
        };
        // NONBLOCK: opening a FIFO for reading must not wait for a writer; the
        // descriptor is checked to be a regular file before anything is read.
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                    .bits()
                    .cast_signed(),
            )
            .open(path)
            .map_err(|error| {
                if error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
                    not_regular()
                } else {
                    format!("cannot open executable for verification: {error}")
                }
            })?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect executable identity: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err(not_regular());
        }
        validate_executable_metadata(path, &metadata, "configured executable")?;
        if require_trusted_parents {
            validate_trusted_parent_chain(path)?;
        }
        hook(CaptureStage::Opened);
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; 65_536].into_boxed_slice();
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| format!("cannot hash executable: {error}"))?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        let after = file
            .metadata()
            .map_err(|error| format!("cannot recheck executable identity: {error}"))?;
        hook(CaptureStage::Hashed);
        let named = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot recheck executable path: {error}"))?;
        let same = |left: &fs::Metadata, right: &fs::Metadata| {
            (left.dev(), left.ino()) == (right.dev(), right.ino())
        };
        if !same(&metadata, &named)
            || !same(&metadata, &after)
            || (
                metadata.size(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ) != (
                after.size(),
                after.mtime(),
                after.mtime_nsec(),
                after.ctime(),
                after.ctime_nsec(),
            )
        {
            return Err(format!(
                "executable was replaced or modified while it was verified: {}",
                path.display()
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            sha256: digest.finalize().into(),
        })
    }
}

impl DirectoryIdentity {
    pub(crate) fn capture(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect managed owner-only directory: {error}"))?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(format!(
                "managed directory must remain owner-only and must not be a symlink: {}",
                path.display()
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            mode: metadata.mode(),
        })
    }
}
