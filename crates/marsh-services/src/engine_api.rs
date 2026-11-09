//! A minimal Docker Engine API client for the services agent's own use.
//!
//! This talks to the **production** engine's socket (inside the engine
//! container) and is never exposed to a user: user requests go through the
//! policy (E3), not through here. Requests are HTTP/1.0 with `Connection:
//! close`, so response bodies end at EOF and need no chunk parsing; a chunked
//! reply is decoded anyway in case an engine answers 1.0 with it.
//!
//! Everything taken from a response and put into a request path (container
//! and image ids) is validated first; names and labels never become paths.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// The label the agent stamps on every object it admits (A4).
pub const SCOPE_LABEL: &str = "dev.marsh.scope";
pub const ATTEMPT_LABEL: &str = "dev.marsh.attempt";

#[derive(Debug)]
pub enum ApiError {
    Io(std::io::Error),
    Status { code: u16, body: String },
    Protocol(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "engine socket: {error}"),
            Self::Status { code, body } => write!(f, "engine answered {code}: {body}"),
            Self::Protocol(why) => write!(f, "engine protocol: {why}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// The object does not exist (any more).
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Status { code: 404, .. })
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug)]
pub struct Engine {
    socket: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MountSummary {
    #[serde(default, rename = "Type")]
    pub kind: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PortSummary {
    #[serde(default)]
    pub public_port: u16,
    #[serde(default, rename = "Type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerSummary {
    pub id: String,
    #[serde(default)]
    pub names: Option<Vec<String>>,
    #[serde(default)]
    pub image: String,
    #[serde(default, rename = "ImageID")]
    pub image_id: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub mounts: Option<Vec<MountSummary>>,
    #[serde(default)]
    pub ports: Option<Vec<PortSummary>>,
}

impl ContainerSummary {
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .as_ref()
            .and_then(|labels| labels.get(key))
            .map(String::as_str)
    }

    #[must_use]
    pub fn display_name(&self) -> String {
        self.names
            .as_ref()
            .and_then(|names| names.first())
            .map(|name| name.trim_start_matches('/').to_owned())
            .unwrap_or_default()
    }
}

/// What a container inspect says about who may dial it: its state, its scope
/// label and the TCP ports it publishes.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerDetail {
    #[serde(default)]
    state: DetailState,
    #[serde(default)]
    config: DetailConfig,
    #[serde(default)]
    network_settings: DetailNetwork,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DetailState {
    #[serde(default)]
    status: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DetailConfig {
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DetailNetwork {
    /// `"5432/tcp"` to its host bindings (null for an exposed, unpublished port).
    #[serde(default)]
    ports: Option<BTreeMap<String, Option<Vec<DetailBinding>>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DetailBinding {
    #[serde(default)]
    host_port: String,
}

impl ContainerDetail {
    /// Running (the engine's own status word: a paused or restarting container
    /// is not), as the container list says.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state.status == "running"
    }

    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.config
            .labels
            .as_ref()
            .and_then(|labels| labels.get(key))
            .map(String::as_str)
    }

    /// Does the container publish this TCP port on the host side?
    #[must_use]
    pub fn publishes_tcp(&self, port: u16) -> bool {
        self.network_settings
            .ports
            .iter()
            .flatten()
            .filter(|(key, _)| key.ends_with("/tcp"))
            .flat_map(|(_, bindings)| bindings.iter().flatten())
            .any(|binding| binding.host_port.parse::<u16>().is_ok_and(|p| p == port))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VolumeSummary {
    pub name: String,
    #[serde(default)]
    pub driver: String,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub options: Option<BTreeMap<String, String>>,
}

impl VolumeSummary {
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .as_ref()
            .and_then(|labels| labels.get(key))
            .map(String::as_str)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct VolumeList {
    volumes: Option<Vec<VolumeSummary>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageInspect {
    #[serde(default)]
    repo_digests: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerInspect {
    state: ContainerState,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerState {
    #[serde(default)]
    paused: bool,
}

/// A container or image id as the engine reports them: hex, with an optional
/// `sha256:` prefix for images. Only such strings are ever put in a path.
#[must_use]
pub fn valid_container_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[must_use]
pub fn valid_image_id(id: &str) -> bool {
    id.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        || valid_container_id(id)
}

#[must_use]
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

impl Engine {
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn connect(&self, timeout: Option<Duration>) -> Result<UnixStream, ApiError> {
        let stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(Some(CALL_TIMEOUT))?;
        Ok(stream)
    }

    fn send(
        stream: &mut UnixStream,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<(), ApiError> {
        let mut head = format!("{method} {path} HTTP/1.0\r\nHost: docker\r\n");
        if let Some(body) = body {
            let _ = write!(
                head,
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            );
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        if let Some(body) = body {
            stream.write_all(body)?;
        }
        Ok(())
    }

    fn read_head(reader: &mut impl BufRead) -> Result<(u16, bool), ApiError> {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let code = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| ApiError::Protocol(format!("bad status line: {}", line.trim_end())))?;
        let mut chunked = false;
        let mut header_bytes = line.len();
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            header_bytes += read;
            if header_bytes > 64 * 1024 {
                return Err(ApiError::Protocol("response headers too large".into()));
            }
            if read == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            }
        }
        Ok((code, chunked))
    }

    fn read_body(reader: &mut impl BufRead, chunked: bool) -> Result<Vec<u8>, ApiError> {
        let mut raw = Vec::new();
        reader.take(MAX_RESPONSE_BYTES + 1).read_to_end(&mut raw)?;
        if raw.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(ApiError::Protocol("response too large".into()));
        }
        if !chunked {
            return Ok(raw);
        }
        let mut out = Vec::new();
        let mut rest = raw.as_slice();
        loop {
            let end = rest
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or_else(|| ApiError::Protocol("bad chunk".into()))?;
            let size = std::str::from_utf8(&rest[..end])
                .ok()
                .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok())
                .ok_or_else(|| ApiError::Protocol("bad chunk size".into()))?;
            rest = &rest[end + 2..];
            if size == 0 {
                return Ok(out);
            }
            if rest.len() < size + 2 {
                return Err(ApiError::Protocol("short chunk".into()));
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
    }

    /// One request, whole response.
    ///
    /// # Errors
    /// Fails on transport errors, malformed responses, and status >= 300.
    pub fn call(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Vec<u8>, ApiError> {
        let mut stream = self.connect(Some(CALL_TIMEOUT))?;
        Self::send(&mut stream, method, path, body)?;
        let mut reader = BufReader::new(stream);
        let (code, chunked) = Self::read_head(&mut reader)?;
        let body = Self::read_body(&mut reader, chunked)?;
        if code >= 300 {
            let mut text = String::from_utf8_lossy(&body).into_owned();
            text.truncate(200);
            return Err(ApiError::Status { code, body: text });
        }
        Ok(body)
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let body = self.call("GET", path, None)?;
        serde_json::from_slice(&body).map_err(|error| ApiError::Protocol(error.to_string()))
    }

    /// # Errors
    /// Fails when the engine does not answer `/_ping`.
    pub fn ping(&self) -> Result<(), ApiError> {
        self.call("GET", "/_ping", None).map(|_| ())
    }

    /// All containers (running or not), with no scope filter. Callers filter
    /// on the exact label value themselves.
    ///
    /// # Errors
    /// Fails on engine errors.
    pub fn list_containers(&self, all: bool) -> Result<Vec<ContainerSummary>, ApiError> {
        let path = if all {
            "/containers/json?all=1"
        } else {
            "/containers/json"
        };
        self.get_json(path)
    }

    /// One container by id: state, scope label and published ports. Cheaper
    /// than listing every container when the id is already known.
    ///
    /// # Errors
    /// Fails on engine errors (a 404 for a container that is gone: see
    /// [`ApiError::is_not_found`]), or if `id` is not a container id.
    pub fn inspect_container(&self, id: &str) -> Result<ContainerDetail, ApiError> {
        if !valid_container_id(id) {
            return Err(ApiError::Protocol("invalid container id".into()));
        }
        self.get_json(&format!("/containers/{id}/json"))
    }

    /// Volumes carrying the label `key` (with exactly `value` when given).
    ///
    /// # Errors
    /// Fails on engine errors.
    pub fn list_volumes_with_label(
        &self,
        key: &str,
        value: Option<&str>,
    ) -> Result<Vec<VolumeSummary>, ApiError> {
        let selector = value.map_or_else(|| key.to_owned(), |value| format!("{key}={value}"));
        let filters = serde_json::json!({ "label": [selector] }).to_string();
        let list: VolumeList =
            self.get_json(&format!("/volumes?filters={}", percent_encode(&filters)))?;
        Ok(list.volumes.unwrap_or_default())
    }

    /// # Errors
    /// Fails on engine errors, or if `id` is not a container id.
    pub fn pause(&self, id: &str) -> Result<(), ApiError> {
        self.container_action(id, "pause")
    }

    /// # Errors
    /// Fails on engine errors, or if `id` is not a container id.
    pub fn unpause(&self, id: &str) -> Result<(), ApiError> {
        self.container_action(id, "unpause")
    }

    fn container_action(&self, id: &str, action: &str) -> Result<(), ApiError> {
        if !valid_container_id(id) {
            return Err(ApiError::Protocol("invalid container id".into()));
        }
        self.call("POST", &format!("/containers/{id}/{action}"), None)
            .map(|_| ())
    }

    /// # Errors
    /// Fails on engine errors, or if `id` is not a container id.
    pub fn is_paused(&self, id: &str) -> Result<bool, ApiError> {
        if !valid_container_id(id) {
            return Err(ApiError::Protocol("invalid container id".into()));
        }
        let inspect: ContainerInspect = self.get_json(&format!("/containers/{id}/json"))?;
        Ok(inspect.state.paused)
    }

    /// # Errors
    /// Fails on engine errors, or if `id` is not an image id.
    pub fn image_digests(&self, id: &str) -> Result<Vec<String>, ApiError> {
        if !valid_image_id(id) {
            return Err(ApiError::Protocol("invalid image id".into()));
        }
        let inspect: ImageInspect = self.get_json(&format!("/images/{id}/json"))?;
        Ok(inspect.repo_digests.unwrap_or_default())
    }

    /// Opens the container event stream; each line is one JSON event.
    ///
    /// # Errors
    /// Fails on transport errors or a non-200 answer.
    pub fn events(&self) -> Result<BufReader<UnixStream>, ApiError> {
        self.open_events(&serde_json::json!({ "type": ["container"] }))
    }

    /// `pause` and `unpause` events of all containers, with the engine's own
    /// timestamps (`timeNano`); callers keep the ids they care about.
    ///
    /// # Errors
    /// Fails on transport errors or a non-200 answer.
    pub fn freeze_events(&self) -> Result<BufReader<UnixStream>, ApiError> {
        self.open_events(&serde_json::json!({
            "type": ["container"],
            "event": ["pause", "unpause"],
        }))
    }

    fn open_events(&self, filters: &serde_json::Value) -> Result<BufReader<UnixStream>, ApiError> {
        let mut stream = self.connect(None)?;
        Self::send(
            &mut stream,
            "GET",
            &format!("/events?filters={}", percent_encode(&filters.to_string())),
            None,
        )?;
        let mut reader = BufReader::new(stream);
        let (code, chunked) = Self::read_head(&mut reader)?;
        if code != 200 {
            return Err(ApiError::Status {
                code,
                body: String::new(),
            });
        }
        if chunked {
            return Err(ApiError::Protocol("chunked event stream".into()));
        }
        Ok(reader)
    }
}
