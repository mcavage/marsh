//! Allowlist schemas for Docker request bodies (B2).

use super::schema::{Field, Ty, f, fnull};

pub const CAP_ADD_ALLOWED: &[&str] = &[
    "NET_BIND_SERVICE",
    "NET_RAW",
    "NET_ADMIN",
    "SYS_NICE",
    "SYS_PTRACE",
];

fn cap_add_ok(s: &str) -> bool {
    let n = s.strip_prefix("CAP_").unwrap_or(s);
    CAP_ADD_ALLOWED.contains(&n)
}

fn cap_drop_ok(s: &str) -> bool {
    let n = s.strip_prefix("CAP_").unwrap_or(s);
    !n.is_empty() && n.len() < 32 && n.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
}

fn host_port_ok(s: &str) -> bool {
    s.is_empty()
        || (s.split('-').count() <= 2
            && s.split('-')
                .all(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())))
}

fn host_ip_ok(s: &str) -> bool {
    s.is_empty() || s.parse::<std::net::IpAddr>().is_ok()
}

fn mac_ok(s: &str) -> bool {
    s.is_empty() || (s.len() == 17 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b':'))
}

fn image_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/:@".contains(&b))
}

fn log_driver_ok(s: &str) -> bool {
    matches!(s, "" | "json-file" | "local" | "none")
}

fn str_nonempty(s: &str) -> bool {
    !s.is_empty()
}

fn ulimit_ok(s: &str) -> bool {
    matches!(
        s,
        "core"
            | "cpu"
            | "data"
            | "fsize"
            | "locks"
            | "memlock"
            | "msgqueue"
            | "nice"
            | "nofile"
            | "nproc"
            | "rss"
            | "rtprio"
            | "rttime"
            | "sigpending"
            | "stack"
    )
}

pub static HEALTHCHECK: &[Field] = &[
    fnull("Test", Ty::Arr(&Ty::Str)),
    f("Interval", Ty::Int(0, i64::MAX)),
    f("Timeout", Ty::Int(0, i64::MAX)),
    f("StartPeriod", Ty::Int(0, i64::MAX)),
    f("StartInterval", Ty::Int(0, i64::MAX)),
    f("Retries", Ty::Int(0, 1_000_000)),
];

static STR_ARR: Ty = Ty::Arr(&Ty::Str);

static PORT_BINDING: &[Field] = &[
    f("HostIp", Ty::StrIf(host_ip_ok, "an IP address")),
    f("HostPort", Ty::StrIf(host_port_ok, "a port")),
];
static PORT_BINDING_ARR: Ty = Ty::Arr(&Ty::Obj(PORT_BINDING));

static RESTART: &[Field] = &[
    f(
        "Name",
        Ty::StrEnum(&["", "no", "always", "unless-stopped", "on-failure"]),
    ),
    f("MaximumRetryCount", Ty::Int(0, 1_000_000)),
];

static LOG_CONFIG: &[Field] = &[
    f("Type", Ty::StrIf(log_driver_ok, "log driver")),
    fnull("Config", Ty::StrMap(&Ty::Str)),
];

static ULIMIT: &[Field] = &[
    f("Name", Ty::StrIf(ulimit_ok, "ulimit name")),
    f("Hard", Ty::Int(-1, i64::MAX)),
    f("Soft", Ty::Int(-1, i64::MAX)),
];

static BIND_OPTIONS: &[Field] = &[
    f("Propagation", Ty::Str),
    f("NonRecursive", Ty::Bool),
    f("CreateMountpoint", Ty::Bool),
    f("ReadOnlyNonRecursive", Ty::Bool),
    f("ReadOnlyForceRecursive", Ty::Bool),
];
static DRIVER_CONFIG: &[Field] = &[f("Name", Ty::Str), fnull("Options", Ty::StrMap(&Ty::Str))];
static VOLUME_OPTIONS: &[Field] = &[
    f("NoCopy", Ty::Bool),
    fnull("Labels", Ty::StrMap(&Ty::Str)),
    fnull("DriverConfig", Ty::Obj(DRIVER_CONFIG)),
    f("Subpath", Ty::Zero),
];
static TMPFS_OPTIONS: &[Field] = &[
    f("SizeBytes", Ty::Int(0, i64::MAX)),
    f("Mode", Ty::Int(0, 0o7777)),
    fnull("Options", Ty::Arr(&STR_ARR)),
];
pub static MOUNT: &[Field] = &[
    f("Type", Ty::StrEnum(&["bind", "volume", "tmpfs"])),
    f("Source", Ty::Str),
    f("Target", Ty::StrIf(str_nonempty, "non-empty")),
    f("ReadOnly", Ty::Bool),
    f(
        "Consistency",
        Ty::StrEnum(&["", "default", "consistent", "cached", "delegated"]),
    ),
    fnull("BindOptions", Ty::Obj(BIND_OPTIONS)),
    fnull("VolumeOptions", Ty::Obj(VOLUME_OPTIONS)),
    fnull("TmpfsOptions", Ty::Obj(TMPFS_OPTIONS)),
];

