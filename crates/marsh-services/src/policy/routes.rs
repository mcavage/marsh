//! The route allowlist (B1 routing): method + path pattern, versioned and
//! unversioned. Unknown routes are refused (default deny).

use super::Refusal;
use super::http::Target;

pub const MIN_API_MINOR: u32 = 44;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ping,
    Version,
    Info,
    Events,
    ContainerList,
    ContainerCreate,
    ContainerInspect,
    ContainerTop,
    ContainerLogs,
    ContainerChanges,
    ContainerExport,
    ContainerStats,
    ContainerStart,
    ContainerStop,
    ContainerRestart,
    ContainerKill,
    ContainerPause,
    ContainerUnpause,
    ContainerWait,
    ContainerResize,
    ContainerRename,
    ContainerUpdate,
    ContainerAttach,
    ContainerAttachWs,
    ContainerArchiveHead,
    ContainerArchiveGet,
    ContainerArchivePut,
    ContainerDelete,
    ContainerPrune,
    ExecCreate,
    ExecStart,
    ExecResize,
    ExecInspect,
    ImageList,
    ImageCreate,
    ImageInspect,
    ImageHistory,
    ImageSave,
    ImageSaveNames,
    ImageTag,
    ImageDelete,
    Commit,
    Build,
    Session,
    Grpc,
    NetworkList,
    NetworkInspect,
    NetworkCreate,
    NetworkConnect,
    NetworkDisconnect,
    NetworkDelete,
    NetworkPrune,
    VolumeList,
    VolumeCreate,
    VolumeInspect,
    VolumeDelete,
    VolumePrune,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hijack {
    Tcp,
    H2c,
    Websocket,
}

