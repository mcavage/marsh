//! Per-route admission: query allowlists, body schemas, scope checks, and the
//! one canonical request that goes upstream.

use super::create::{
    PinGuard, admit_create, check_labels, check_new_name, stamp_labels, tmpfs_driver_opts,
};
use super::docker::{CREATE, CREATE_FIELDS, ENDPOINT_CONFIG};
use super::http::{Framing, Head};
use super::json::{self, Json, parse, string};
use super::routes::{Hijack, Kind, RouteMatch};
use super::schema::{Field, Ty, check, f, fnull};
use super::{Env, Lookup, Refusal, VolumeScope};
use crate::breaks::break_mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyPolicy {
    None,
    /// Docker (API 1.44+) ignores a body on these action routes, and dockerode
    /// sends `{}`. It is read, bounded, and dropped: never forwarded.
    Discard,
    Json {
        required: bool,
    },
    Stream,
}

#[must_use]
pub fn body_policy(k: Kind) -> BodyPolicy {
    match k {
        Kind::ContainerCreate
        | Kind::ContainerUpdate
        | Kind::ExecCreate
        | Kind::NetworkCreate
        | Kind::NetworkConnect
        | Kind::NetworkDisconnect
        | Kind::VolumeCreate => BodyPolicy::Json { required: true },
        Kind::ExecStart | Kind::Commit => BodyPolicy::Json { required: false },
        Kind::ContainerStart
        | Kind::ContainerStop
        | Kind::ContainerRestart
        | Kind::ContainerKill
        | Kind::ContainerPause
        | Kind::ContainerUnpause
        | Kind::ContainerWait
        | Kind::ContainerResize
        | Kind::ContainerRename
        | Kind::ExecResize
        | Kind::ContainerPrune
        | Kind::NetworkPrune
        | Kind::VolumePrune
        // dockerode (Testcontainers) sends the pull's query again as a small JSON
        // body (`{"fromImage":...,"tag":...}`); the query is what counts, the body
        // is read within the JSON bound and dropped, never forwarded.
        | Kind::ImageCreate => BodyPolicy::Discard,
        Kind::ContainerArchivePut | Kind::Build => BodyPolicy::Stream,
        _ => BodyPolicy::None,
    }
}

#[derive(Debug, Clone)]
pub enum ReqBody {
    None,
    Json(Vec<u8>),
    /// Streamed from the client with its own framing, never buffered.
    Stream(Framing),
}

/// What the agent does with the engine's answer.
#[derive(Debug, Clone)]
pub enum Post {
    None,
    ContainerCreate {
        name: Option<String>,
        /// Pins this create made.
        pins: Vec<String>,
        /// Pins of containers it takes volumes from; they join its lease.
        inherited: Vec<String>,
    },
    ContainerDelete {
        id: String,
    },
    ContainerRename {
        id: String,
        name: String,
    },
    ExecCreate {
        container: String,
    },
    NetworkCreate {
        name: String,
    },
    NetworkDelete {
        id: String,
    },
    VolumeCreate {
        name: Option<String>,
        /// This request reserved the name (give it back if the engine says no).
        reserved: bool,
    },
    VolumeDelete {
        name: String,
    },
    ListContainers,
    ListNetworks,
    ListVolumes,
    Events,
    Prune,
}

#[derive(Debug, Clone)]
pub struct Admitted {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: ReqBody,
    pub content_type: Option<String>,
    pub headers: Vec<(String, String)>,
    pub upgrade: Option<String>,
    pub hijack: Option<Hijack>,
    pub post: Post,
    /// `filter-lenient-json` only: the client's own bytes.
    pub raw_body: Option<Vec<u8>>,
    /// The client's JSON bytes, set by the endpoint (the `filter-forward-original-bytes`
    /// defect forwards them).
    pub client_body: Option<Vec<u8>>,
}

struct Q<'a>(&'a [(String, String)]);

impl Q<'_> {
    fn only(&self, allowed: &[&str]) -> Result<(), Refusal> {
        for (k, _) in self.0 {
            if !allowed.contains(&k.as_str()) {
                return Err(Refusal::forbid(format!(
                    "query parameter {:?} is not allowed on this route",
                    k.chars()
                        .filter(char::is_ascii_graphic)
                        .take(32)
                        .collect::<String>()
                )));
            }
        }
        Ok(())
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(ek, _)| ek == k)
            .map(|(_, v)| v.as_str())
    }

    fn bools(&self, keys: &[&str]) -> Result<(), Refusal> {
        for k in keys {
            if let Some(v) = self.get(k)
                && !matches!(v, "0" | "1" | "true" | "false")
            {
                return Err(Refusal::forbid(format!(
                    "query parameter {k} must be 0, 1, true or false"
                )));
            }
        }
        Ok(())
    }

    fn ints(&self, keys: &[&str]) -> Result<(), Refusal> {
        for k in keys {
            if let Some(v) = self.get(k)
                && (v.is_empty()
                    || v.len() > 10
                    || !v.bytes().all(|b| b.is_ascii_digit() || b == b'-')
                    || v[1..].contains('-'))
            {
                return Err(Refusal::forbid(format!(
                    "query parameter {k} must be an integer"
                )));
            }
        }
        Ok(())
    }

    fn token(&self, k: &str, max: usize) -> Result<(), Refusal> {
        if let Some(v) = self.get(k)
            && (v.len() > max
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-:/,@=+ ".contains(&b)))
        {
            return Err(Refusal::forbid(format!(
                "query parameter {k} has an unsupported value"
            )));
        }
        Ok(())
    }

    fn pass(&self, keys: &[&str]) -> Vec<(String, String)> {
        self.0
            .iter()
            .filter(|(k, _)| keys.contains(&k.as_str()))
            .cloned()
            .collect()
    }
}

/// Normalize a `filters` parameter to Docker's map form, allowlist the keys
/// and optionally pin results to the caller's scope.
fn filters_param(
    q: &Q<'_>,
    allowed: &[&str],
    scope: Option<&str>,
) -> Result<Option<String>, Refusal> {
    let mut map: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(raw) = q.get("filters") {
        let j = parse(raw.as_bytes()).map_err(|e| Refusal::forbid(format!("filters: {e}")))?;
        let Json::Obj(members) = j else {
            return Err(Refusal::forbid("filters must be an object"));
        };
        for (k, v) in members {
            if !allowed.contains(&k.as_str()) {
                return Err(Refusal::forbid(format!(
                    "filter {:?} is not allowed",
                    k.chars()
                        .filter(char::is_ascii_graphic)
                        .take(24)
                        .collect::<String>()
                )));
            }
            let vals: Vec<String> = match v {
                Json::Obj(m) => {
                    let mut out = Vec::new();
                    for (ik, iv) in m {
                        match iv {
                            Json::Bool(true) => out.push(ik),
                            Json::Bool(false) => {}
                            _ => return Err(Refusal::forbid("filter values must be booleans")),
                        }
                    }
                    out
                }
                Json::Arr(a) => {
                    let mut out = Vec::new();
                    for x in a {
                        out.push(
                            x.as_str()
                                .ok_or_else(|| Refusal::forbid("filter values must be strings"))?
                                .to_string(),
                        );
                    }
                    out
                }
                _ => return Err(Refusal::forbid("filters must map names to lists")),
            };
            map.push((k, vals));
        }
    }
    if let Some(scope) = scope
        && !break_mode("lane-authority-off")
    {
        let want = format!("dev.marsh.scope={scope}");
        match map.iter_mut().find(|(k, _)| k == "label") {
            Some((_, v)) => v.push(want),
            None => map.push(("label".into(), vec![want])),
        }
    }
    if map.is_empty() {
        return Ok(None);
    }
    let obj = Json::Obj(
        map.into_iter()
            .map(|(k, vs)| {
                (
                    k,
                    Json::Obj(vs.into_iter().map(|v| (v, Json::Bool(true))).collect()),
                )
            })
            .collect(),
    );
    Ok(Some(obj.to_string_json()))
}

