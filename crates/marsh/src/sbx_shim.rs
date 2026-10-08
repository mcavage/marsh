//! Guest `sbx` shim for `marsh --dev` sessions.
//!
//! The host copies the outer build's guest `marsh` to `$MARSH_DEV_SCRATCH/
//! tmp/bin/sbx`, so the host owns this protocol and a candidate cannot break
//! its own broker hop. Invoked as `sbx`, it forwards its argv over the
//! session relay as one `DevSbx` call and relays stdio. The relay location
//! comes from the sidecar `sbx-relay.json` beside the executable (daemons
//! run it with a cleared environment), else from the shell's relay variables.

use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub fn invoked_as_sbx(arguments: &[OsString]) -> bool {
    arguments
        .first()
        .and_then(|argv0| Path::new(argv0).file_name())
        .is_some_and(|name| name == "sbx")
}

fn relay_paths() -> Option<(PathBuf, PathBuf)> {
    let sidecar = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("sbx-relay.json")));
    if let Some(bytes) = sidecar.and_then(|path| std::fs::read(path).ok())
        && let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let (Some(socket), Some(token)) = (
            document.get("socket").and_then(serde_json::Value::as_str),
            document.get("token").and_then(serde_json::Value::as_str),
        )
    {
        return Some((socket.into(), token.into()));
    }
    Some((
        env::var_os("MARSH_DAEMON_SOCKET")?.into(),
        env::var_os("MARSH_DAEMON_TOKEN")?.into(),
    ))
}

/// The caller's terminal size when it has one: the host gives a PTY to the
/// calls that take one (`exec -t`, an attached `run`) and the shim goes raw.
fn caller_terminal() -> Option<marsh_contracts::TerminalSize> {
    use std::io::IsTerminal as _;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return None;
    }
    let size = rustix::termios::tcgetwinsize(std::io::stdin()).ok()?;
    Some(if size.ws_row == 0 || size.ws_col == 0 {
        marsh_contracts::TerminalSize {
            rows: 24,
            columns: 80,
        }
    } else {
        marsh_contracts::TerminalSize {
            rows: size.ws_row,
            columns: size.ws_col,
        }
    })
}

pub fn run(arguments: &[OsString]) -> i32 {
    let Some(argv) = arguments[1..]
        .iter()
        .map(|word| word.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    else {
        eprintln!("sbx: dev broker arguments must be UTF-8");
        return 2;
    };
    let Some((socket, token)) = relay_paths() else {
        eprintln!("sbx: this is the marsh --dev broker shim; no session relay is available");
        return 125;
    };
    match marsh_daemon::Client::connect_relay(&socket, &token)
        .and_then(|client| client.dev_sbx(argv, caller_terminal(), env::current_dir().ok()))
    {
        Ok(code) => code,
        Err(error) => {
            eprintln!("sbx: dev broker: {error}");
            125
        }
    }
}
