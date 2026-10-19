use std::{
    fs,
    os::unix::fs::{DirBuilderExt as _, MetadataExt as _},
    path::{Path, PathBuf},
};

/// Prepare the persistent guest backing directory, never the host control root.
///
/// # Errors
/// Refuses symlinks, foreign ownership and group/other-writable directories.
pub fn prepare_persistent_guest_home(path: &Path, uid: u32) -> Result<PathBuf, &'static str> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|_| "cannot create selected guest home")?;
        }
        Err(_) => return Err("cannot inspect selected guest home"),
        Ok(_) => {}
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| "cannot inspect selected guest home")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("selected guest home must be a directory");
    }
    if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err("selected guest home ownership or mode is unsafe");
    }
    path.canonicalize()
        .map_err(|_| "cannot resolve selected guest home")
}
