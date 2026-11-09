//! Container create: the semantic pass behind the closed schema (B2, A3-A5).

use super::json::{Json, int, string};
use super::{Env, Lookup, PinRequest, Refusal};
use crate::breaks::break_mode;

pub const SOCKET_PATHS: [&str; 2] = ["/var/run/docker.sock", "/run/docker.sock"];

/// Releases pins created for a request that is then refused or fails.
pub struct PinGuard<'a> {
    env: &'a dyn Env,
    /// Pins this request made; released if it fails.
    pub ids: Vec<String>,
    /// Pins of other containers this one now depends on (`VolumesFrom`): they
    /// join its lease, but a failed request does not own them.
    pub inherited: Vec<String>,
    armed: bool,
}

impl<'a> PinGuard<'a> {
    #[must_use]
    pub fn new(env: &'a dyn Env) -> Self {
        Self {
            env,
            ids: Vec::new(),
            inherited: Vec::new(),
            armed: true,
        }
    }

    /// The request is admitted; the caller now owns the pins.
    pub fn disarm(&mut self) -> Vec<String> {
        self.armed = false;
        std::mem::take(&mut self.ids)
    }
}

impl Drop for PinGuard<'_> {
    fn drop(&mut self) {
        if self.armed && !self.ids.is_empty() {
            self.env.release_pins(&self.ids);
        }
    }
}

#[must_use]
pub fn simple_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// `marsh-*` and `nf-*` are reserved names (Opus F5/F11).
#[must_use]
pub fn reserved_name(s: &str) -> bool {
    let l = s.trim_start_matches('/').to_ascii_lowercase();
    l.starts_with("marsh-") || l.starts_with("nf-") || l == "marsh" || l.starts_with("marsh/")
}

pub fn check_new_name(kind: &str, s: &str) -> Result<(), Refusal> {
    if !simple_name(s) {
        return Err(Refusal::forbid(format!("invalid {kind} name")));
    }
    if reserved_name(s) {
        return Err(Refusal::forbid(format!(
            "{kind} names starting with marsh- or nf- are reserved"
        )));
    }
    // Docker resolves a full ID before a name, so a name that is another
    // object's full ID would reach that object.
    if s.len() == 64
        && s.bytes().all(|b| b.is_ascii_hexdigit())
        && !break_mode("fullid-name-allowed")
    {
        return Err(Refusal::forbid(format!(
            "{kind} names that look like a full ID are not allowed"
        )));
    }
    Ok(())
}

/// Reserved labels are set by the agent only (A4).
pub fn check_labels(labels: &Json) -> Result<(), Refusal> {
    if let Json::Obj(m) = labels {
        for (k, _) in m {
            if k.to_ascii_lowercase().starts_with("dev.marsh.")
                && !break_mode("reserved-labels-allowed")
            {
                return Err(Refusal::forbid(
                    "labels starting with dev.marsh. are reserved",
                ));
            }
        }
    }
    Ok(())
}

/// Add the scope labels to a `Labels` member of `obj` (creating it).
pub fn stamp_labels(obj: &mut Json, env: &dyn Env) {
    let a = env.authority();
    let mut pairs = match obj.get("Labels") {
        Some(Json::Obj(m)) => m.clone(),
        _ => Vec::new(),
    };
    for (k, v) in [
        ("dev.marsh.scope", &a.workspace),
        ("dev.marsh.attempt", &a.attempt),
    ] {
        if break_mode("reserved-labels-allowed") && pairs.iter().any(|(ek, _)| ek == k) {
            continue;
        }
        pairs.retain(|(ek, _)| ek != k);
        pairs.push((k.to_string(), string(v)));
    }
    obj.set("Labels", Json::Obj(pairs));
}

fn abs_container_path(p: &str) -> bool {
    p.starts_with('/') && p.len() <= 4096 && !p.bytes().any(|b| b < 0x20 || b == 0x7f)
}

fn bind_opts_ok(opts: &str) -> Result<(bool, Vec<String>), Refusal> {
    let mut ro = false;
    let mut out = Vec::new();
    for o in opts.split(',') {
        match o {
            "ro" => ro = true,
            "rw" | "z" | "Z" | "nocopy" | "delegated" | "cached" | "consistent" | "rprivate" => {}
            "private" if break_mode("private-propagation-allowed") => {}
            "" => return Err(Refusal::forbid("empty bind option")),
            other => {
                return Err(Refusal::forbid(format!(
                    "bind option {:?} is not allowed (only ro, rw, z, Z, nocopy, rprivate)",
                    other
                        .chars()
                        .filter(char::is_ascii_graphic)
                        .take(20)
                        .collect::<String>()
                )));
            }
        }
        out.push(o.to_string());
    }
    Ok((ro, out))
}

