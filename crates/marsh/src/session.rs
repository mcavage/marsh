//! Host session identity and path selection.

use serde::{Deserialize, Serialize};
use std::{env, fmt, path::PathBuf};

/// Owns a bounded private home slot during a host session. Drop never releases
/// authority or deletes data. Explicit release requires a trusted daemon drain
/// receipt and fresh native all-owner closure; failure retains the exact slot.
pub struct EphemeralHomeGuard {
    lease: marsh_sbx::EphemeralHomeLease,
    adapter: marsh_sbx::StockSbx,
}

impl fmt::Debug for EphemeralHomeGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EphemeralHomeGuard")
            .field("path", &self.path())
            .finish_non_exhaustive()
    }
}

impl EphemeralHomeGuard {
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        self.lease.path()
    }

    #[must_use]
    pub fn token(&self) -> marsh_sbx::EphemeralHomeToken {
        self.lease.token()
    }

    /// Revoke future private source admission without claiming writer absence.
    ///
    /// # Errors
    /// Retains the allocation if the native transaction cannot be persisted.
    pub fn close_admission(&self) -> Result<(), marsh_sbx::SbxError> {
        self.lease.close_admission()
    }

    /// Consume concrete all-owner closure only after the daemon's cleanup receipt.
    ///
    /// # Errors
    /// Keeps data/authority on pending, live, changed or unverifiable ownership.
    pub fn release(&self) -> Result<(), marsh_sbx::SbxError> {
        self.close_admission()?;
        self.adapter.release_ephemeral_home(&self.lease)
    }

    /// Keep a mount source when shell teardown cannot be confirmed.
    #[must_use]
    pub fn keep(self) -> PathBuf {
        self.lease.keep()
    }
}

/// The shell context approved by the host launcher.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub session_id: Option<String>,
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub launch_directory: PathBuf,
    pub guest_home: PathBuf,
    pub home_backing: PathBuf,
    pub ephemeral_home: bool,
}