static ENDPOINT: &[Field] = &[
    f("GwPriority", Ty::Zero),
    fnull("DNSNames", Ty::Arr(&Ty::Str)),
    fnull("IPAMConfig", Ty::Obj(IPAM_ENDPOINT)),
    fnull("Links", Ty::Zero),
    fnull("Aliases", Ty::Arr(&Ty::Str)),
    f("MacAddress", Ty::StrIf(mac_ok, "a MAC address")),
    fnull("DriverOpts", Ty::Zero),
    f("NetworkID", Ty::Zero),
    f("EndpointID", Ty::Zero),
    f("Gateway", Ty::Zero),
    f("IPAddress", Ty::Zero),
    f("IPPrefixLen", Ty::Zero),
    f("IPv6Gateway", Ty::Zero),
    f("GlobalIPv6Address", Ty::Zero),
    f("GlobalIPv6PrefixLen", Ty::Zero),
];
static IPAM_ENDPOINT: &[Field] = &[
    f("IPv4Address", Ty::Str),
    f("IPv6Address", Ty::Str),
    fnull("LinkLocalIPs", Ty::Arr(&Ty::Str)),
];
pub static ENDPOINT_CONFIG: Ty = Ty::Obj(ENDPOINT);

static NETWORKING: &[Field] = &[fnull("EndpointsConfig", Ty::StrMap(&ENDPOINT_CONFIG))];