/// Resolve one bind source into what dockerd must mount. `Ok(None)` for named
/// and anonymous volumes (nothing to rewrite).
fn bind_source(
    env: &dyn Env,
    guard: &mut PinGuard<'_>,
    src: &str,
    read_only: bool,
    create_missing: bool,
) -> Result<Option<(String, bool)>, Refusal> {
    if SOCKET_PATHS.contains(&src) || env.socket_aliases().iter().any(|a| a == src) {
        if break_mode("socket-refuse") {
            return Err(Refusal::forbid("the Docker socket cannot be mounted"));
        }
        if break_mode("socket-substitute-raw") {
            return Ok(Some((src.to_string(), false)));
        }
        return Ok(Some((env.substituted_socket()?, false)));
    }
    if src.starts_with('/') {
        if src.len() > 4096 || src.bytes().any(|b| b < 0x20 || b == 0x7f) {
            return Err(Refusal::forbid("invalid bind source"));
        }
        let pin = env.pin_bind(&PinRequest {
            source: src.to_string(),
            read_only,
            create_missing,
        })?;
        if let Some(id) = &pin.pin_id {
            guard.ids.push(id.clone());
        }
        return Ok(Some((pin.path, pin.force_read_only)));
    }
    named_volume(env, src)?;
    Ok(None)
}

fn named_volume(env: &dyn Env, name: &str) -> Result<(), Refusal> {
    check_new_name("volume", name)?;
    // One atomic step: claim the name, look at what the engine has, create it
    // with the scope's labels. A name another scope holds is refused.
    env.ensure_volume(name)
}

fn rewrite_bind(env: &dyn Env, guard: &mut PinGuard<'_>, s: &str) -> Result<String, Refusal> {
    let parts: Vec<&str> = s.split(':').collect();
    let (src, dst, opts) = match parts.as_slice() {
        [dst] => {
            // Anonymous volume at dst.
            if !abs_container_path(dst) {
                return Err(Refusal::forbid("invalid bind"));
            }
            return Ok((*s).to_string());
        }
        [src, dst] => (*src, *dst, None),
        [src, dst, opts] => (*src, *dst, Some(*opts)),
        _ => return Err(Refusal::forbid("invalid bind")),
    };
    if !abs_container_path(dst) {
        return Err(Refusal::forbid("bind destination must be an absolute path"));
    }
    let (ro, mut opt_list) = match opts {
        Some(o) => bind_opts_ok(o)?,
        None => (false, Vec::new()),
    };
    let Some((new_src, force_ro)) = bind_source(env, guard, src, ro, true)? else {
        return Ok(s.to_string());
    };
    if force_ro && !ro {
        opt_list.retain(|o| o != "rw");
        opt_list.push("ro".into());
    }
    let mut out = format!("{new_src}:{dst}");
    if !opt_list.is_empty() {
        out.push(':');
        out.push_str(&opt_list.join(","));
    }
    Ok(out)
}