fn container_id(env: &dyn Env, kind: Kind, r: &str) -> Result<String, Refusal> {
    if break_mode("lane-authority-off")
        || (kind == Kind::ContainerInspect
            && break_mode("scope-reference-check-off")
            && r.len() == 64)
    {
        return Ok(r.to_string());
    }
    match env.container(r) {
        Lookup::Own(id) => Ok(id),
        Lookup::Missing => Err(Refusal::not_found(format!("No such container: {r}"))),
        Lookup::Ambiguous => Err(Refusal {
            status: 400,
            msg: format!("Multiple IDs found with provided prefix: {r}"),
            close: false,
        }),
    }
}

fn network_id(env: &dyn Env, r: &str) -> Result<String, Refusal> {
    if break_mode("lane-authority-off") {
        return Ok(r.to_string());
    }
    match env.network(r) {
        Lookup::Own(id) => Ok(id),
        _ => Err(Refusal::not_found(format!("network {r} not found"))),
    }
}

fn volume_name(env: &dyn Env, r: &str) -> Result<String, Refusal> {
    if break_mode("lane-authority-off") {
        return Ok(r.to_string());
    }
    match env.volume(r) {
        VolumeScope::Own => Ok(r.to_string()),
        _ => Err(Refusal::not_found(format!("get {r}: no such volume"))),
    }
}

// --- body schemas -----------------------------------------------------------

static EXEC_CREATE: &[Field] = &[
    f("AttachStdin", Ty::Bool),
    f("AttachStdout", Ty::Bool),
    f("AttachStderr", Ty::Bool),
    fnull("ConsoleSize", Ty::Arr(&Ty::Int(0, i64::MAX))),
    f("DetachKeys", Ty::Str),
    f("Tty", Ty::Bool),
    fnull("Env", Ty::Arr(&Ty::Str)),
    fnull("Cmd", Ty::StrOrArr),
    f("Privileged", Ty::False),
    f("User", Ty::Str),
    f("WorkingDir", Ty::Str),
];

static EXEC_START: &[Field] = &[
    f("Detach", Ty::Bool),
    f("Tty", Ty::Bool),
    fnull("ConsoleSize", Ty::Arr(&Ty::Int(0, i64::MAX))),
];

static IPAM_CFG: &[Field] = &[
    f("Subnet", Ty::Str),
    f("IPRange", Ty::Str),
    f("Gateway", Ty::Str),
    fnull("AuxiliaryAddresses", Ty::StrMap(&Ty::Str)),
];
static IPAM: &[Field] = &[
    f("Driver", Ty::StrEnum(&["", "default"])),
    fnull("Config", Ty::Arr(&Ty::Obj(IPAM_CFG))),
    fnull("Options", Ty::Zero),
];
static NETWORK_CREATE: &[Field] = &[
    f("Name", Ty::Str),
    f("Driver", Ty::StrEnum(&["", "bridge"])),
    f("Scope", Ty::StrEnum(&["", "local"])),
    f("EnableIPv4", Ty::Bool),
    f("EnableIPv6", Ty::Bool),
    f("Internal", Ty::Bool),
    f("Attachable", Ty::Bool),
    f("Ingress", Ty::Zero),
    f("ConfigOnly", Ty::Zero),
    fnull("ConfigFrom", Ty::Zero),
    f("CheckDuplicate", Ty::Bool),
    fnull("IPAM", Ty::Obj(IPAM)),
    fnull("Options", Ty::StrMap(&Ty::Str)),
    fnull("Labels", Ty::StrMap(&Ty::Str)),
];
static NETWORK_CONNECT: &[Field] = &[f("Container", Ty::Str), fnull("EndpointConfig", Ty::Raw)];
static NETWORK_DISCONNECT: &[Field] = &[f("Container", Ty::Str), f("Force", Ty::Bool)];
static VOLUME_CREATE: &[Field] = &[
    f("Name", Ty::Str),
    f("Driver", Ty::StrEnum(&["", "local"])),
    fnull("DriverOpts", Ty::StrMap(&Ty::Str)),
    fnull("Labels", Ty::StrMap(&Ty::Str)),
    fnull("ClusterVolumeSpec", Ty::Zero),
];
static UPDATE: &[Field] = &[
    f("BlkioWeight", Ty::Int(0, 1000)),
    f("CpuShares", Ty::Int(0, 1_000_000)),
    f("CpuPeriod", Ty::Int(0, 1_000_000)),
    f("CpuQuota", Ty::Int(0, i64::MAX)),
    f("CpusetCpus", Ty::Str),
    f("CpusetMems", Ty::Str),
    f("Memory", Ty::Int(0, i64::MAX)),
    f("MemorySwap", Ty::Int(-1, i64::MAX)),
    f("MemoryReservation", Ty::Int(0, i64::MAX)),
    f("NanoCpus", Ty::Int(0, i64::MAX)),
    fnull("PidsLimit", Ty::Int(-1, i64::MAX)),
    // The CLI sends the zero value; changing the policy is not a resource.
    f("RestartPolicy", Ty::Zero),
    fnull("BlkioWeightDevice", Ty::Zero),
    fnull("BlkioDeviceReadBps", Ty::Zero),
    fnull("BlkioDeviceWriteBps", Ty::Zero),
    fnull("BlkioDeviceReadIOps", Ty::Zero),
    fnull("BlkioDeviceWriteIOps", Ty::Zero),
    fnull("Devices", Ty::Zero),
    fnull("DeviceCgroupRules", Ty::Zero),
    fnull("DeviceRequests", Ty::Zero),
    f("CpuCount", Ty::Zero),
    f("CpuPercent", Ty::Zero),
    f("CpuRealtimePeriod", Ty::Zero),
    f("CpuRealtimeRuntime", Ty::Zero),
    fnull("OomKillDisable", Ty::Zero),
    f("KernelMemory", Ty::Zero),
    f("KernelMemoryTCP", Ty::Zero),
    f("CgroupParent", Ty::Zero),
    f("IOMaximumIOps", Ty::Zero),
    f("IOMaximumBandwidth", Ty::Zero),
    fnull("Ulimits", Ty::Zero),
    fnull("MemorySwappiness", Ty::Int(-1, 100)),
];