impl Kind {
    #[must_use]
    pub fn hijack(self) -> Option<Hijack> {
        match self {
            Kind::ContainerAttach | Kind::ExecStart => Some(Hijack::Tcp),
            Kind::Session | Kind::Grpc => Some(Hijack::H2c),
            Kind::ContainerAttachWs => Some(Hijack::Websocket),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouteMatch {
    pub kind: Kind,
    /// `Some("v1.47")` or `None` for an unversioned request.
    pub version: Option<String>,
    /// The raw values of `{c}`, `{e}`, `{n}`, `{v}`, `{i}` in order.
    pub params: Vec<String>,
    pub query: Vec<(String, String)>,
    pub head_method: bool,
}

const ROUTES: &[(&str, &str, Kind)] = &[
    ("GET", "/_ping", Kind::Ping),
    ("HEAD", "/_ping", Kind::Ping),
    ("GET", "/version", Kind::Version),
    ("GET", "/info", Kind::Info),
    ("GET", "/events", Kind::Events),
    ("GET", "/containers/json", Kind::ContainerList),
    ("POST", "/containers/create", Kind::ContainerCreate),
    ("POST", "/containers/prune", Kind::ContainerPrune),
    ("GET", "/containers/{c}/json", Kind::ContainerInspect),
    ("GET", "/containers/{c}/top", Kind::ContainerTop),
    ("GET", "/containers/{c}/logs", Kind::ContainerLogs),
    ("GET", "/containers/{c}/changes", Kind::ContainerChanges),
    ("GET", "/containers/{c}/export", Kind::ContainerExport),
    ("GET", "/containers/{c}/stats", Kind::ContainerStats),
    ("POST", "/containers/{c}/start", Kind::ContainerStart),
    ("POST", "/containers/{c}/stop", Kind::ContainerStop),
    ("POST", "/containers/{c}/restart", Kind::ContainerRestart),
    ("POST", "/containers/{c}/kill", Kind::ContainerKill),
    ("POST", "/containers/{c}/pause", Kind::ContainerPause),
    ("POST", "/containers/{c}/unpause", Kind::ContainerUnpause),
    ("POST", "/containers/{c}/wait", Kind::ContainerWait),
    ("POST", "/containers/{c}/resize", Kind::ContainerResize),
    ("POST", "/containers/{c}/rename", Kind::ContainerRename),
    ("POST", "/containers/{c}/update", Kind::ContainerUpdate),
    ("POST", "/containers/{c}/attach", Kind::ContainerAttach),
    ("GET", "/containers/{c}/attach/ws", Kind::ContainerAttachWs),
    (
        "HEAD",
        "/containers/{c}/archive",
        Kind::ContainerArchiveHead,
    ),
    ("GET", "/containers/{c}/archive", Kind::ContainerArchiveGet),
    ("PUT", "/containers/{c}/archive", Kind::ContainerArchivePut),
    ("DELETE", "/containers/{c}", Kind::ContainerDelete),
    ("POST", "/containers/{c}/exec", Kind::ExecCreate),
    ("POST", "/exec/{e}/start", Kind::ExecStart),
    ("POST", "/exec/{e}/resize", Kind::ExecResize),
    ("GET", "/exec/{e}/json", Kind::ExecInspect),
    ("GET", "/images/json", Kind::ImageList),
    ("POST", "/images/create", Kind::ImageCreate),
    ("GET", "/images/{i}/json", Kind::ImageInspect),
    ("GET", "/images/{i}/history", Kind::ImageHistory),
    ("GET", "/images/get", Kind::ImageSaveNames),
    ("GET", "/images/{i}/get", Kind::ImageSave),
    ("POST", "/images/{i}/tag", Kind::ImageTag),
    ("DELETE", "/images/{i}", Kind::ImageDelete),
    ("POST", "/commit", Kind::Commit),
    ("POST", "/build", Kind::Build),
    ("POST", "/session", Kind::Session),
    ("POST", "/grpc", Kind::Grpc),
    ("GET", "/networks", Kind::NetworkList),
    ("POST", "/networks/create", Kind::NetworkCreate),
    ("POST", "/networks/prune", Kind::NetworkPrune),
    ("GET", "/networks/{n}", Kind::NetworkInspect),
    ("POST", "/networks/{n}/connect", Kind::NetworkConnect),
    ("POST", "/networks/{n}/disconnect", Kind::NetworkDisconnect),
    ("DELETE", "/networks/{n}", Kind::NetworkDelete),
    ("GET", "/volumes", Kind::VolumeList),
    ("POST", "/volumes/create", Kind::VolumeCreate),
    ("POST", "/volumes/prune", Kind::VolumePrune),
    ("GET", "/volumes/{v}", Kind::VolumeInspect),
    ("DELETE", "/volumes/{v}", Kind::VolumeDelete),
];

fn simple_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn image_seg(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-:@".contains(&b))
}

fn match_pattern(pat: &str, segs: &[String]) -> Option<Vec<String>> {
    let parts: Vec<&str> = pat.trim_start_matches('/').split('/').collect();
    let mut params = Vec::new();
    let mut i = 0;
    for (pi, part) in parts.iter().enumerate() {
        match *part {
            "{i}" => {
                // One or more segments, up to the remaining literals.
                let tail = parts.len() - pi - 1;
                if segs.len() < i + 1 + tail {
                    return None;
                }
                let take = segs.len() - tail - i;
                let name = segs[i..i + take].join("/");
                if name.len() > 255 || !segs[i..i + take].iter().all(|s| image_seg(s)) {
                    return None;
                }
                params.push(name);
                i += take;
            }
            "{c}" | "{n}" | "{v}" => {
                let s = segs.get(i)?;
                if !simple_name(s) {
                    return None;
                }
                params.push(s.clone());
                i += 1;
            }
            "{e}" => {
                let s = segs.get(i)?;
                if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                params.push(s.clone());
                i += 1;
            }
            lit => {
                if segs.get(i).map(String::as_str) != Some(lit) {
                    return None;
                }
                i += 1;
            }
        }
    }
    (i == segs.len()).then_some(params)
}

fn family_refusal(first: &str) -> Option<&'static str> {
    Some(match first {
        "auth" => "registry login through the services endpoint is not available",
        "plugins" | "swarm" | "services" | "secrets" | "configs" | "nodes" | "tasks" => {
            "plugins, swarm, services, secrets, configs and nodes are not available"
        }
        "system" => "this system route is not available",
        "distribution" => "registry lookups are not available",
        _ => return None,
    })
}

/// Parse the optional `/v1.NN` prefix.
fn version_of(first: &str) -> Result<Option<u32>, Refusal> {
    let Some(rest) = first.strip_prefix('v') else {
        return Ok(None);
    };
    if rest.is_empty() || !rest.as_bytes()[0].is_ascii_digit() {
        return Ok(None);
    }
    let bad = || Refusal::forbid("unsupported API version");
    let (major, minor) = rest.split_once('.').ok_or_else(bad)?;
    // Docker's own router accepts `v1.47.0` and `v01.47`; we accept one spelling.
    if major != "1" || minor.len() != 2 || !minor.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let m: u32 = minor.parse().map_err(|_| bad())?;
    if m < MIN_API_MINOR {
        return Err(Refusal::forbid(format!(
            "API versions older than 1.{MIN_API_MINOR} are not supported"
        )));
    }
    Ok(Some(m))
}

pub fn classify(method: &str, target: &Target) -> Result<RouteMatch, Refusal> {
    let mut segs: &[String] = &target.segments;
    let mut version = None;
    if let Some(first) = segs.first()
        && version_of(first)?.is_some()
    {
        version = Some(first.clone());
        segs = &segs[1..];
    }
    let Some(first) = segs.first() else {
        return Err(Refusal::forbid("route is not allowed"));
    };
    if let Some(why) = family_refusal(first) {
        return Err(Refusal::forbid(why));
    }
    if segs.len() >= 3 && segs[0] == "images" && segs[segs.len() - 1] == "push" {
        return Err(Refusal::forbid(
            "pushing images through the services endpoint is not available",
        ));
    }
    if matches!(segs, [a, b] if a == "images" && (b == "load" || b == "search" || b == "prune")) {
        return Err(Refusal::forbid(match segs[1].as_str() {
            "load" => "loading images through the services endpoint is not available",
            "search" => "registry search is not available",
            _ => "pruning shared images is not available",
        }));
    }
    if matches!(segs, [a, b] if a == "build" && b == "prune") {
        return Err(Refusal::forbid(
            "pruning the shared build cache is not available",
        ));
    }
    let mut method_seen = false;
    for (m, pat, kind) in ROUTES {
        if let Some(params) = match_pattern(pat, segs) {
            if *m == method {
                return Ok(RouteMatch {
                    kind: *kind,
                    version,
                    params,
                    query: target.query.clone(),
                    head_method: method == "HEAD",
                });
            }
            method_seen = true;
        }
    }
    if method_seen {
        return Err(Refusal::forbid(format!(
            "{method} is not allowed on this route"
        )));
    }
    Err(Refusal::forbid("route is not allowed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::http::parse_target;

    fn route(method: &str, target: &str) -> Result<Kind, String> {
        let t = parse_target(target).map_err(str::to_string)?;
        classify(method, &t).map(|m| m.kind).map_err(|r| r.msg)
    }

    #[test]
    fn route_table() {
        for (m, t, k) in [
            (
                "POST",
                "/v1.47/containers/create?name=x",
                Kind::ContainerCreate,
            ),
            ("POST", "/containers/create", Kind::ContainerCreate),
            (
                "GET",
                "/v1.52/containers/abc123/json",
                Kind::ContainerInspect,
            ),
            ("GET", "/containers/json", Kind::ContainerList),
            ("DELETE", "/v1.47/containers/db", Kind::ContainerDelete),
            (
                "GET",
                "/v1.47/images/library/busybox/json",
                Kind::ImageInspect,
            ),
            (
                "POST",
                "/v1.47/images/create?fromImage=busybox",
                Kind::ImageCreate,
            ),
            ("POST", "/session", Kind::Session),
            ("HEAD", "/_ping", Kind::Ping),
        ] {
            assert_eq!(route(m, t), Ok(k), "{m} {t}");
        }
        for (m, t) in [
            ("POST", "/v1.24/containers/create"),
            ("POST", "/v1.47.0/containers/create"),
            ("POST", "/v01.47/containers/create"),
            ("POST", "/v2.47/containers/create"),
            ("PUT", "/containers/create"),
            ("PATCH", "/containers/create"),
            ("GET", "/containers/create"),
            ("GET", "/v1.47/plugins"),
            ("POST", "/v1.47/swarm/init"),
            ("DELETE", "/v1.47/services/x"),
            ("GET", "/secrets"),
            ("POST", "/auth"),
            ("POST", "/images/x/push"),
            ("POST", "/images/load"),
            ("GET", "/system/df"),
            ("POST", "/build/prune"),
            ("GET", "/"),
            ("POST", "/containers/x/y/z"),
            ("GET", "/exec/short/json"),
        ] {
            assert!(route(m, t).is_err(), "{m} {t}");
        }
    }
}