fn rewrite_mount(env: &dyn Env, guard: &mut PinGuard<'_>, m: &mut Json) -> Result<(), Refusal> {
    let ty = m
        .get("Type")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let target = m
        .get("Target")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    if !abs_container_path(&target) {
        return Err(Refusal::forbid("mount target must be an absolute path"));
    }
    match ty.as_str() {
        "bind" => {
            let src = m
                .get("Source")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            if !src.starts_with('/') {
                return Err(Refusal::forbid(
                    "bind mount source must be an absolute path",
                ));
            }
            if let Some(bo) = m.get("BindOptions")
                && !bo.is_null()
            {
                if matches!(bo.get("CreateMountpoint"), Some(Json::Bool(true))) {
                    return Err(Refusal::forbid(
                        "bind mounts may not create their source (CreateMountpoint)",
                    ));
                }
                // Docker's `bind-recursive=disabled`: a plain bind drops the
                // read-only `.git`/`.marsh` sub-mounts of a project pin.
                if matches!(bo.get("NonRecursive"), Some(Json::Bool(true)))
                    && !break_mode("bind-nonrecursive-allowed")
                {
                    return Err(Refusal::forbid(
                        "non-recursive bind mounts are not allowed (they would drop the read-only .git and .marsh)",
                    ));
                }
                match bo.get("Propagation").and_then(Json::as_str) {
                    None | Some("" | "rprivate") => {}
                    Some("private") if break_mode("private-propagation-allowed") => {}
                    Some(_) => {
                        return Err(Refusal::forbid(
                            "bind propagation other than rprivate is not allowed",
                        ));
                    }
                }
            }
            let ro = matches!(m.get("ReadOnly"), Some(Json::Bool(true)));
            // `Mounts` never creates a missing source; Docker errors instead.
            let (new_src, force_ro) = bind_source(env, guard, &src, ro, false)?
                .ok_or_else(|| Refusal::forbid("invalid bind mount"))?;
            m.set("Source", string(&new_src));
            if force_ro {
                m.set("ReadOnly", Json::Bool(true));
            }
        }
        "volume" => {
            let src = m
                .get("Source")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string();
            if !src.is_empty() {
                named_volume(env, &src)?;
            }
            if let Some(vo) = m.get("VolumeOptions")
                && !vo.is_null()
            {
                if let Some(l) = vo.get("Labels") {
                    check_labels(l)?;
                }
                if let Some(dc) = vo.get("DriverConfig")
                    && !dc.is_null()
                {
                    driver_config(dc)?;
                }
            }
        }
        "tmpfs" => {
            if let Some(to) = m.get("TmpfsOptions")
                && let Some(Json::Arr(opts)) = to.get("Options")
            {
                for pair in opts {
                    let Json::Arr(kv) = pair else {
                        return Err(Refusal::forbid("invalid tmpfs options"));
                    };
                    let k = kv.first().and_then(Json::as_str).unwrap_or("");
                    let v = kv.get(1).and_then(Json::as_str);
                    tmpfs_option(k, v)?;
                }
            }
        }
        _ => return Err(Refusal::forbid("mount type is not allowed")),
    }
    Ok(())
}

fn tmpfs_option(k: &str, v: Option<&str>) -> Result<(), Refusal> {
    let ok = match (k, v) {
        ("ro" | "rw" | "noexec" | "exec" | "nosuid" | "suid" | "nodev" | "dev", None) => true,
        ("size", Some(v)) => size_ok(v),
        ("mode", Some(v)) => {
            !v.is_empty() && v.len() <= 5 && v.bytes().all(|b| (b'0'..=b'7').contains(&b))
        }
        ("uid" | "gid" | "nr_inodes", Some(v)) => {
            !v.is_empty() && v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(Refusal::forbid(format!(
            "tmpfs option {:?} is not allowed",
            k.chars()
                .filter(char::is_ascii_graphic)
                .take(20)
                .collect::<String>()
        )))
    }
}

fn size_ok(v: &str) -> bool {
    let digits = v.trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G']);
    !digits.is_empty()
        && digits.len() <= 12
        && digits.bytes().all(|b| b.is_ascii_digit())
        && v.len() - digits.len() <= 1
}

/// Local-volume driver options: only `type=tmpfs` (A3).
pub fn tmpfs_driver_opts(opts: &[(String, String)]) -> Result<(), Refusal> {
    for (k, v) in opts {
        match k.as_str() {
            "type" | "device" if v == "tmpfs" => {}
            "o" => {
                for item in v.split(',') {
                    let (ik, iv) = item.split_once('=').ok_or_else(|| {
                        Refusal::forbid("volume option is not allowed (only size, mode, uid)")
                    })?;
                    match ik {
                        "size" if size_ok(iv) => {}
                        "mode"
                            if !iv.is_empty()
                                && iv.len() <= 5
                                && iv.bytes().all(|b| (b'0'..=b'7').contains(&b)) => {}
                        "uid"
                            if !iv.is_empty()
                                && iv.len() <= 10
                                && iv.bytes().all(|b| b.is_ascii_digit()) => {}
                        _ => {
                            return Err(Refusal::forbid(
                                "volume option is not allowed (only size, mode, uid)",
                            ));
                        }
                    }
                }
            }
            _ => {
                return Err(Refusal::forbid(
                    "local volume options other than type=tmpfs are not allowed",
                ));
            }
        }
    }
    // A tmpfs volume must say so; options without it would be a plain volume.
    if !opts.is_empty() && !opts.iter().any(|(k, v)| k == "type" && v == "tmpfs") {
        return Err(Refusal::forbid(
            "local volume options other than type=tmpfs are not allowed",
        ));
    }
    Ok(())
}

pub fn driver_config(dc: &Json) -> Result<(), Refusal> {
    match dc.get("Name").and_then(Json::as_str) {
        None | Some("" | "local") => {}
        Some(_) => {
            return Err(Refusal::forbid(
                "volume drivers other than local are not allowed",
            ));
        }
    }
    if let Some(Json::Obj(o)) = dc.get("Options") {
        let pairs: Vec<(String, String)> = o
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect();
        tmpfs_driver_opts(&pairs)?;
    }
    Ok(())
}

fn loopback_rewrite(hc: &mut Json) {
    if break_mode("publish-nonloopback") {
        return;
    }
    if let Some(Json::Obj(pb)) = hc.get_mut("PortBindings") {
        for (_, v) in pb.iter_mut() {
            if let Json::Arr(list) = v {
                for b in list.iter_mut() {
                    let ip = b.get("HostIp").and_then(Json::as_str).unwrap_or("");
                    let keep = ip
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|a| a.is_loopback());
                    if !keep {
                        b.set("HostIp", string("127.0.0.1"));
                    }
                }
            }
        }
    }
}