#[derive(Debug)]
pub struct SessionError(&'static str);

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for SessionError {}

impl SessionConfig {
    /// Create the selected guest home below the host-only scope root.
    ///
    /// # Errors
    /// Rejects a symlink, another owner's directory, or loose permissions.
    pub fn ensure_persistent_home(&mut self) -> Result<(), SessionError> {
        if self.ephemeral_home {
            return Ok(());
        }
        if !self.home_backing.exists() {
            std::fs::create_dir(&self.home_backing)
                .map_err(|_| SessionError("cannot create selected guest home"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &self.home_backing,
                    std::fs::Permissions::from_mode(0o700),
                )
                .map_err(|_| SessionError("cannot protect selected guest home"))?;
            }
        }
        let metadata = std::fs::symlink_metadata(&self.home_backing)
            .map_err(|_| SessionError("cannot inspect selected guest home"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(SessionError("selected guest home must be a directory"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != self.uid || metadata.mode() & 0o022 != 0 {
                return Err(SessionError(
                    "selected guest home ownership or mode is unsafe",
                ));
            }
        }
        self.home_backing = self
            .home_backing
            .canonicalize()
            .map_err(|_| SessionError("cannot resolve selected guest home"))?;
        Ok(())
    }

    /// Capture the natural host paths once, before daemon attachment.
    ///
    /// # Errors
    /// Rejects missing or unsafe identity and path inputs.
    pub fn from_environment(ephemeral_home: bool) -> Result<Self, SessionError> {
        let username = env::var("USER")
            .or_else(|_| env::var("LOGNAME"))
            .map_err(|_| SessionError("USER or LOGNAME is required"))?;
        validate_username(&username)?;

        let launch_directory = env::current_dir()
            .map_err(|_| SessionError("cannot determine the launch directory"))?;
        if !launch_directory.is_absolute() {
            return Err(SessionError("launch directory must be absolute"));
        }

        let host_home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(SessionError("HOME is required"))?;
        if !host_home.is_absolute() {
            return Err(SessionError("HOME must be absolute"));
        }
        let scope_root =
            env::var_os("MARSH_HOME").map_or_else(|| host_home.join(".marsh"), PathBuf::from);
        if !scope_root.is_absolute() {
            return Err(SessionError("MARSH_HOME must be absolute"));
        }
        let home_backing = scope_root.join("home");

        #[cfg(unix)]
        let (uid, gid) = {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&host_home)
                .map_err(|_| SessionError("cannot inspect HOME ownership"))?;
            (metadata.uid(), metadata.gid())
        };
        #[cfg(not(unix))]
        let (uid, gid) = (1000, 1000);

        Ok(Self {
            session_id: None,
            // Preserve the host's actual absolute home path. macOS normally
            // uses /Users/<name>, but network homes and test accounts do not
            // have to. The shell VM and every job container must agree on the
            // same natural path.
            guest_home: natural_guest_home(&host_home),
            username,
            uid,
            gid,
            launch_directory,
            home_backing,
            ephemeral_home,
        })
    }

    /// Allocate a blank owner-only home from the fixed private slot pool. The
    /// caller must retain the lease through the complete session and explicitly
    /// release it only after actual daemon/VM cleanup.
    ///
    /// # Errors
    /// Returns an error when the temporary directory cannot be created.
    pub fn activate_ephemeral_home(
        &mut self,
    ) -> Result<EphemeralHomeGuard, Box<dyn std::error::Error + Send + Sync>> {
        let stock = crate::client::resolve_stock_sbx()?;
        let adapter = marsh_sbx::StockSbx::new(
            stock,
            std::sync::Arc::new(marsh_runtime::SystemCommandRunner::new(&self.guest_home)),
        );
        self.activate_ephemeral_home_with(adapter)
    }

    /// Same native private-slot transaction with an explicit host adapter. This
    /// is also the isolated owned caller seam, never a temporary-path bypass.
    ///
    /// # Errors
    /// Reports actual bounded-pool capacity and source/owner closure diagnostics.
    pub fn activate_ephemeral_home_with(
        &mut self,
        adapter: marsh_sbx::StockSbx,
    ) -> Result<EphemeralHomeGuard, Box<dyn std::error::Error + Send + Sync>> {
        if !self.ephemeral_home {
            return Err(Box::new(SessionError(
                "cannot activate an ephemeral home for a persistent session",
            )));
        }
        let lease = adapter.allocate_ephemeral_home()?;
        lease.path().clone_into(&mut self.home_backing);
        Ok(EphemeralHomeGuard { lease, adapter })
    }

    /// Apply the host-approved backing path carried into the trusted guest
    /// shell. This path is used only in daemon requests; the guest does not
    /// own its lifetime.
    ///
    /// # Errors
    /// Rejects relative internal paths.
    pub fn apply_ephemeral_backing(&mut self, path: PathBuf) -> Result<(), SessionError> {
        if !self.ephemeral_home {
            return Err(SessionError(
                "ephemeral home backing requires --ephemeral-home",
            ));
        }
        if !path.is_absolute() {
            return Err(SessionError("ephemeral home backing must be absolute"));
        }
        self.home_backing = path;
        Ok(())
    }
}

fn natural_guest_home(host_home: &std::path::Path) -> PathBuf {
    host_home.to_path_buf()
}

fn validate_username(username: &str) -> Result<(), SessionError> {
    if username.is_empty()
        || username.len() > 255
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(SessionError("host username is invalid"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(ephemeral_home: bool, home_backing: PathBuf) -> SessionConfig {
        SessionConfig {
            session_id: None,
            username: "user".into(),
            uid: 501,
            gid: 20,
            launch_directory: PathBuf::from("/Users/user/project"),
            guest_home: PathBuf::from("/Users/user"),
            home_backing,
            ephemeral_home,
        }
    }

    #[test]
    fn username_rejects_path_syntax() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("../root").is_err());
        assert!(validate_username("bad/name").is_err());
    }

    #[test]
    fn guest_home_preserves_a_nonstandard_host_home_path() {
        assert_eq!(
            natural_guest_home(std::path::Path::new("/Volumes/network-homes/user")),
            PathBuf::from("/Volumes/network-homes/user")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ephemeral_home_is_blank_private_and_removed_with_guard() {
        use std::os::unix::fs::PermissionsExt;

        let persistent = tempfile::tempdir().unwrap();
        std::fs::write(persistent.path().join("persistent"), "keep").unwrap();
        let mut config = session(true, persistent.path().to_path_buf());

        let registry_root = tempfile::tempdir().unwrap();
        let registry = registry_root
            .path()
            .canonicalize()
            .unwrap()
            .join("registry");
        let adapter = marsh_sbx::StockSbx::new(
            "/unused",
            std::sync::Arc::new(marsh_runtime::SystemCommandRunner::new("/nonexistent")),
        )
        .with_ephemeral_root_for_test(registry.clone());
        let guard = config.activate_ephemeral_home_with(adapter).unwrap();
        let temporary = guard.path().to_path_buf();
        assert_eq!(config.home_backing, temporary.canonicalize().unwrap());
        assert_ne!(config.home_backing, persistent.path());
        assert_eq!(std::fs::read_dir(&temporary).unwrap().count(), 0);
        assert_eq!(
            std::fs::metadata(&temporary).unwrap().permissions().mode() & 0o777,
            0o700
        );

        std::fs::write(temporary.join("guest-write"), "discard").unwrap();
        guard.release().unwrap();
        drop(guard);
        assert!(!temporary.exists());
        assert_eq!(
            std::fs::read_to_string(persistent.path().join("persistent")).unwrap(),
            "keep"
        );
        assert!(!persistent.path().join("guest-write").exists());
    }

    #[test]
    fn internal_ephemeral_backing_must_be_absolute() {
        let mut config = session(true, PathBuf::from("/persistent"));
        assert!(
            config
                .apply_ephemeral_backing(PathBuf::from("relative"))
                .is_err()
        );
        config
            .apply_ephemeral_backing(PathBuf::from("/private/tmp/session"))
            .unwrap();
        assert_eq!(config.home_backing, PathBuf::from("/private/tmp/session"));
    }
}
