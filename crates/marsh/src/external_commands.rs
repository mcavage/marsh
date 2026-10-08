//! Make Kit registrations visible to descendants that resolve commands on PATH.

use crate::session::SessionConfig;
use marsh_contracts::command_registry::CommandName;
use std::{io, os::unix::fs::MetadataExt, path::Path};

pub const SESSION_VARIABLE: &str = "MARSH_EXTERNAL_SESSION";

/// The directory remains alive for the lifetime of the Brush shell.
pub struct ExternalCommands(tempfile::TempDir);

impl ExternalCommands {
    /// Install only validated Kit names. Existing Brush builtins and shims keep
    /// their normal precedence; these links serve descendant processes.
    ///
    /// # Errors
    /// Returns a filesystem error or rejects an invalid command name.
    pub fn install(names: &[String]) -> io::Result<Self> {
        let names = names
            .iter()
            .map(|name| checked_command_name(name))
            .collect::<io::Result<Vec<_>>>()?;
        let directory = tempfile::Builder::new()
            .prefix("marsh-commands-")
            .tempdir_in("/tmp")?;
        let executable = std::env::current_exe()?;
        for name in names {
            std::os::unix::fs::symlink(&executable, directory.path().join(name.as_str()))?;
        }
        Ok(Self(directory))
    }

    /// For a bash or zsh session, which has no Brush builtins: also link the
    /// shell commands marsh provides in-process under Brush (`acp`, `mcp`,
    /// `ps`, `top`). Invoked by name, each re-enters the guest's bundled
    /// dispatch for this session (`ps`/`top` without `--marsh` run the
    /// system's own).
    ///
    /// # Errors
    /// Returns a filesystem error.
    pub fn install_shell_builtins(&self) -> io::Result<()> {
        let executable = std::env::current_exe()?;
        for name in SHELL_BUILTINS {
            std::os::unix::fs::symlink(&executable, self.0.path().join(name))?;
        }
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

/// Brush-provided shell commands that bash/zsh sessions reach through links.
pub const SHELL_BUILTINS: [&str; 4] = ["acp", "mcp", "ps", "top"];

/// Arguments that re-enter the guest's bundled dispatch for `name` in the
/// attached session described by `context` (as Brush's process shim does).
///
/// # Errors
/// Rejects a missing or malformed session context.
pub fn bundled_arguments(
    name: &str,
    context: &std::ffi::OsStr,
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Vec<std::ffi::OsString>, &'static str> {
    let session: SessionConfig = serde_json::from_slice(context.as_encoded_bytes())
        .map_err(|_| "invalid marsh session context")?;
    let id = session.session_id.ok_or("missing marsh session identity")?;
    let mut arguments: Vec<std::ffi::OsString> = vec![
        "--invoke-bundled".into(),
        name.into(),
        "--marsh-guest".into(),
        "--marsh-session".into(),
        id.into(),
    ];
    if session.ephemeral_home {
        arguments.extend([
            "--ephemeral-home".into(),
            "--marsh-home-backing".into(),
            session.home_backing.into_os_string(),
        ]);
    }
    arguments.extend(args);
    Ok(arguments)
}

/// Serialize the already attached session for child command dispatch. The
/// relay token still authorizes every request and binds it to this session.
///
/// # Errors
/// Returns an error if the session cannot be encoded.
pub fn session_context(session: &SessionConfig) -> Result<String, serde_json::Error> {
    serde_json::to_string(session)
}

/// Verify an invoked name against the daemon's current Kit registry.
#[must_use]
pub fn registered_name(name: &str, names: &[String]) -> bool {
    CommandName::parse(name).is_ok() && names.iter().any(|registered| registered == name)
}

fn checked_command_name(name: &str) -> io::Result<CommandName> {
    CommandName::parse(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid registered command name",
        )
    })
}

/// Expose a newly installed command to the already-running project shell.
/// The command is still checked against the live daemon registry on invocation.
///
/// # Errors
/// Returns an I/O error if the active shell's command directory is missing or
/// the new command link cannot be created.
pub fn install_live_command(name: &str) -> io::Result<()> {
    let name = checked_command_name(name)?;
    if std::env::var_os(SESSION_VARIABLE).is_none() {
        return Ok(());
    }
    let path = std::env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "shell PATH is missing"))?;
    let uid = nix::unistd::geteuid().as_raw();
    let directory = std::env::split_paths(&path)
        .find(|part| {
            part.parent() == Some(Path::new("/tmp"))
                && part
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("marsh-commands-"))
                && std::fs::symlink_metadata(part).is_ok_and(|meta| {
                    meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == uid
                })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "active marsh command directory is missing",
            )
        })?;
    std::os::unix::fs::symlink(std::env::current_exe()?, directory.join(name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn links_and_live_lookup_share_the_authoritative_command_name_policy() {
        for name in [
            "", ".", "..", "a.b", "-option", "a/b", "marsh", "acp", "top", "é",
        ] {
            assert_eq!(
                ExternalCommands::install(&[name.into()])
                    .err()
                    .unwrap()
                    .kind(),
                io::ErrorKind::InvalidInput,
            );
            assert_eq!(
                install_live_command(name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(!registered_name(name, &[name.into()]));
        }
        assert!(registered_name("valid-name_2", &["valid-name_2".into()]));
        assert!(!registered_name("valid-name_2", &["other".into()]));
    }

    #[test]
    fn descendants_resolve_installed_names_without_touching_other_path_entries() {
        let links = ExternalCommands::install(&["fixture".into()]).unwrap();
        let path = format!("{}:/usr/bin:/bin", links.path().display());
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("command -v fixture")
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            links.path().join("fixture").to_str().unwrap()
        );
        assert_eq!(
            fs::read_link(links.path().join("fixture")).unwrap(),
            std::env::current_exe().unwrap()
        );
    }
}