fn json_body(raw: Option<Vec<u8>>, required: bool) -> Result<(Json, Option<Vec<u8>>), Refusal> {
    let bytes = raw.unwrap_or_default();
    if bytes.iter().all(u8::is_ascii_whitespace) {
        if required {
            return Err(Refusal::forbid("a JSON object body is required"));
        }
        return Ok((Json::Obj(Vec::new()), None));
    }
    match parse(&bytes) {
        Ok(j @ Json::Obj(_)) => Ok((j, None)),
        Ok(_) => Err(Refusal::forbid("body must be a JSON object")),
        Err(e) => {
            if break_mode("filter-lenient-json") {
                // Defect: fail open, forward what the client sent.
                return Ok((Json::Obj(Vec::new()), Some(bytes)));
            }
            Err(Refusal::forbid(e.0))
        }
    }
}

fn schema_err(e: super::schema::SchemaError) -> Refusal {
    Refusal::forbid(e.0)
}

/// Header allowlist (B1): only these ever reach the engine.
fn forwardable(head: &Head, kind: Kind) -> Result<Vec<(String, String)>, Refusal> {
    let single: &[(&str, usize)] = &[
        ("user-agent", 256),
        ("x-registry-auth", 16 * 1024),
        ("x-registry-config", 16 * 1024),
        ("x-docker-expose-session-uuid", 128),
        ("x-docker-expose-session-name", 128),
        ("x-docker-expose-session-sharedkey", 256),
    ];
    let session = matches!(kind, Kind::Session | Kind::Build | Kind::Grpc);
    let auth = matches!(kind, Kind::ImageCreate | Kind::Build);
    let mut out = Vec::new();
    for (name, max) in single {
        let is_session = name.starts_with("x-docker-expose-session");
        if (is_session && !session) || (name.starts_with("x-registry") && !auth) {
            continue;
        }
        let vals: Vec<&str> = head.headers_named(name).collect();
        if vals.len() > 1 {
            return Err(Refusal {
                status: 400,
                msg: format!("duplicate {name} header"),
                close: true,
            });
        }
        if let Some(v) = vals.first() {
            if v.len() > *max || !v.bytes().all(|b| (0x20..0x7f).contains(&b)) {
                return Err(Refusal {
                    status: 400,
                    msg: format!("invalid {name} header"),
                    close: true,
                });
            }
            out.push(((*name).to_string(), (*v).to_string()));
        }
    }
    if session {
        for v in head.headers_named("x-docker-expose-session-grpc-method") {
            if v.len() > 256
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
            {
                return Err(Refusal {
                    status: 400,
                    msg: "invalid x-docker-expose-session-grpc-method header".into(),
                    close: true,
                });
            }
            out.push(("x-docker-expose-session-grpc-method".into(), v.to_string()));
        }
    }
    // A WebSocket upgrade needs its handshake key and version at the engine, which
    // answers 400 without them. Only on the websocket route, only in their exact
    // shape (a 16-byte nonce in base64; version 13).
    if kind.hijack() == Some(Hijack::Websocket) && head.upgrade.as_deref() == Some("websocket") {
        let bad = |what: &str| Refusal {
            status: 400,
            msg: format!("invalid {what} header"),
            close: true,
        };
        let key: Vec<&str> = head.headers_named("sec-websocket-key").collect();
        let ver: Vec<&str> = head.headers_named("sec-websocket-version").collect();
        if key.len() != 1 || ver.len() != 1 {
            return Err(bad("Sec-WebSocket-Key/Version"));
        }
        let k = key[0];
        let b64 = |b: u8| b.is_ascii_alphanumeric() || b == b'+' || b == b'/';
        if k.len() != 24 || !k.as_bytes()[..22].iter().all(|b| b64(*b)) || &k[22..] != "==" {
            return Err(bad("Sec-WebSocket-Key"));
        }
        if ver[0] != "13" {
            return Err(bad("Sec-WebSocket-Version"));
        }
        out.push(("Sec-WebSocket-Key".into(), k.to_string()));
        out.push(("Sec-WebSocket-Version".into(), "13".into()));
    }
    Ok(out)
}

fn content_type(head: &Head) -> Result<Option<String>, Refusal> {
    let vals: Vec<&str> = head.headers_named("content-type").collect();
    if vals.len() > 1 {
        return Err(Refusal {
            status: 400,
            msg: "duplicate Content-Type header".into(),
            close: true,
        });
    }
    Ok(vals.first().map(|v| v.to_ascii_lowercase()))
}

fn upgrade_for(kind: Kind, head: &Head) -> Result<Option<String>, Refusal> {
    let Some(up) = head.upgrade.clone() else {
        if head.h2c_settings {
            return Err(Refusal {
                status: 400,
                msg: "h2c upgrade is not accepted".into(),
                close: true,
            });
        }
        return Ok(None);
    };
    match kind.hijack() {
        Some(h) => {
            let want = match h {
                Hijack::Tcp => "tcp",
                Hijack::H2c => "h2c",
                Hijack::Websocket => "websocket",
            };
            if up == want {
                Ok(Some(up))
            } else {
                Err(Refusal {
                    status: 400,
                    msg: "unexpected Upgrade for this route".into(),
                    close: true,
                })
            }
        }
        None if up == "h2c" || head.h2c_settings => Err(Refusal {
            status: 400,
            msg: "h2c upgrade is not accepted".into(),
            close: true,
        }),
        // Any other Upgrade on a plain route is ignored and never forwarded.
        None => Ok(None),
    }
}

fn mk_path(m: &RouteMatch, rest: &[&str]) -> String {
    let mut p = String::new();
    if let Some(v) = &m.version {
        p.push('/');
        p.push_str(v);
    }
    for s in rest {
        p.push('/');
        p.push_str(s);
    }
    p
}

const TAR_TYPES: [&str; 4] = [
    "application/x-tar",
    "application/octet-stream",
    "application/gzip",
    "application/x-gzip",
];