fn port_binding_keys_ok(hc: &Json) -> Result<(), Refusal> {
    if let Some(Json::Obj(pb)) = hc.get("PortBindings") {
        for (k, _) in pb {
            // `5432` is `5432/tcp` (the engine's own default); dockerode and so
            // Testcontainers send it that way.
            let (ports, proto) = k.split_once('/').unwrap_or((k.as_str(), "tcp"));
            let ok = matches!(proto, "tcp" | "udp" | "sctp")
                && !ports.is_empty()
                && ports.split('-').count() <= 2
                && ports.split('-').all(|p| {
                    !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())
                });
            if !ok {
                return Err(Refusal::forbid("invalid port binding"));
            }
        }
    }
    Ok(())
}

fn container_ref(env: &dyn Env, what: &str) -> Result<String, Refusal> {
    if !simple_name(what) {
        return Err(Refusal::forbid("invalid container reference"));
    }
    if break_mode("lane-authority-off") {
        return Ok(what.to_string());
    }
    match env.container(what) {
        Lookup::Own(id) => Ok(id),
        Lookup::Missing | Lookup::Ambiguous => Err(Refusal::forbid(format!(
            "container {what:?} is not available to this workspace"
        ))),
    }
}

fn set_limit(hc: &mut Json, key: &str, default: i64, max: i64, off: bool) -> Result<(), Refusal> {
    let cur = hc.get(key).and_then(Json::as_i64).unwrap_or(0);
    if cur > max {
        return Err(Refusal::forbid(format!(
            "{key} is above the project maximum"
        )));
    }
    if cur <= 0 && !off {
        hc.set(key, int(default));
    }
    Ok(())
}

fn limits(env: &dyn Env, hc: &mut Json) -> Result<(), Refusal> {
    let l = env.limits();
    let off = break_mode("services-resource-limit-off");
    if hc
        .get("MemorySwap")
        .and_then(Json::as_i64)
        .is_some_and(|v| v < 0)
        && !off
    {
        return Err(Refusal::forbid("unlimited swap is not allowed"));
    }
    for key in ["MemorySwap", "MemoryReservation"] {
        if hc
            .get(key)
            .and_then(Json::as_i64)
            .is_some_and(|v| v > l.max_memory)
        {
            return Err(Refusal::forbid(format!(
                "{key} is above the project maximum"
            )));
        }
    }
    set_limit(hc, "Memory", l.default_memory, l.max_memory, off)?;
    let quota = hc.get("CpuQuota").and_then(Json::as_i64).unwrap_or(0);
    if quota > 0 {
        let period = hc
            .get("CpuPeriod")
            .and_then(Json::as_i64)
            .filter(|p| *p > 0)
            .unwrap_or(100_000);
        if i128::from(quota) * 1_000_000_000 / i128::from(period) > i128::from(l.max_nano_cpus) {
            return Err(Refusal::forbid("CpuQuota is above the project maximum"));
        }
    }
    if quota <= 0 {
        set_limit(hc, "NanoCpus", l.default_nano_cpus, l.max_nano_cpus, off)?;
    } else if hc.get("NanoCpus").and_then(Json::as_i64).unwrap_or(0) > l.max_nano_cpus {
        return Err(Refusal::forbid("NanoCpus is above the project maximum"));
    }
    let pids = hc.get("PidsLimit").and_then(Json::as_i64).unwrap_or(0);
    if pids > l.max_pids {
        return Err(Refusal::forbid("PidsLimit is above the project maximum"));
    }
    if pids <= 0 && !off {
        hc.set("PidsLimit", int(l.default_pids));
    }
    if hc.get("ShmSize").and_then(Json::as_i64).unwrap_or(0) > l.max_shm {
        return Err(Refusal::forbid("ShmSize is above the project maximum"));
    }
    Ok(())
}

