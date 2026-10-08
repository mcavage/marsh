//! Guest relay process identity, reported in-band at relay startup. Cleanup
//! proof is the relay's in-band `Cleaned` frame plus its supervisor-reported exit.
use serde::{Deserialize, Serialize};
use std::{fs, io};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelayIdentity {
    pub pid: u32,
    pub start_time: u64,
    pub boot_id: String,
}

pub fn current_identity() -> io::Result<Option<RelayIdentity>> {
    if !cfg!(target_os = "linux") {
        return Ok(None);
    }
    let pid = std::process::id();
    let (start_time, _) = process_identity(pid)?;
    Ok(Some(RelayIdentity {
        pid,
        start_time,
        boot_id: boot_id()?,
    }))
}

fn boot_id() -> io::Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

fn process_identity(pid: u32) -> io::Result<(u64, char)> {
    let contents = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, tail) = contents
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("invalid relay process stat"))?;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let state = fields
        .first()
        .and_then(|value| value.chars().next())
        .ok_or_else(|| io::Error::other("missing relay state"))?;
    let birth_ticks = fields
        .get(19)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other("missing relay start time"))?;
    Ok((birth_ticks, state))
}