/// Validate a route and build the canonical upstream request. `body` is the
/// raw JSON body for [`BodyPolicy::Json`] routes.
#[allow(clippy::too_many_lines)]
pub fn admit(
    m: &RouteMatch,
    head: &Head,
    body: Option<Vec<u8>>,
    env: &dyn Env,
) -> Result<Admitted, Refusal> {
    let kind = m.kind;
    // dockerode (Testcontainers) repeats body fields in the query string of
    // create calls; Docker ignores them. They are dropped, never forwarded.
    let stripped: Vec<(String, String)> = {
        let fields: &[Field] = match kind {
            Kind::ContainerCreate => CREATE_FIELDS,
            Kind::ExecCreate => EXEC_CREATE,
            Kind::ExecStart => EXEC_START,
            Kind::NetworkCreate => NETWORK_CREATE,
            Kind::VolumeCreate => VOLUME_CREATE,
            Kind::ContainerUpdate => UPDATE,
            _ => &[],
        };
        m.query
            .iter()
            .filter(|(k, _)| k == "name" || !fields.iter().any(|f| f.name == k))
            // dockerode also puts the pull's (empty) registry auth in the query;
            // it is never forwarded, and no `X-Registry-Auth` header passes either.
            .filter(|(k, _)| !(kind == Kind::ImageCreate && k == "authconfig"))
            .cloned()
            .collect()
    };
    let q = Q(&stripped);
    if body_policy(kind) == BodyPolicy::None
        && !matches!(head.framing, Framing::None | Framing::Length(0))
    {
        return Err(Refusal::forbid("this route does not take a request body").closing());
    }
    content_type(head)?;
    let mut guard = PinGuard::new(env);
    let mut a = Admitted {
        method: head.method.clone(),
        path: String::new(),
        query: Vec::new(),
        body: ReqBody::None,
        content_type: None,
        headers: forwardable(head, kind)?,
        upgrade: upgrade_for(kind, head)?,
        hijack: kind.hijack(),
        post: Post::None,
        raw_body: None,
        client_body: None,
    };
    let p = |i: usize| m.params.get(i).map_or("", String::as_str);
    let scope_filters =
        |allowed: &[&str]| filters_param(&q, allowed, Some(&env.authority().workspace));
    let set_filters = |a: &mut Admitted, f: Option<String>| {
        if let Some(f) = f {
            a.query.push(("filters".into(), f));
        }
    };
    match kind {
        Kind::Ping => {
            q.only(&[])?;
            a.path = mk_path(m, &["_ping"]);
        }
        Kind::Version | Kind::Info => {
            q.only(&[])?;
            a.path = mk_path(
                m,
                &[if kind == Kind::Version {
                    "version"
                } else {
                    "info"
                }],
            );
        }
        Kind::Events => {
            q.only(&["since", "until", "filters"])?;
            q.token("since", 40)?;
            q.token("until", 40)?;
            a.path = mk_path(m, &["events"]);
            a.query = q.pass(&["since", "until"]);
            let f = filters_param(
                &q,
                &[
                    "type",
                    "event",
                    "container",
                    "image",
                    "label",
                    "volume",
                    "network",
                    "daemon",
                    "scope",
                ],
                None,
            )?;
            set_filters(&mut a, f);
            a.post = Post::Events;
        }
        Kind::ContainerList => {
            q.only(&["all", "limit", "size", "filters"])?;
            q.bools(&["all", "size"])?;
            q.ints(&["limit"])?;
            a.path = mk_path(m, &["containers", "json"]);
            a.query = q.pass(&["all", "limit", "size"]);
            let f = scope_filters(&[
                "ancestor",
                "before",
                "exited",
                "expose",
                "health",
                "id",
                "isolation",
                "label",
                "name",
                "network",
                "publish",
                "since",
                "status",
                "volume",
            ])?;
            set_filters(&mut a, f);
            a.post = Post::ListContainers;
        }
        Kind::ContainerCreate => {
            q.only(&["name", "platform"])?;
            if let Some(n) = q.get("name") {
                check_new_name("container", n)?;
            }
            q.token("platform", 64)?;
            let (j, raw) = json_body(body, true)?;
            a.raw_body = raw;
            let canon = if a.raw_body.is_some() {
                j
            } else {
                let c = check(&CREATE, &j, "$").map_err(schema_err)?;
                admit_create(env, c, &mut guard)?
            };
            a.path = mk_path(m, &["containers", "create"]);
            a.query = q.pass(&["name", "platform"]);
            a.body = ReqBody::Json(canon.to_bytes());
            a.content_type = Some("application/json".into());
            let inherited = std::mem::take(&mut guard.inherited);
            a.post = Post::ContainerCreate {
                name: q.get("name").map(str::to_string),
                pins: guard.disarm(),
                inherited,
            };
        }
        Kind::ContainerPrune => {
            q.only(&["filters"])?;
            a.path = mk_path(m, &["containers", "prune"]);
            let f = scope_filters(&["until", "label"])?;
            set_filters(&mut a, f);
            a.post = Post::Prune;
        }
        Kind::ContainerInspect
        | Kind::ContainerTop
        | Kind::ContainerLogs
        | Kind::ContainerChanges
        | Kind::ContainerExport
        | Kind::ContainerStats => {
            let id = container_id(env, kind, p(0))?;
            let (seg, keys): (&str, &[&str]) = match kind {
                Kind::ContainerInspect => ("json", &["size"]),
                Kind::ContainerTop => ("top", &["ps_args"]),
                Kind::ContainerLogs => (
                    "logs",
                    &[
                        "follow",
                        "stdout",
                        "stderr",
                        "since",
                        "until",
                        "timestamps",
                        "tail",
                        "details",
                    ],
                ),
                Kind::ContainerChanges => ("changes", &[]),
                Kind::ContainerExport => ("export", &[]),
                _ => ("stats", &["stream", "one-shot"]),
            };
            q.only(keys)?;
            q.bools(&[
                "size",
                "follow",
                "stdout",
                "stderr",
                "timestamps",
                "details",
                "stream",
                "one-shot",
            ])?;
            q.token("ps_args", 64)?;
            q.token("since", 40)?;
            q.token("until", 40)?;
            q.token("tail", 12)?;
            a.path = mk_path(m, &["containers", &id, seg]);
            a.query = q.pass(keys);
        }
        Kind::ContainerStart
        | Kind::ContainerStop
        | Kind::ContainerRestart
        | Kind::ContainerKill
        | Kind::ContainerPause
        | Kind::ContainerUnpause
        | Kind::ContainerWait => {
            let id = container_id(env, kind, p(0))?;
            // A pin that restore could not recreate fails `start` closed: the
            // container would otherwise run on an empty directory (A3).
            if matches!(kind, Kind::ContainerStart | Kind::ContainerRestart)
                && env.container_blocked(&id)
                && !break_mode("restore-fail-open")
            {
                return Err(Refusal::forbid(
                    "a bind mount of this container could not be restored after the engine restarted; remove the container and create it again",
                ));
            }
            let (seg, keys): (&str, &[&str]) = match kind {
                Kind::ContainerStart => ("start", &["detachKeys"]),
                Kind::ContainerStop => ("stop", &["signal", "t"]),
                Kind::ContainerRestart => ("restart", &["signal", "t"]),
                Kind::ContainerKill => ("kill", &["signal"]),
                Kind::ContainerPause => ("pause", &[]),
                Kind::ContainerUnpause => ("unpause", &[]),
                _ => ("wait", &["condition"]),
            };
            q.only(keys)?;
            q.ints(&["t"])?;
            q.token("signal", 16)?;
            q.token("detachKeys", 32)?;
            if let Some(c) = q.get("condition")
                && !matches!(c, "not-running" | "next-exit" | "removed")
            {
                return Err(Refusal::forbid("unsupported wait condition"));
            }
            a.path = mk_path(m, &["containers", &id, seg]);
            a.query = q.pass(keys);
        }
        Kind::ContainerResize | Kind::ExecResize => {
            q.only(&["h", "w"])?;
            q.ints(&["h", "w"])?;
            a.query = q.pass(&["h", "w"]);
            if kind == Kind::ContainerResize {
                let id = container_id(env, kind, p(0))?;
                a.path = mk_path(m, &["containers", &id, "resize"]);
            } else {
                exec_check(env, p(0))?;
                a.path = mk_path(m, &["exec", p(0), "resize"]);
            }
        }
        Kind::ContainerRename => {
            q.only(&["name"])?;
            let id = container_id(env, kind, p(0))?;
            let name = q
                .get("name")
                .ok_or_else(|| Refusal::forbid("name is required"))?;
            check_new_name("container", name)?;
            a.path = mk_path(m, &["containers", &id, "rename"]);
            a.query = q.pass(&["name"]);
            a.post = Post::ContainerRename {
                id,
                name: name.to_string(),
            };
        }
        Kind::ContainerUpdate => {
            q.only(&[])?;
            let id = container_id(env, kind, p(0))?;
            let (mut j, raw) = json_body(body, true)?;
            a.raw_body = raw;
            if break_mode("restart-update-allowed")
                && let Json::Obj(f) = &mut j
            {
                // Defect: a restart-policy change is not refused.
                f.retain(|(k, _)| !k.eq_ignore_ascii_case("RestartPolicy"));
            }
            let mut c = if a.raw_body.is_some() {
                j
            } else {
                check(&Ty::Obj(UPDATE), &j, "$").map_err(schema_err)?
            };
            update_limits(env, &id, &mut c)?;
            a.path = mk_path(m, &["containers", &id, "update"]);
            a.body = ReqBody::Json(c.to_bytes());
            a.content_type = Some("application/json".into());
        }
        Kind::ContainerAttach | Kind::ContainerAttachWs => {
            q.only(&["detachKeys", "logs", "stream", "stdin", "stdout", "stderr"])?;
            q.bools(&["logs", "stream", "stdin", "stdout", "stderr"])?;
            q.token("detachKeys", 32)?;
            let id = container_id(env, kind, p(0))?;
            a.path = if kind == Kind::ContainerAttach {
                mk_path(m, &["containers", &id, "attach"])
            } else {
                mk_path(m, &["containers", &id, "attach", "ws"])
            };
            a.query = q.pass(&["detachKeys", "logs", "stream", "stdin", "stdout", "stderr"]);
        }
        Kind::ContainerArchiveHead | Kind::ContainerArchiveGet | Kind::ContainerArchivePut => {
            let keys: &[&str] = if kind == Kind::ContainerArchivePut {
                &["path", "noOverwriteDirNonDir", "copyUIDGID"]
            } else {
                &["path"]
            };
            q.only(keys)?;
            q.bools(&["noOverwriteDirNonDir", "copyUIDGID"])?;
            let path = q
                .get("path")
                .ok_or_else(|| Refusal::forbid("path is required"))?;
            if !path.starts_with('/') || path.len() > 4096 || path.bytes().any(|b| b < 0x20) {
                return Err(Refusal::forbid("archive path must be absolute"));
            }
            let id = container_id(env, kind, p(0))?;
            a.path = mk_path(m, &["containers", &id, "archive"]);
            a.query = q.pass(keys);
            if kind == Kind::ContainerArchivePut {
                a.body = ReqBody::Stream(head.framing);
                a.content_type = Some(stream_content_type(head)?);
            }
        }
        Kind::ContainerDelete => {
            q.only(&["v", "force", "link"])?;
            q.bools(&["v", "force", "link"])?;
            if matches!(q.get("link"), Some("1" | "true")) {
                return Err(Refusal::forbid("removing links is not allowed"));
            }
            let id = container_id(env, kind, p(0))?;
            a.path = mk_path(m, &["containers", &id]);
            a.query = q.pass(&["v", "force"]);
            a.post = Post::ContainerDelete { id };
        }
        Kind::ExecCreate => {
            q.only(&[])?;
            let id = container_id(env, kind, p(0))?;
            let (j, raw) = json_body(body, true)?;
            a.raw_body = raw;
            let c = if a.raw_body.is_some() {
                j
            } else {
                check(&Ty::Obj(EXEC_CREATE), &j, "$").map_err(schema_err)?
            };
            a.path = mk_path(m, &["containers", &id, "exec"]);
            a.body = ReqBody::Json(c.to_bytes());
            a.content_type = Some("application/json".into());
            a.post = Post::ExecCreate { container: id };
        }
        Kind::ExecStart => {
            q.only(&[])?;
            exec_check(env, p(0))?;
            let (j, raw) = json_body(body, false)?;
            a.raw_body = raw;
            let c = if a.raw_body.is_some() {
                j
            } else {
                check(&Ty::Obj(EXEC_START), &j, "$").map_err(schema_err)?
            };
            a.path = mk_path(m, &["exec", p(0), "start"]);
            a.body = ReqBody::Json(c.to_bytes());
            a.content_type = Some("application/json".into());
        }
        Kind::ExecInspect => {
            q.only(&[])?;
            exec_check(env, p(0))?;
            a.path = mk_path(m, &["exec", p(0), "json"]);
        }
        Kind::ImageList => {
            q.only(&["all", "digests", "filters", "shared-size", "manifests"])?;
            q.bools(&["all", "digests", "shared-size", "manifests"])?;
            a.path = mk_path(m, &["images", "json"]);
            a.query = q.pass(&["all", "digests", "shared-size", "manifests"]);
            let f = filters_param(
                &q,
                &["before", "dangling", "label", "reference", "since", "until"],
                None,
            )?;
            set_filters(&mut a, f);
        }
        Kind::ImageCreate => {
            q.only(&["fromImage", "tag", "platform"])?;
            let img = q
                .get("fromImage")
                .ok_or_else(|| Refusal::forbid("importing images (fromSrc) is not available"))?;
            if img.is_empty()
                || img.len() > 512
                || !img
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-/:@".contains(&b))
            {
                return Err(Refusal::forbid("invalid image reference"));
            }
            if !env.registry_allowed(img) {
                return Err(Refusal::forbid("image registry is not allowed by policy"));
            }
            q.token("tag", 128)?;
            q.token("platform", 64)?;
            a.path = mk_path(m, &["images", "create"]);
            a.query = q.pass(&["fromImage", "tag", "platform"]);
        }
        Kind::ImageInspect | Kind::ImageHistory | Kind::ImageSave => {
            let seg = match kind {
                Kind::ImageInspect => "json",
                Kind::ImageHistory => "history",
                _ => "get",
            };
            q.only(&["manifests", "platform"])?;
            q.bools(&["manifests"])?;
            q.token("platform", 128)?;
            image_name_ok(p(0))?;
            a.path = format!(
                "{}/images/{}/{seg}",
                m.version
                    .as_ref()
                    .map_or(String::new(), |v| format!("/{v}")),
                p(0)
            );
            a.query = q.pass(&["manifests", "platform"]);
        }
        Kind::ImageSaveNames => {
            q.only(&["names", "platform"])?;
            let n = q
                .get("names")
                .ok_or_else(|| Refusal::forbid("names is required"))?;
            image_name_ok(n)?;
            q.token("platform", 128)?;
            a.path = mk_path(m, &["images", "get"]);
            a.query = q.pass(&["names", "platform"]);
        }
        Kind::ImageTag => {
            q.only(&["repo", "tag"])?;
            image_name_ok(p(0))?;
            let repo = q
                .get("repo")
                .ok_or_else(|| Refusal::forbid("repo is required"))?;
            reserved_repo(repo)?;
            q.token("repo", 255)?;
            q.token("tag", 128)?;
            a.path = format!(
                "{}/images/{}/tag",
                m.version
                    .as_ref()
                    .map_or(String::new(), |v| format!("/{v}")),
                p(0)
            );
            a.query = q.pass(&["repo", "tag"]);
        }
        Kind::ImageDelete => {
            q.only(&["force", "noprune"])?;
            q.bools(&["force", "noprune"])?;
            image_name_ok(p(0))?;
            a.path = format!(
                "{}/images/{}",
                m.version
                    .as_ref()
                    .map_or(String::new(), |v| format!("/{v}")),
                p(0)
            );
            a.query = q.pass(&["force", "noprune"]);
        }
        Kind::Commit => {
            q.only(&["container", "repo", "tag", "comment", "author", "pause"])?;
            q.bools(&["pause"])?;
            let c = q
                .get("container")
                .ok_or_else(|| Refusal::forbid("container is required"))?;
            let id = container_id(env, kind, c)?;
            if let Some(repo) = q.get("repo") {
                reserved_repo(repo)?;
                q.token("repo", 255)?;
                scope_repo(&env.authority().workspace, repo)?;
            }
            q.token("tag", 128)?;
            let (j, raw) = json_body(body, false)?;
            a.raw_body = raw;
            let cfg = if a.raw_body.is_some() {
                j
            } else {
                check(&CREATE, &j, "$").map_err(schema_err)?
            };
            if !matches!(cfg.get("HostConfig"), None | Some(Json::Null))
                || !matches!(cfg.get("NetworkingConfig"), None | Some(Json::Null))
            {
                return Err(Refusal::forbid("commit accepts a container config only"));
            }
            if let Some(l) = cfg.get("Labels") {
                check_labels(l)?;
            }
            a.path = mk_path(m, &["commit"]);
            a.query = q.pass(&["repo", "tag", "comment", "author", "pause"]);
            a.query.push(("container".into(), id));
            a.body = ReqBody::Json(cfg.to_bytes());
            a.content_type = Some("application/json".into());
        }
        Kind::Build => build(&q, m, head, env, &mut a)?,
        Kind::Session => {
            q.only(&[])?;
            a.path = mk_path(m, &["session"]);
        }
        Kind::Grpc => {
            q.only(&[])?;
            a.path = mk_path(m, &["grpc"]);
        }
        Kind::NetworkList => {
            q.only(&["filters"])?;
            a.path = mk_path(m, &["networks"]);
            let f = scope_filters(&["dangling", "driver", "id", "label", "name", "scope", "type"])?;
            set_filters(&mut a, f);
            a.post = Post::ListNetworks;
        }
        Kind::NetworkInspect => {
            q.only(&["verbose", "scope"])?;
            q.bools(&["verbose"])?;
            q.token("scope", 16)?;
            let id = network_id(env, p(0))?;
            a.path = mk_path(m, &["networks", &id]);
            a.query = q.pass(&["verbose", "scope"]);
        }
        Kind::NetworkCreate => {
            q.only(&[])?;
            let (j, raw) = json_body(body, true)?;
            a.raw_body = raw;
            let mut c = if a.raw_body.is_some() {
                j
            } else {
                check(&Ty::Obj(NETWORK_CREATE), &j, "$").map_err(schema_err)?
            };
            let name = c
                .get("Name")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            check_new_name("network", &name)?;
            if matches!(name.as_str(), "bridge" | "host" | "none" | "default") {
                return Err(Refusal::forbid("that network name is reserved by Docker"));
            }
            if let Some(Json::Obj(o)) = c.get("Options")
                && !o.is_empty()
                && !(break_mode("network-mtu-allowed")
                    && o.iter().all(|(k, _)| k == "com.docker.network.driver.mtu"))
            {
                return Err(Refusal::forbid("network options are not allowed"));
            }
            if let Some(l) = c.get("Labels") {
                check_labels(l)?;
            }
            if a.raw_body.is_none() {
                stamp_labels(&mut c, env);
            }
            a.path = mk_path(m, &["networks", "create"]);
            a.body = ReqBody::Json(c.to_bytes());
            a.content_type = Some("application/json".into());
            a.post = Post::NetworkCreate { name };
        }
        Kind::NetworkConnect | Kind::NetworkDisconnect => {
            q.only(&[])?;
            let net = network_id(env, p(0))?;
            let spec = if kind == Kind::NetworkConnect {
                NETWORK_CONNECT
            } else {
                NETWORK_DISCONNECT
            };
            let (j, raw) = json_body(body, true)?;
            a.raw_body = raw;
            let mut c = if a.raw_body.is_some() {
                j
            } else {
                check(&Ty::Obj(spec), &j, "$").map_err(schema_err)?
            };
            if a.raw_body.is_none() {
                let cn = c.get("Container").and_then(Json::as_str).unwrap_or("");
                let cid = container_id(env, kind, cn)?;
                c.set("Container", string(&cid));
                if let Some(ep) = c.get("EndpointConfig").cloned()
                    && !ep.is_null()
                {
                    let checked =
                        check(&ENDPOINT_CONFIG, &ep, "$.EndpointConfig").map_err(schema_err)?;
                    c.set("EndpointConfig", checked);
                }
            }
            let seg = if kind == Kind::NetworkConnect {
                "connect"
            } else {
                "disconnect"
            };
            a.path = mk_path(m, &["networks", &net, seg]);
            a.body = ReqBody::Json(c.to_bytes());
            a.content_type = Some("application/json".into());
        }
        Kind::NetworkDelete => {
            q.only(&[])?;
            let id = network_id(env, p(0))?;
            a.path = mk_path(m, &["networks", &id]);
            a.post = Post::NetworkDelete { id };
        }
        Kind::NetworkPrune => {
            q.only(&["filters"])?;
            a.path = mk_path(m, &["networks", "prune"]);
            let f = scope_filters(&["until", "label"])?;
            set_filters(&mut a, f);
            a.post = Post::Prune;
        }
        Kind::VolumeList => {
            q.only(&["filters"])?;
            a.path = mk_path(m, &["volumes"]);
            let f = scope_filters(&["dangling", "driver", "label", "name"])?;
            set_filters(&mut a, f);
            a.post = Post::ListVolumes;
        }
        Kind::VolumeCreate => {
            q.only(&[])?;
            let (j, raw) = json_body(body, true)?;
            a.raw_body = raw;
            let mut c = if a.raw_body.is_some() {
                j
            } else {
                check(&Ty::Obj(VOLUME_CREATE), &j, "$").map_err(schema_err)?
            };
            let name = c
                .get("Name")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            let mut reserved = false;
            if !name.is_empty() {
                check_new_name("volume", &name)?;
            }
            if let Some(Json::Obj(o)) = c.get("DriverOpts") {
                let pairs: Vec<(String, String)> = o
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect();
                tmpfs_driver_opts(&pairs)?;
            }
            if let Some(l) = c.get("Labels") {
                check_labels(l)?;
            }
            if a.raw_body.is_none() {
                stamp_labels(&mut c, env);
            }
            // Last, so that nothing refused above leaves a reservation behind.
            if !name.is_empty() {
                reserved = env.reserve_volume(&name)?;
            }
            a.path = mk_path(m, &["volumes", "create"]);
            a.body = ReqBody::Json(c.to_bytes());
            a.content_type = Some("application/json".into());
            a.post = Post::VolumeCreate {
                name: (!name.is_empty()).then_some(name),
                reserved,
            };
        }
        Kind::VolumeInspect | Kind::VolumeDelete => {
            q.only(&["force"])?;
            q.bools(&["force"])?;
            let name = volume_name(env, p(0))?;
            a.path = mk_path(m, &["volumes", &name]);
            a.query = q.pass(&["force"]);
            if kind == Kind::VolumeDelete {
                a.post = Post::VolumeDelete { name };
            }
        }
        Kind::VolumePrune => {
            q.only(&["filters"])?;
            a.path = mk_path(m, &["volumes", "prune"]);
            let f = scope_filters(&["all", "label"])?;
            set_filters(&mut a, f);
            a.post = Post::Prune;
        }
    }
    Ok(a)
}

