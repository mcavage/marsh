//! Explicit host-only recovery of one retained private ephemeral slot.
use crate::{RunError, protocol_arguments};
use std::{env, path::PathBuf, sync::Arc};

pub(super) fn run(arguments: &[std::ffi::OsString]) -> Result<i32, RunError> {
    let args = protocol_arguments(arguments)?;
    if args.len() != 2 || args[1] != "--discard" {
        return Err(RunError::new(
            "usage: marsh recover-home SLOT --discard; use the SAME stock SDK selection as the owning session",
            2,
        ));
    }
    let slot = args[0]
        .parse::<u8>()
        .ok()
        .filter(|slot| *slot < 16)
        .ok_or_else(|| RunError::new("ephemeral slot must be 0..15", 2))?;
    if [
        "MARSH_DAEMON_SOCKET",
        "MARSH_DAEMON_TOKEN",
        "MARSH_DEV_UPSTREAM_CONFIG",
    ]
    .iter()
    .any(|key| env::var_os(key).is_some())
    {
        return Err(RunError::new(
            "retained ephemeral home recovery is host-only, never delegated through a guest relay",
            125,
        ));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| RunError::new("host HOME must be absolute", 2))?;
    let stock =
        marsh::client::resolve_stock_sbx().map_err(|e| RunError::new(e.to_string(), 125))?;
    let adapter = marsh_sbx::StockSbx::new(
        stock,
        Arc::new(marsh_runtime::SystemCommandRunner::new(home)),
    );
    match adapter.recover_ephemeral_home(slot) {
        Ok(path) => {
            println!(
                "{}",
                serde_json::json!({"slot":slot,"released":path.is_some(),"retained":false,"path":path})
            );
            Ok(0)
        }
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({"slot":slot,"released":false,"retained":true,"error":error.to_string()})
            );
            Err(RunError::new(
                format!(
                    "ephemeral slot retained: {error}; no VM was removed; finish exact owner cleanup using the same SDK control domain before retrying"
                ),
                125,
            ))
        }
    }
}