pub static HOST_CONFIG: &[Field] = &[
    fnull("Binds", Ty::Arr(&Ty::Str)),
    fnull("Mounts", Ty::Arr(&Ty::Obj(MOUNT))),
    fnull("Tmpfs", Ty::StrMap(&Ty::Str)),
    fnull("PortBindings", Ty::StrMap(&PORT_BINDING_ARR)),
    f("RestartPolicy", Ty::Obj(RESTART)),
    fnull(
        "CapAdd",
        Ty::Arr(&Ty::StrIf(cap_add_ok, "capability is not in the allowlist")),
    ),
    fnull(
        "CapDrop",
        Ty::Arr(&Ty::StrIf(cap_drop_ok, "capability name")),
    ),
    f("Memory", Ty::Int(0, i64::MAX)),
    f("MemorySwap", Ty::Int(-1, i64::MAX)),
    f("MemoryReservation", Ty::Int(0, i64::MAX)),
    fnull("MemorySwappiness", Ty::Int(-1, 100)),
    f("NanoCpus", Ty::Int(0, i64::MAX)),
    f("CpuShares", Ty::Int(0, 1_000_000)),
    f("CpuPeriod", Ty::Int(0, 1_000_000)),
    f("CpuQuota", Ty::Int(0, i64::MAX)),
    f("CpusetCpus", Ty::StrIf(cpuset_ok, "cpu list")),
    f("CpusetMems", Ty::StrIf(cpuset_ok, "memory node list")),
    fnull("PidsLimit", Ty::Int(-1, i64::MAX)),
    f("BlkioWeight", Ty::Int(0, 1000)),
    fnull("Ulimits", Ty::Arr(&Ty::Obj(ULIMIT))),
    f("ShmSize", Ty::Int(0, i64::MAX)),
    fnull("Init", Ty::Bool),
    fnull("ExtraHosts", Ty::Arr(&Ty::Str)),
    f("NetworkMode", Ty::Str),
    fnull("Dns", Ty::Arr(&Ty::Str)),
    fnull("DnsOptions", Ty::Arr(&Ty::Str)),
    fnull("DnsSearch", Ty::Arr(&Ty::Str)),
    f("AutoRemove", Ty::Bool),
    f("ReadonlyRootfs", Ty::Bool),
    f("PublishAllPorts", Ty::Bool),
    fnull("Sysctls", Ty::StrMap(&Ty::Str)),
    fnull("SecurityOpt", Ty::Arr(&Ty::Str)),
    fnull("GroupAdd", Ty::Arr(&Ty::Str)),
    fnull("VolumesFrom", Ty::Arr(&Ty::Str)),
    fnull("Links", Ty::Arr(&Ty::Str)),
    f("IpcMode", Ty::Str),
    f("PidMode", Ty::Str),
    f("UTSMode", Ty::Zero),
    f("UsernsMode", Ty::Zero),
    f("CgroupnsMode", Ty::StrEnum(&["", "private"])),
    f("Isolation", Ty::StrEnum(&["", "default"])),
    f("Runtime", Ty::StrEnum(&["", "runc"])),
    f("VolumeDriver", Ty::StrEnum(&["", "local"])),
    f("OomScoreAdj", Ty::Int(0, 1000)),
    f("LogConfig", Ty::Obj(LOG_CONFIG)),
    fnull("ConsoleSize", Ty::Arr(&Ty::Int(0, i64::MAX))),
    f("ContainerIDFile", Ty::Zero),
    f("Cgroup", Ty::Zero),
    f("CgroupParent", Ty::Zero),
    fnull("Devices", Ty::Zero),
    fnull("DeviceCgroupRules", Ty::Zero),
    fnull("DeviceRequests", Ty::Zero),
    fnull("MaskedPaths", Ty::Null),
    fnull("ReadonlyPaths", Ty::Null),
    fnull("Capabilities", Ty::Zero),
    fnull("Annotations", Ty::Zero),
    fnull("StorageOpt", Ty::Zero),
    f("OomKillDisable", Ty::False),
    f("Privileged", Ty::False),
    f("CpuCount", Ty::Zero),
    f("CpuPercent", Ty::Zero),
    f("CpuRealtimePeriod", Ty::Zero),
    f("CpuRealtimeRuntime", Ty::Zero),
    f("IOMaximumIOps", Ty::Zero),
    f("IOMaximumBandwidth", Ty::Zero),
    fnull("BlkioWeightDevice", Ty::Zero),
    fnull("BlkioDeviceReadBps", Ty::Zero),
    fnull("BlkioDeviceWriteBps", Ty::Zero),
    fnull("BlkioDeviceReadIOps", Ty::Zero),
    fnull("BlkioDeviceWriteIOps", Ty::Zero),
    f("KernelMemory", Ty::Zero),
    f("KernelMemoryTCP", Ty::Zero),
];

fn cpuset_ok(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || b == b',' || b == b'-')
}

pub static CREATE_FIELDS: &[Field] = &[
    // dockerode repeats the container name in the body; Docker ignores it.
    fnull("name", Ty::Str),
    f("Hostname", Ty::Str),
    f("Domainname", Ty::Str),
    f("User", Ty::Str),
    f("AttachStdin", Ty::Bool),
    f("AttachStdout", Ty::Bool),
    f("AttachStderr", Ty::Bool),
    fnull("ExposedPorts", Ty::StrMap(&Ty::EmptyObj)),
    f("Tty", Ty::Bool),
    f("OpenStdin", Ty::Bool),
    f("StdinOnce", Ty::Bool),
    fnull("Env", Ty::Arr(&Ty::Str)),
    fnull("Cmd", Ty::StrOrArr),
    fnull("Healthcheck", Ty::Obj(HEALTHCHECK)),
    fnull("ArgsEscaped", Ty::Bool),
    f("Image", Ty::StrIf(image_ok, "image reference")),
    fnull("Volumes", Ty::StrMap(&Ty::EmptyObj)),
    f("WorkingDir", Ty::Str),
    fnull("Entrypoint", Ty::StrOrArr),
    f("NetworkDisabled", Ty::Bool),
    f("MacAddress", Ty::StrIf(mac_ok, "a MAC address")),
    fnull("OnBuild", Ty::Arr(&Ty::Str)),
    fnull("Labels", Ty::StrMap(&Ty::Str)),
    f("StopSignal", Ty::Str),
    fnull("StopTimeout", Ty::Int(0, 86_400)),
    fnull("Shell", Ty::Arr(&Ty::Str)),
    fnull("HostConfig", Ty::Obj(HOST_CONFIG)),
    fnull("NetworkingConfig", Ty::Obj(NETWORKING)),
];

pub static CREATE: Ty = Ty::Obj(CREATE_FIELDS);
