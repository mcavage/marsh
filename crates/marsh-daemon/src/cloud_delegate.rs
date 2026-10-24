//! `marsh config cloud-delegate on` (`docs/design/cloud-detach.md` s6): a
//! detached cloud run may start agent sandboxes itself, signed in with a
//! Docker personal access token the user keeps in the macOS Keychain. Off
//! by default. The token is read at a run's start and travels only on the
//! stdin of one root-only helper step; it is never serialized into a
//! request, a receipt, a record, a log, or an argument list.

use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};

/// `cloud_delegate` in `$MARSH_HOME/config.json`.
pub const CONFIG_KEY: &str = "cloud_delegate";
/// The Keychain generic password's service; its account is the Docker ID.
pub const KEYCHAIN_SERVICE: &str = "marsh-cloud-delegate";
/// Agent sandboxes one detached run may start, unless `MARSH_CLOUD_CHILDREN`.
pub const DEFAULT_CHILDREN: u32 = 8;
const MAX_TOKEN: usize = 4096;

/// Whether delegation is on in a scope's `config.json` (missing is off).
#[must_use]
pub fn configured(config: &Path) -> bool {
    std::fs::read(config)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get(CONFIG_KEY).and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

/// The cap on agent sandboxes per detached run: `MARSH_CLOUD_CHILDREN` in
/// the daemon's environment (1 to 64), else [`DEFAULT_CHILDREN`].
#[must_use]
pub fn max_children() -> u32 {
    std::env::var("MARSH_CLOUD_CHILDREN")
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|count| (1..=64).contains(count))
        .unwrap_or(DEFAULT_CHILDREN)
}

/// The exact command that stores the token.
#[must_use]
pub fn keychain_command(account: Option<&str>) -> String {
    format!(
        "security add-generic-password -s {KEYCHAIN_SERVICE} -a {} -w",
        account.unwrap_or("YOUR_DOCKER_ID")
    )
}

/// Why a detached block with an agent call is refused while delegation is off.
#[must_use]
pub fn off_message(name: &str) -> String {
    format!(
        "cloud: this detached block calls {name}, a registered Kit. A detached run can start agent sandboxes only with \
         `marsh config cloud-delegate on` and a Docker access token with \"Allow Cloud Sandboxes access\" in your Keychain \
         ({}). Or run that call on its own as `cloud -d {{ {name} ...; }}`, or the block without -d",
        keychain_command(None)
    )
}

/// A Docker ID and its access token. Not `Serialize`; `Debug` omits the
/// token, and its bytes are overwritten on drop.
pub struct DelegateToken {
    pub account: String,
    secret: Vec<u8>,
}

impl fmt::Debug for DelegateToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DelegateToken")
            .field("account", &self.account)
            .field("secret", &"[redacted]")
            .finish()
    }
}

impl Drop for DelegateToken {
    fn drop(&mut self) {
        self.secret.fill(0);
    }
}

impl DelegateToken {
    /// # Errors
    /// Rejects an implausible Docker ID or token.
    pub fn new(account: &str, secret: Vec<u8>) -> Result<Self, String> {
        let mut secret = secret;
        while secret.last().is_some_and(u8::is_ascii_whitespace) {
            secret.pop();
        }
        let valid_account = !account.is_empty()
            && account.len() <= 64
            && account
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !valid_account {
            secret.fill(0);
            return Err(format!(
                "the Keychain item's account must be your Docker ID ({})",
                keychain_command(None)
            ));
        }
        if secret.is_empty() || secret.len() > MAX_TOKEN || !secret.iter().all(u8::is_ascii_graphic)
        {
            secret.fill(0);
            return Err(format!(
                "the Keychain item for {account} does not hold a Docker access token ({})",
                keychain_command(Some(account))
            ));
        }
        Ok(Self {
            account: account.to_owned(),
            secret,
        })
    }

    /// The helper step's stdin: `ID\nTOKEN\n`. The caller overwrites it
    /// after use.
    #[must_use]
    pub fn upload_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.account.len() + self.secret.len() + 2);
        bytes.extend_from_slice(self.account.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(&self.secret);
        bytes.push(b'\n');
        bytes
    }
}

/// The account (`"acct"<blob>="..."`) of a `security find-generic-password`
/// listing.
fn listed_account(listing: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("\"acct\"<blob>=\"")?;
        Some(rest.strip_suffix('"')?.to_owned())
    })
}

/// Read the token from the login Keychain with the stock `security` tool:
/// the item's account, then its password (`-w`), never on a command line.
///
/// # Errors
/// The item is missing (the message names the command that stores it) or
/// unreadable.
pub fn read_keychain() -> Result<DelegateToken, String> {
    let missing = || {
        format!(
            "cloud-delegate is on, but no access token is in your Keychain. Create a Docker personal access token with \
             \"Allow Cloud Sandboxes access\", then store it: {}",
            keychain_command(None)
        )
    };
    let listing = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("cloud-delegate: cannot run security: {error}"))?;
    if !listing.status.success() {
        return Err(missing());
    }
    let account = listed_account(&String::from_utf8_lossy(&listing.stdout)).ok_or_else(missing)?;
    let secret = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &account,
            "-w",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("cloud-delegate: cannot run security: {error}"))?;
    if !secret.status.success() {
        return Err(format!(
            "cloud-delegate: the Keychain did not release the token for {account} ({})",
            keychain_command(Some(&account))
        ));
    }
    DelegateToken::new(&account, secret.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_is_off_unless_turned_on() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        assert!(!configured(&config));
        std::fs::write(&config, r#"{"open_urls":true}"#).unwrap();
        assert!(!configured(&config));
        std::fs::write(&config, r#"{"cloud_delegate":true}"#).unwrap();
        assert!(configured(&config));
    }

    #[test]
    fn the_token_never_appears_in_debug_and_the_account_is_parsed() {
        let token = DelegateToken::new("markc741", b"dckr_pat_SECRET123\n".to_vec()).unwrap();
        let shown = format!("{token:?}");
        assert!(
            shown.contains("markc741") && !shown.contains("SECRET123"),
            "{shown}"
        );
        assert_eq!(token.upload_bytes(), b"markc741\ndckr_pat_SECRET123\n");
        assert!(DelegateToken::new("bad id", b"x".to_vec()).is_err());
        assert!(DelegateToken::new("id", b"tok en".to_vec()).is_err());
        assert_eq!(
            listed_account(
                "keychain: \"/x\"\nattributes:\n    \"acct\"<blob>=\"markcavage741\"\n    \"svce\"<blob>=\"marsh-cloud-delegate\"\n"
            )
            .as_deref(),
            Some("markcavage741")
        );
    }

    #[test]
    fn refusals_name_the_switch_and_the_keychain_command() {
        let message = off_message("codex");
        assert!(
            message.contains("marsh config cloud-delegate on"),
            "{message}"
        );
        assert!(
            message.contains(
                "security add-generic-password -s marsh-cloud-delegate -a YOUR_DOCKER_ID -w"
            ),
            "{message}"
        );
        assert_eq!(
            keychain_command(Some("markcavage741")),
            "security add-generic-password -s marsh-cloud-delegate -a markcavage741 -w"
        );
    }
}