fn exec_check(env: &dyn Env, id: &str) -> Result<(), Refusal> {
    if break_mode("lane-authority-off") {
        return Ok(());
    }
    match env.exec_container(id) {
        Lookup::Own(_) => Ok(()),
        _ => Err(Refusal::not_found(format!("No such exec instance: {id}"))),
    }
}

fn image_name_ok(n: &str) -> Result<(), Refusal> {
    if n.is_empty() || n.len() > 255 {
        return Err(Refusal::forbid("invalid image name"));
    }
    reserved_repo(n)
}

/// B2: commit only into the workspace's own repository namespace, so that a
/// lane cannot retag an image (`busybox:latest`) that every scope builds on.
/// The CLI sends `docker.io/<repo>` for an unqualified name.
fn scope_repo(workspace: &str, repo: &str) -> Result<(), Refusal> {
    if break_mode("commit-any-repo") {
        return Ok(());
    }
    let r = repo.to_ascii_lowercase();
    let r = r.strip_prefix("docker.io/").unwrap_or(&r);
    let prefix = format!("{}/", workspace.to_ascii_lowercase());
    if r.starts_with(&prefix) && r.len() > prefix.len() {
        Ok(())
    } else {
        Err(Refusal::forbid(format!(
            "commit repositories must be under {prefix}"
        )))
    }
}

