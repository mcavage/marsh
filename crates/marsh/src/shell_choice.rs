//! Which interactive shell a session runs (`docs/shells.md`).
//!
//! The default is marsh's Brush. A user may instead choose the shell VM's own
//! `bash` or `zsh`. The guest marsh still attaches the session, installs the
//! registered-command links, and owns the containment record; it then runs
//! the chosen shell as its child with the links first on `PATH`, and a small
//! generated startup layer that sources the user's own startup files from the
//! selected home and re-pins `PATH` afterwards.

use std::{
    ffi::OsString,
    fmt, io,
    os::unix::{
        fs::{OpenOptionsExt as _, PermissionsExt as _},
        process::{CommandExt as _, ExitStatusExt as _},
    },
    path::{Path, PathBuf},
    process::Command,
};

/// Environment variable that selects the shell for one launch.
pub const SHELL_VARIABLE: &str = "MARSH_SHELL";
/// Exported to the chosen shell: the session's registered-command directory.
pub const BIN_VARIABLE: &str = "MARSH_SESSION_BIN";
/// Per-home default, beside (never inside) the guest home.
pub const CONFIG_FILE: &str = "config.json";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShellChoice {
    #[default]
    Marsh,
    Bash,
    Zsh,
}

impl ShellChoice {
    /// Parses `marsh`, `brush`, `bash`, or `zsh` (an absolute path's file name is accepted).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let name = value.rsplit('/').next().unwrap_or(value);
        match name {
            "marsh" | "msh" | "brush" => Some(Self::Marsh),
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Marsh => "marsh",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
        }
    }
}

impl fmt::Display for ShellChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

fn invalid(source: &str, value: &str) -> String {
    format!("{source}: unknown shell {value:?} (choose marsh, bash, or zsh)")
}

/// The configured per-home default, if any.
///
/// # Errors
/// Reports an unreadable or malformed config file.
pub fn configured(scope_root: &Path) -> Result<Option<ShellChoice>, String> {
    let path = scope_root.join(CONFIG_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    match value.get("shell") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(name)) => ShellChoice::parse(name)
            .map(Some)
            .ok_or_else(|| invalid(&path.display().to_string(), name)),
        Some(_) => Err(format!(
            "invalid {}: shell must be a string",
            path.display()
        )),
    }
}

/// `--shell` beats `MARSH_SHELL`, which beats the per-home default; else marsh.
///
/// # Errors
/// Rejects an unknown `MARSH_SHELL` value or a malformed config file.
pub fn resolve(
    flag: Option<ShellChoice>,
    environment: Option<&str>,
    scope_root: &Path,
) -> Result<ShellChoice, String> {
    if let Some(choice) = flag {
        return Ok(choice);
    }
    if let Some(value) = environment.filter(|value| !value.is_empty()) {
        return ShellChoice::parse(value).ok_or_else(|| invalid(SHELL_VARIABLE, value));
    }
    Ok(configured(scope_root)?.unwrap_or_default())
}

pub const CONFIG_HELP: &str = "Usage: marsh config shell [marsh|bash|zsh]\n\nShow or set this home's default interactive shell. `marsh` (the default) is\nmarsh's Brush; `bash` and `zsh` are the shell VM's own, started with your\n~/.bashrc or ~/.zshrc from the selected home. One launch may override it with\n--shell NAME or MARSH_SHELL=NAME. Stored in $MARSH_HOME/config.json.\nRun a script named `config` as `marsh ./config`.\n";

/// `marsh config ...` on the host.
///
/// # Errors
/// Returns a message and exit status for usage or filesystem failures.
pub fn config_command(arguments: &[OsString], scope_root: &Path) -> Result<i32, (String, i32)> {
    let words = arguments
        .iter()
        .map(|argument| argument.to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| ("config arguments must be UTF-8".to_owned(), 2))?;
    match words.as_slice() {
        [] | ["-h" | "--help"] | ["shell", "-h" | "--help"] => {
            print!("{CONFIG_HELP}");
            Ok(0)
        }
        ["shell"] => {
            let choice = configured(scope_root).map_err(|message| (message, 1))?;
            println!("{}", choice.unwrap_or_default());
            Ok(0)
        }
        ["shell", name] => {
            let choice =
                ShellChoice::parse(name).ok_or_else(|| (invalid("marsh config shell", name), 2))?;
            write_config(scope_root, choice).map_err(|error| {
                (
                    format!(
                        "cannot write {}: {error}",
                        scope_root.join(CONFIG_FILE).display()
                    ),
                    1,
                )
            })?;
            println!("{choice}");
            Ok(0)
        }
        _ => Err(("usage: marsh config shell [marsh|bash|zsh]".to_owned(), 2)),
    }
}