fn network_mode(env: &dyn Env, hc: &mut Json) -> Result<(), Refusal> {
    let mode = hc
        .get("NetworkMode")
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    let new = match mode.as_str() {
        "" | "default" => scope_network_id(env)?,
        "bridge" => {
            return Err(Refusal::forbid(
                "Docker's default bridge network is not available; omit --network or use a network of your own",
            ));
        }
        "host" => return Err(Refusal::forbid("host networking is not allowed")),
        "none" => "none".to_string(),
        m if m.starts_with("container:") => format!(
            "container:{}",
            container_ref(env, &m["container:".len()..])?
        ),
        name => resolve_network(env, name)?,
    };
    hc.set("NetworkMode", string(&new));
    Ok(())
}

/// The scope's own default network, by ID.
fn scope_network_id(env: &dyn Env) -> Result<String, Refusal> {
    if break_mode("scope-default-network-off") {
        // Defect: every scope's containers share Docker's default bridge.
        return Ok("bridge".to_string());
    }
    let name = env.scope_network()?;
    resolve_network(env, &name)
}

/// The caller's network, by its full ID: Docker resolves an exact ID before a
/// name, and the ledger is what says whose it is.
pub fn resolve_network(env: &dyn Env, name: &str) -> Result<String, Refusal> {
    if !simple_name(name) {
        return Err(Refusal::forbid("invalid network name"));
    }
    if break_mode("lane-authority-off") {
        return Ok(name.to_string());
    }
    match env.network(name) {
        // Defect: the name goes to the engine, which resolves an exact ID first.
        Lookup::Own(_) if break_mode("network-name-forwarded") => Ok(name.to_string()),
        Lookup::Own(id) => Ok(id),
        _ => Err(Refusal::not_found(format!("network {name} not found"))),
    }
}

fn namespace_mode(env: &dyn Env, hc: &mut Json, key: &str, plain: &[&str]) -> Result<(), Refusal> {
    let v = hc.get(key).and_then(Json::as_str).unwrap_or("").to_string();
    if plain.contains(&v.as_str()) {
        return Ok(());
    }
    if let Some(r) = v.strip_prefix("container:") {
        let full = container_ref(env, r)?;
        hc.set(key, string(&format!("container:{full}")));
        return Ok(());
    }
    Err(Refusal::forbid(format!(
        "{key} {:?} is not allowed",
        v.chars()
            .filter(char::is_ascii_graphic)
            .take(24)
            .collect::<String>()
    )))
}

fn security_opts(hc: &Json) -> Result<(), Refusal> {
    if let Some(Json::Arr(opts)) = hc.get("SecurityOpt") {
        for o in opts {
            let s = o.as_str().unwrap_or("");
            let ok = matches!(
                s,
                "no-new-privileges"
                    | "no-new-privileges=true"
                    | "no-new-privileges=false"
                    | "no-new-privileges:true"
                    | "no-new-privileges:false"
            ) || ((s.starts_with("label=") || s.starts_with("label:"))
                && s.len() < 128
                && s.bytes().all(|b| b.is_ascii_graphic()));
            if !ok {
                return Err(Refusal::forbid(
                    "security options other than no-new-privileges are not allowed (no custom seccomp, apparmor or systempaths)",
                ));
            }
        }
    }
    Ok(())
}