/// Exporter attributes of the `moby` output that only affect how the image is
/// stored locally. Anything that leaves the engine (`push`, registry options,
/// `dest`, a local or tar exporter) is not in the list.
const MOBY_ATTRS: [&str; 9] = [
    "name",
    "unpack",
    "compression",
    "compression-level",
    "force-compression",
    "oci-mediatypes",
    "name-canonical",
    "dangling-name-prefix",
    "rewrite-timestamp",
];

/// `outputs` is a closed schema: a list of `{"Type":"moby","Attrs":{..}}`, every
/// key spelled exactly, every attribute listed. The text that goes upstream is
/// rebuilt from what was validated.
fn canonical_outputs(raw: &str) -> Result<String, Refusal> {
    let bad = |m: &str| Refusal::forbid(format!("outputs: {m}"));
    let j = parse(raw.as_bytes()).map_err(|e| Refusal::forbid(format!("outputs: {e}")))?;
    let list = match &j {
        Json::Arr(l) => l.clone(),
        Json::Obj(_) => vec![j.clone()],
        _ => return Err(bad("must be a list")),
    };
    if list.len() > 4 {
        return Err(bad("too many outputs"));
    }
    if break_mode("outputs-open") {
        // Defect: only the type is looked at, and the client's text is forwarded.
        for item in &list {
            let ty = item.get("Type").and_then(Json::as_str).unwrap_or("");
            if !matches!(ty, "moby" | "docker") {
                return Err(bad("only the moby exporter is allowed"));
            }
        }
        return Ok(raw.to_string());
    }
    let mut out = Vec::new();
    for item in &list {
        let Json::Obj(fields) = item else {
            return Err(bad("each output must be an object"));
        };
        if fields.iter().any(|(k, _)| k != "Type" && k != "Attrs") {
            return Err(bad("unknown field"));
        }
        if item.get("Type").and_then(Json::as_str) != Some("moby") {
            return Err(bad("only the moby exporter is allowed"));
        }
        let mut attrs = Vec::new();
        match item.get("Attrs") {
            None | Some(Json::Null) => {}
            Some(Json::Obj(a)) => {
                for (k, v) in a {
                    if !MOBY_ATTRS.contains(&k.as_str()) {
                        return Err(bad("exporter attribute is not allowed"));
                    }
                    let Some(v) = v.as_str() else {
                        return Err(bad("exporter attributes are strings"));
                    };
                    attrs.push((k.clone(), string(v)));
                }
            }
            Some(_) => return Err(bad("Attrs must be an object")),
        }
        out.push(Json::Obj(vec![
            ("Type".into(), string("moby")),
            ("Attrs".into(), Json::Obj(attrs)),
        ]));
    }
    Ok(String::from_utf8_lossy(&Json::Arr(out).to_bytes()).into_owned())
}