fn write_config(scope_root: &Path, choice: ShellChoice) -> io::Result<()> {
    if !scope_root.exists() {
        std::fs::create_dir_all(scope_root)?;
        std::fs::set_permissions(scope_root, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = scope_root.join(CONFIG_FILE);
    let mut document = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(error),
    };
    document.insert("shell".into(), choice.name().into());
    let mut encoded = serde_json::to_vec_pretty(&document)?;
    encoded.push(b'\n');
    let temporary = scope_root.join(format!(".{CONFIG_FILE}.{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        io::Write::write_all(&mut file, &encoded)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

// ---- guest launch ---------------------------------------------------------

const BANNER: &str = "# Generated by marsh for one session; edit your own startup files instead.\n";

/// Move `$MARSH_SESSION_BIN` to the front of `PATH` (bash; literal matching).
const BASH_PIN: &str = r#"if [ -n "${MARSH_SESSION_BIN-}" ]; then
  __marsh_rest=":$PATH:"
  while [[ $__marsh_rest == *":$MARSH_SESSION_BIN:"* ]]; do
    __marsh_rest=${__marsh_rest/":$MARSH_SESSION_BIN:"/:}
  done
  __marsh_rest=${__marsh_rest#:}
  __marsh_rest=${__marsh_rest%:}
  PATH="$MARSH_SESSION_BIN${__marsh_rest:+:$__marsh_rest}"
  export PATH
  unset __marsh_rest
fi
"#;

const ZSH_PIN: &str = r#"if [[ -n ${MARSH_SESSION_BIN-} ]]; then
  path=("$MARSH_SESSION_BIN" ${path:#$MARSH_SESSION_BIN})
  export PATH
fi
"#;

/// Startup files written into the session's private rc directory.
#[must_use]
pub fn startup_files(choice: ShellChoice) -> Vec<(&'static str, String)> {
    match choice {
        ShellChoice::Marsh => Vec::new(),
        // `bash --rcfile` replaces ~/.bashrc for interactive shells: source the
        // user's file, then re-pin PATH. Noninteractive bash reads nothing and
        // inherits the pinned PATH from the environment.
        ShellChoice::Bash => vec![(
            "bashrc",
            format!(
                "{BANNER}if [ -f \"$HOME/.bashrc\" ]; then\n  . \"$HOME/.bashrc\"\nfi\n{BASH_PIN}"
            ),
        )],
        // zsh reads each startup file from the ZDOTDIR current at that moment.
        // `.zshenv` sources the user's, then hands ZDOTDIR back unless the
        // shell is interactive; for interactive shells `.zprofile` and `.zshrc`
        // delegate too and `.zshrc` hands ZDOTDIR back (so `.zlogin` and
        // `.zlogout` are read from the user's directory directly).
        ShellChoice::Zsh => vec![
            (
                ".zshenv",
                format!(
                    "{BANNER}__marsh_shim=$ZDOTDIR\nZDOTDIR=${{MARSH_USER_ZDOTDIR:-$HOME}}\nif [[ -f $ZDOTDIR/.zshenv ]]; then\n  source $ZDOTDIR/.zshenv\nfi\nexport MARSH_USER_ZDOTDIR=$ZDOTDIR\nif [[ -o interactive ]]; then\n  ZDOTDIR=$__marsh_shim\nfi\nunset __marsh_shim\n{ZSH_PIN}"
                ),
            ),
            (
                ".zprofile",
                format!(
                    "{BANNER}__marsh_shim=$ZDOTDIR\nZDOTDIR=$MARSH_USER_ZDOTDIR\nif [[ -f $ZDOTDIR/.zprofile ]]; then\n  source $ZDOTDIR/.zprofile\nfi\nexport MARSH_USER_ZDOTDIR=$ZDOTDIR\nZDOTDIR=$__marsh_shim\nunset __marsh_shim\n"
                ),
            ),
            (
                ".zshrc",
                format!(
                    "{BANNER}ZDOTDIR=$MARSH_USER_ZDOTDIR\nif [[ -f $ZDOTDIR/.zshrc ]]; then\n  source $ZDOTDIR/.zshrc\nfi\n{ZSH_PIN}"
                ),
            ),
        ],
    }
}

/// Find the shell VM's own `bash`/`zsh`, never a session link.
fn locate(choice: ShellChoice, bin: &Path) -> Option<PathBuf> {
    let search = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    std::env::split_paths(&search)
        .chain([PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
        .filter(|directory| directory != bin && directory.is_absolute())
        .map(|directory| directory.join(choice.name()))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

fn pinned_path(bin: &Path) -> OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let rest = std::env::split_paths(&current).filter(|entry| entry != bin);
    std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(rest))
        .unwrap_or_else(|_| bin.as_os_str().to_owned())
}

/// Status as a shell reports it: the exit code, or 128 + the signal number.
fn status_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Run the chosen shell as this process's child and return its status.
///
/// `arguments` are the user's shell arguments (after argv[0]), passed through
/// unchanged. The caller keeps `bin` alive until this returns.
///
/// # Errors
/// Fails if the shell is not installed or cannot be started.
pub fn run_user_shell(
    choice: ShellChoice,
    arguments: &[OsString],
    bin: &Path,
    session_context: &str,
) -> Result<i32, (String, i32)> {
    let program = locate(choice, bin).ok_or_else(|| {
        (
            format!(
                "{choice} is not installed in this shell VM image; rebuild the shell image or choose another --shell"
            ),
            127,
        )
    })?;
    let rc = tempfile::Builder::new()
        .prefix("marsh-shellrc-")
        .tempdir_in("/tmp")
        .map_err(|error| (format!("cannot create shell startup files: {error}"), 1))?;
    for (name, contents) in startup_files(choice) {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(rc.path().join(name))
            .and_then(|mut file| io::Write::write_all(&mut file, contents.as_bytes()))
            .map_err(|error| (format!("cannot write shell startup files: {error}"), 1))?;
    }
    let mut command = Command::new(&program);
    command.arg0(choice.name());
    command
        .env("PATH", pinned_path(bin))
        .env(crate::external_commands::SESSION_VARIABLE, session_context)
        .env(BIN_VARIABLE, bin)
        .env(SHELL_VARIABLE, choice.name())
        .env("SHELL", &program)
        .env_remove("BASH_ENV")
        .env_remove("ENV");
    match choice {
        ShellChoice::Bash => {
            command.arg("--rcfile").arg(rc.path().join("bashrc"));
        }
        ShellChoice::Zsh => {
            let user = std::env::var_os("ZDOTDIR")
                .filter(|value| !value.is_empty())
                .or_else(|| std::env::var_os("HOME"))
                .unwrap_or_default();
            command
                .env("ZDOTDIR", rc.path())
                .env("MARSH_USER_ZDOTDIR", user);
        }
        ShellChoice::Marsh => unreachable!("Brush runs in-process"),
    }
    command.args(arguments);
    let status = spawn_and_wait(command)
        .map_err(|error| (format!("cannot start {}: {error}", program.display()), 126))?;
    drop(rc);
    Ok(status_code(status))
}

/// Like system(3): while the shell runs, terminal job-control signals sent to
/// this parent are caught and dropped (the shell owns the terminal). Caught,
/// not ignored: exec resets caught signals, so the shell starts with default
/// dispositions.
fn spawn_and_wait(mut command: Command) -> io::Result<std::process::ExitStatus> {
    use signal_hook::consts::{SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU};
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut registrations = Vec::new();
    for held in [SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU] {
        registrations.push(signal_hook::flag::register(
            held,
            std::sync::Arc::clone(&dropped),
        )?);
    }
    let result = command.spawn().and_then(|mut child| child.wait());
    for registration in registrations {
        signal_hook::low_level::unregister(registration);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_precedence_and_config_roundtrip() {
        let scope = tempfile::tempdir().unwrap();
        let root = scope.path().join("scope");
        assert_eq!(resolve(None, None, &root), Ok(ShellChoice::Marsh));
        assert_eq!(
            config_command(&["shell".into(), "zsh".into()], &root),
            Ok(0)
        );
        assert_eq!(
            std::fs::metadata(root.join(CONFIG_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(resolve(None, None, &root), Ok(ShellChoice::Zsh));
        assert_eq!(resolve(None, Some("bash"), &root), Ok(ShellChoice::Bash));
        assert_eq!(
            resolve(Some(ShellChoice::Marsh), Some("bash"), &root),
            Ok(ShellChoice::Marsh)
        );
        assert!(resolve(None, Some("fish"), &root).is_err());
        assert_eq!(
            config_command(&["shell".into(), "fish".into()], &root)
                .unwrap_err()
                .1,
            2
        );
        std::fs::write(root.join(CONFIG_FILE), b"{\"shell\":3}").unwrap();
        assert!(resolve(None, None, &root).is_err());
    }

    fn parses(shell: &str, flags: &[&str], contents: &str) -> bool {
        let Some(path) = ["/bin", "/usr/bin", "/opt/homebrew/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(shell))
            .find(|path| path.exists())
        else {
            return true;
        };
        let mut child = Command::new(path)
            .args(flags)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        io::Write::write_all(child.stdin.as_mut().unwrap(), contents.as_bytes()).unwrap();
        drop(child.stdin.take());
        child.wait().unwrap().success()
    }

    #[test]
    fn generated_startup_files_parse_in_their_shells() {
        for (name, contents) in startup_files(ShellChoice::Bash) {
            assert!(parses("bash", &["-n"], &contents), "{name}");
        }
        for (name, contents) in startup_files(ShellChoice::Zsh) {
            assert!(parses("zsh", &["-f", "-n"], &contents), "{name}");
        }
    }

    #[test]
    fn zsh_layer_honors_user_files_and_keeps_session_bin_first() {
        let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
            .iter()
            .map(PathBuf::from)
            .find(|path| path.exists())
        else {
            return;
        };
        let home = tempfile::tempdir().unwrap();
        let shim = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".zshrc"),
            "alias hi='echo hi-from-zshrc'\nexport PATH=/user/first:$PATH\n",
        )
        .unwrap();
        std::fs::write(home.path().join(".zshenv"), "export FROM_ZSHENV=1\n").unwrap();
        for (name, contents) in startup_files(ShellChoice::Zsh) {
            std::fs::write(shim.path().join(name), contents).unwrap();
        }
        let path = format!("{}:/usr/bin:/bin", bin.path().display());
        let run = |args: &[&str]| {
            Command::new(&zsh)
                .args(args)
                .env_clear()
                .env("HOME", home.path())
                .env("PATH", &path)
                .env("ZDOTDIR", shim.path())
                .env("MARSH_USER_ZDOTDIR", home.path())
                .env(BIN_VARIABLE, bin.path())
                .output()
                .unwrap()
        };
        let interactive = run(&["-i", "-c", "hi; echo ${path[1]}; echo $ZDOTDIR"]);
        let stdout = String::from_utf8_lossy(&interactive.stdout);
        let lines: Vec<_> = stdout.lines().collect();
        assert_eq!(
            lines,
            [
                "hi-from-zshrc",
                bin.path().to_str().unwrap(),
                home.path().to_str().unwrap()
            ],
            "{}",
            String::from_utf8_lossy(&interactive.stderr)
        );
        let plain = run(&["-c", "echo $FROM_ZSHENV $ZDOTDIR"]);
        assert_eq!(
            String::from_utf8_lossy(&plain.stdout).trim(),
            format!("1 {}", home.path().display())
        );
    }

    #[test]
    fn bash_layer_honors_user_bashrc_and_keeps_session_bin_first() {
        let bash = PathBuf::from("/bin/bash");
        if !bash.exists() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let rc = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".bashrc"),
            "alias hi='echo hi-from-bashrc'\nexport PATH=/user/first:$PATH\n",
        )
        .unwrap();
        for (name, contents) in startup_files(ShellChoice::Bash) {
            std::fs::write(rc.path().join(name), contents).unwrap();
        }
        let output = Command::new(&bash)
            .arg("--rcfile")
            .arg(rc.path().join("bashrc"))
            .args(["-i", "-c", "hi; echo ${PATH%%:*}"])
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
            .env(BIN_VARIABLE, bin.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.lines().collect::<Vec<_>>(),
            ["hi-from-bashrc", bin.path().to_str().unwrap()],
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