fn host_config(env: &dyn Env, guard: &mut PinGuard<'_>, hc: &mut Json) -> Result<(), Refusal> {
    if let Some(Json::Arr(binds)) = hc.get("Binds").cloned().as_ref() {
        let mut out = Vec::with_capacity(binds.len());
        for b in binds {
            out.push(string(&rewrite_bind(env, guard, b.as_str().unwrap_or(""))?));
        }
        hc.set("Binds", Json::Arr(out));
    }
    if let Some(Json::Arr(ms)) = hc.get_mut("Mounts") {
        for m in ms.iter_mut() {
            rewrite_mount(env, guard, m)?;
        }
    }
    if let Some(Json::Obj(t)) = hc.get("Tmpfs") {
        for (dst, opts) in t {
            if !abs_container_path(dst) {
                return Err(Refusal::forbid(
                    "tmpfs destination must be an absolute path",
                ));
            }
            for o in opts
                .as_str()
                .unwrap_or("")
                .split(',')
                .filter(|o| !o.is_empty())
            {
                let (k, v) = o.split_once('=').map_or((o, None), |(k, v)| (k, Some(v)));
                tmpfs_option(k, v)?;
            }
        }
    }
    port_binding_keys_ok(hc)?;
    loopback_rewrite(hc);
    security_opts(hc)?;
    if let Some(Json::Obj(sc)) = hc.get("Sysctls")
        && sc.iter().any(|(k, _)| {
            !k.starts_with("net.")
                || !k
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
    {
        return Err(Refusal::forbid("only net.* sysctls are allowed"));
    }
    if let Some(Json::Obj(lc)) = hc
        .get("LogConfig")
        .and_then(|l| l.get("Config"))
        .cloned()
        .as_ref()
    {
        let ok = |k: &str| {
            matches!(
                k,
                "max-size" | "max-file" | "compress" | "mode" | "max-buffer-size"
            )
        };
        if lc.iter().any(|(k, _)| !ok(k)) {
            return Err(Refusal::forbid(
                "log options other than max-size, max-file, compress and mode are not allowed",
            ));
        }
    }
    if let Some(Json::Arr(vf)) = hc.get("VolumesFrom").cloned().as_ref() {
        let mut out = Vec::new();
        for v in vf {
            let s = v.as_str().unwrap_or("");
            let (name, mode) = s.split_once(':').map_or((s, None), |(n, m)| (n, Some(m)));
            if mode.is_some_and(|m| !matches!(m, "ro" | "rw")) {
                return Err(Refusal::forbid("invalid VolumesFrom mode"));
            }
            let full = container_ref(env, name)?;
            if !break_mode("pin-inherit-off") {
                guard.inherited.extend(env.container_pins(&full));
            }
            out.push(string(&match mode {
                Some(m) => format!("{full}:{m}"),
                None => full,
            }));
        }
        hc.set("VolumesFrom", Json::Arr(out));
    }
    if let Some(Json::Arr(links)) = hc.get("Links").cloned().as_ref() {
        let mut out = Vec::new();
        for l in links {
            let s = l.as_str().unwrap_or("");
            let (name, alias) = s.split_once(':').map_or((s, None), |(n, a)| (n, Some(a)));
            let full = container_ref(env, name)?;
            out.push(string(&match alias {
                Some(a) => format!("{full}:{a}"),
                None => full,
            }));
        }
        hc.set("Links", Json::Arr(out));
    }
    namespace_mode(env, hc, "IpcMode", &["", "private", "shareable", "none"])?;
    namespace_mode(env, hc, "PidMode", &[""])?;
    network_mode(env, hc)?;
    limits(env, hc)?;
    Ok(())
}

/// `EndpointsConfig` keys are networks. The CLI names the default network
/// `default` (or `bridge`) when `NetworkMode` is the default; that one becomes
/// the scope's network. Everything else must be a scope network. Keys are
/// rewritten to the full IDs the ledger resolved.
fn networking_config(env: &dyn Env, root: &mut Json, orig_mode: &str) -> Result<(), Refusal> {
    let Some(Json::Obj(eps)) = root
        .get_mut("NetworkingConfig")
        .and_then(|n| n.get_mut("EndpointsConfig"))
    else {
        return Ok(());
    };
    for (name, _) in eps.iter_mut() {
        if matches!(orig_mode, "" | "default") && (name == "default" || name == "bridge") {
            *name = scope_network_id(env)?;
        } else if name == "bridge" || name == "host" || name == "default" {
            return Err(Refusal::forbid(
                "Docker's built-in networks are not available",
            ));
        } else if name != "none" {
            *name = resolve_network(env, name)?;
        }
    }
    Ok(())
}

/// Anonymous volumes (`-v /data`, `Config.Volumes`, image `VOLUME`s) are made
/// by Docker without labels, so they would never show up in the scope's lists
/// or prunes. Make each one an explicit volume mount that carries the scope's
/// reserved labels; Docker then creates it labelled, and skips its own.
fn label_anonymous_volumes(env: &dyn Env, root: &mut Json) {
    if break_mode("anon-volume-unlabeled") {
        return;
    }
    let mut targets: Vec<String> = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    let mut named: Vec<String> = Vec::new();
    if let Some(hc) = root.get("HostConfig") {
        if let Some(Json::Arr(binds)) = hc.get("Binds") {
            for b in binds {
                let s = b.as_str().unwrap_or("");
                let parts: Vec<&str> = s.split(':').collect();
                match parts.as_slice() {
                    [dst] => targets.push((*dst).to_string()),
                    [_, dst, ..] => taken.push((*dst).to_string()),
                    [] => {}
                }
            }
            named.extend(binds.iter().filter_map(|b| b.as_str().map(str::to_string)));
        }
        if let Some(Json::Arr(ms)) = hc.get("Mounts") {
            taken.extend(
                ms.iter()
                    .filter_map(|m| m.get("Target").and_then(Json::as_str).map(str::to_string)),
            );
        }
        if let Some(Json::Obj(t)) = hc.get("Tmpfs") {
            taken.extend(t.iter().map(|(k, _)| k.clone()));
        }
    }
    if let Some(Json::Obj(vs)) = root.get("Volumes") {
        targets.extend(vs.iter().map(|(k, _)| k.clone()));
    }
    if let Some(img) = root.get("Image").and_then(Json::as_str) {
        targets.extend(env.image_volumes(img));
    }
    targets.sort();
    targets.dedup();
    targets.retain(|t| !taken.contains(t));
    if targets.is_empty() {
        return;
    }
    let a = env.authority();
    let mounts: Vec<Json> = targets
        .iter()
        .map(|t| {
            Json::Obj(vec![
                ("Type".into(), string("volume")),
                ("Target".into(), string(t)),
                (
                    "VolumeOptions".into(),
                    Json::Obj(vec![(
                        "Labels".into(),
                        Json::Obj(vec![
                            ("dev.marsh.scope".into(), string(&a.workspace)),
                            ("dev.marsh.attempt".into(), string(&a.attempt)),
                        ]),
                    )]),
                ),
            ])
        })
        .collect();
    let _ = named;
    let Some(hc) = root.get_mut("HostConfig") else {
        return;
    };
    // Single-element binds were the anonymous declarations; drop them.
    if let Some(Json::Arr(binds)) = hc.get_mut("Binds") {
        binds.retain(|b| b.as_str().is_some_and(|s| s.contains(':')));
    }
    let mut all = match hc.get("Mounts") {
        Some(Json::Arr(m)) => m.clone(),
        _ => Vec::new(),
    };
    all.extend(mounts);
    hc.set("Mounts", Json::Arr(all));
}

/// The create route. `root` is schema-canonical. Returns the rewritten body
/// and the pins created for it (owned by the caller from here on).
pub fn admit_create(
    env: &dyn Env,
    mut root: Json,
    guard: &mut PinGuard<'_>,
) -> Result<Json, Refusal> {
    if let Some(l) = root.get("Labels") {
        check_labels(l)?;
    }
    if root
        .get("Image")
        .and_then(Json::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(Refusal::forbid("Image is required"));
    }
    if let Some(img) = root.get("Image").and_then(Json::as_str)
        && !env.registry_allowed(img)
    {
        return Err(Refusal::forbid("image registry is not allowed by policy"));
    }
    if matches!(root.get("HostConfig"), None | Some(Json::Null)) {
        root.set("HostConfig", Json::Obj(Vec::new()));
    }
    let orig_mode = root
        .get("HostConfig")
        .and_then(|h| h.get("NetworkMode"))
        .and_then(Json::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(hc) = root.get_mut("HostConfig") {
        host_config(env, guard, hc)?;
    }
    networking_config(env, &mut root, &orig_mode)?;
    label_anonymous_volumes(env, &mut root);
    stamp_labels(&mut root, env);
    Ok(root)
}