fn reserved_repo(n: &str) -> Result<(), Refusal> {
    let l = n.to_ascii_lowercase();
    let last = l.rsplit('/').next().unwrap_or("");
    if l.starts_with("marsh/")
        || l.starts_with("marsh-")
        || l.starts_with("nf-")
        || last.starts_with("marsh-")
        || last.starts_with("nf-")
    {
        return Err(Refusal::forbid(
            "image names starting with marsh or nf- are reserved",
        ));
    }
    Ok(())
}

fn stream_content_type(head: &Head) -> Result<String, Refusal> {
    match content_type(head)? {
        Some(ct) => {
            let base = ct.split(';').next().unwrap_or("").trim().to_string();
            if TAR_TYPES.contains(&base.as_str()) {
                Ok(base)
            } else {
                Err(Refusal::forbid(
                    "unsupported Content-Type for an archive body",
                ))
            }
        }
        None => Ok("application/x-tar".into()),
    }
}

/// Validate what the container's limits would be *after* the update: the
/// fields the patch leaves out keep their current values, so a partial update
/// (only `CpuPeriod`, say) is checked against the quota already there. A value
/// that means "unlimited" is refused when the client asks for it and reset to
/// the default when the container already has none.
fn update_limits(env: &dyn Env, id: &str, c: &mut Json) -> Result<(), Refusal> {
    let l = env.limits();
    let off = break_mode("services-resource-limit-off");
    let get = |c: &Json, k: &str| c.get(k).and_then(Json::as_i64).unwrap_or(0);
    // Explicit unlimited encodings. (`null` is "unchanged" and reads as 0 here
    // only because `as_i64` is `None`; an explicit 0 or -1 is not.)
    if !off {
        if matches!(c.get("PidsLimit"), Some(Json::Num(_))) && get(c, "PidsLimit") <= 0 {
            return Err(Refusal::forbid("an unlimited PidsLimit is not allowed"));
        }
        if get(c, "MemorySwap") < 0 {
            return Err(Refusal::forbid("unlimited swap is not allowed"));
        }
    }
    let legacy = break_mode("update-limits-unmerged");
    let cur = if legacy {
        crate::policy::CurrentLimits::default()
    } else {
        env.current_limits(id)?
    };
    let pick = |patch: i64, now: i64| if patch > 0 { patch } else { now };
    let memory = pick(get(c, "Memory"), cur.memory);
    let swap = pick(get(c, "MemorySwap"), cur.memory_swap);
    let reservation = get(c, "MemoryReservation");
    let nano = pick(get(c, "NanoCpus"), cur.nano_cpus);
    let quota = pick(get(c, "CpuQuota"), cur.cpu_quota);
    let period = pick(get(c, "CpuPeriod"), cur.cpu_period);
    let pids = pick(get(c, "PidsLimit"), cur.pids);
    let over = |what: &str| Refusal::forbid(format!("{what} is above the project maximum"));
    if memory > l.max_memory {
        return Err(over("Memory"));
    }
    if swap > l.max_memory {
        return Err(over("MemorySwap"));
    }
    if reservation > l.max_memory {
        return Err(over("MemoryReservation"));
    }
    if nano > l.max_nano_cpus {
        return Err(over("NanoCpus"));
    }
    if pids > l.max_pids {
        return Err(over("PidsLimit"));
    }
    if quota > 0 {
        let period = if period > 0 { period } else { 100_000 };
        if i128::from(quota) * 1_000_000_000 / i128::from(period) > i128::from(l.max_nano_cpus) {
            return Err(over("The CPU quota"));
        }
    }
    if legacy || off {
        return Ok(());
    }
    // Reset what is unlimited now to the defaults.
    if memory <= 0 {
        c.set("Memory", json::int(l.default_memory));
    }
    if pids <= 0 {
        c.set("PidsLimit", json::int(l.default_pids));
    }
    if nano <= 0 && quota <= 0 {
        c.set("NanoCpus", json::int(l.default_nano_cpus));
    }
    Ok(())
}

fn build(
    q: &Q<'_>,
    m: &RouteMatch,
    head: &Head,
    env: &dyn Env,
    a: &mut Admitted,
) -> Result<(), Refusal> {
    const KEYS: &[&str] = &[
        "dockerfile",
        "t",
        "q",
        "nocache",
        "pull",
        "rm",
        "forcerm",
        "buildargs",
        "labels",
        "target",
        "platform",
        "networkmode",
        "cachefrom",
        "extrahosts",
        "version",
        "session",
        "buildid",
        "outputs",
        "shmsize",
        "memory",
        "memswap",
        "cpushares",
        "cpusetcpus",
        "cpuperiod",
        "cpuquota",
        "squash",
        "ulimits",
    ];
    q.only(KEYS)?;
    q.bools(&["q", "nocache", "rm", "forcerm", "squash"])?;
    q.ints(&[
        "shmsize",
        "memory",
        "memswap",
        "cpushares",
        "cpuperiod",
        "cpuquota",
    ])?;
    q.token("dockerfile", 256)?;
    q.token("target", 128)?;
    q.token("platform", 128)?;
    q.token("session", 128)?;
    q.token("buildid", 128)?;
    if let Some(p) = q.get("pull")
        && p.len() > 32
    {
        return Err(Refusal::forbid("invalid pull parameter"));
    }
    match q.get("networkmode") {
        None | Some("" | "default" | "none") => {}
        Some(_) => {
            return Err(Refusal::forbid(
                "build network modes other than default and none are not allowed",
            ));
        }
    }
    match q.get("version") {
        None | Some("1" | "2") => {}
        Some(_) => return Err(Refusal::forbid("unsupported build version")),
    }
    if q.get("version") != Some("2") {
        // Legacy builder: no cgroup, isolation or resource escape hatches.
        for k in [
            "cpusetcpus",
            "cpuperiod",
            "cpuquota",
            "cpushares",
            "memory",
            "memswap",
            "shmsize",
            "ulimits",
            "squash",
        ] {
            if q.get(k)
                .is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
            {
                return Err(Refusal::forbid(format!(
                    "legacy build option {k} is not allowed"
                )));
            }
        }
    }
    for t in m.query.iter().filter(|(k, _)| k == "t") {
        if t.1.len() > 255 {
            return Err(Refusal::forbid("invalid tag"));
        }
        reserved_repo(&t.1)?;
    }
    if let Some(l) = q.get("labels") {
        let j = parse(l.as_bytes()).map_err(|e| Refusal::forbid(format!("labels: {e}")))?;
        check(&Ty::StrMap(&Ty::Str), &j, "labels").map_err(schema_err)?;
        check_labels(&j)?;
    }
    if let Some(b) = q.get("buildargs") {
        let j = parse(b.as_bytes()).map_err(|e| Refusal::forbid(format!("buildargs: {e}")))?;
        check(&Ty::StrMap(&Ty::Str), &j, "buildargs").map_err(schema_err)?;
        if let Some(syntax) = j.get("BUILDKIT_SYNTAX").and_then(Json::as_str)
            && !env.registry_allowed(syntax)
        {
            return Err(Refusal::forbid(
                "the build frontend image registry is not allowed by policy",
            ));
        }
    }
    if let Some(c) = q.get("cachefrom") {
        let j = parse(c.as_bytes()).map_err(|e| Refusal::forbid(format!("cachefrom: {e}")))?;
        check(&Ty::Arr(&Ty::Str), &j, "cachefrom").map_err(schema_err)?;
    }
    let outputs = match q.get("outputs") {
        Some(o) => Some(canonical_outputs(o)?),
        None => None,
    };
    if let Some(x) = q.get("extrahosts")
        && x.len() > 2048
    {
        return Err(Refusal::forbid("invalid extrahosts"));
    }
    a.path = mk_path(m, &["build"]);
    a.query = q.pass(KEYS);
    if let Some(c) = outputs {
        // The validated, rebuilt value goes upstream, not the client's text.
        for (k, v) in &mut a.query {
            if k == "outputs" {
                v.clone_from(&c);
            }
        }
    }
    a.body = ReqBody::Stream(head.framing);
    a.content_type = Some(stream_content_type(head)?);
    Ok(())
}
