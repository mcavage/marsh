//! Table-driven decisions for the policy (TEST-PLAN §D U1): request in,
//! decision out. Nothing here touches sockets, mounts or the engine.

use std::cell::RefCell;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::admit::{Admitted, ReqBody, admit};
use super::http::{HttpLimits, parse_request_head, parse_target};
use super::routes::classify;
use super::{
    Authority, CurrentLimits, Env, Lookup, PinRequest, PinnedBind, Refusal, ResourceLimits,
    VolumeScope,
};

const OWN_C: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NET_SCOPE: &str = "5555555555555555555555555555555555555555555555555555555555555555";
const NET_MY: &str = "6666666666666666666666666666666666666666666666666666666666666666";
const OWN_EXEC: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

struct Fake {
    permissive: bool,
    auth: Authority,
    limits: ResourceLimits,
    pinned: RefCell<Vec<String>>,
    current: CurrentLimits,
    blocked: bool,
    inherited: Vec<String>,
    image_volumes: Vec<String>,
}

impl Fake {
    fn new() -> Self {
        Self {
            permissive: false,
            auth: Authority {
                project_root: PathBuf::from("/proj"),
                workspace: "lane-a".into(),
                attempt: "att1".into(),
            },
            limits: ResourceLimits::default(),
            pinned: RefCell::new(Vec::new()),
            current: CurrentLimits::default(),
            blocked: false,
            inherited: Vec::new(),
            image_volumes: Vec::new(),
        }
    }
}

impl Env for Fake {
    fn authority(&self) -> &Authority {
        &self.auth
    }
    fn limits(&self) -> &ResourceLimits {
        &self.limits
    }
    fn pin_bind(&self, req: &PinRequest) -> Result<PinnedBind, Refusal> {
        // The pin result is an input of the table: refuse what the real
        // resolver refuses, name everything else `/pins/<n>`.
        let s = &req.source;
        let root = self.auth.project_root.to_str().unwrap();
        if !(s == root || s.starts_with(&format!("{root}/"))) || s.contains("..") {
            return Err(Refusal::forbid("bind source is outside the project"));
        }
        let rel = s.trim_start_matches(root).trim_start_matches('/');
        let n = self.pinned.borrow().len();
        self.pinned.borrow_mut().push(s.clone());
        let protected = rel == ".git"
            || rel.starts_with(".git/")
            || rel == ".marsh"
            || rel.starts_with(".marsh/");
        Ok(PinnedBind {
            path: format!("/pins/{n}"),
            force_read_only: protected && !req.read_only,
            pin_id: Some(format!("p{n}")),
        })
    }
    fn release_pins(&self, _ids: &[String]) {}
    fn substituted_socket(&self) -> Result<String, Refusal> {
        Ok("/run/marsh/sockets/lane-a/att1/docker.sock".into())
    }
    fn scope_network(&self) -> Result<String, Refusal> {
        Ok("marsh-scope-lane-a".into())
    }
    fn container(&self, r: &str) -> Lookup {
        if self.permissive {
            return Lookup::Own(format!("{r:0<64}"));
        }
        match r {
            "own" | OWN_C => Lookup::Own(OWN_C.into()),
            _ => Lookup::Missing,
        }
    }
    fn exec_container(&self, id: &str) -> Lookup {
        if self.permissive || id == OWN_EXEC {
            Lookup::Own(OWN_C.into())
        } else {
            Lookup::Missing
        }
    }
    fn network(&self, r: &str) -> Lookup {
        if r == "marsh-scope-lane-a" {
            Lookup::Own(NET_SCOPE.into())
        } else if self.permissive || r == "mynet" {
            Lookup::Own(NET_MY.into())
        } else {
            Lookup::Missing
        }
    }
    fn volume(&self, name: &str) -> VolumeScope {
        if self.permissive {
            return VolumeScope::Own;
        }
        match name {
            "mine" => VolumeScope::Own,
            "theirs" => VolumeScope::Foreign,
            _ => VolumeScope::Unknown,
        }
    }
    fn ensure_volume(&self, name: &str) -> Result<(), Refusal> {
        if name == "theirs" && !self.permissive {
            return Err(Refusal::forbid("volume belongs to another workspace"));
        }
        Ok(())
    }
    fn reserve_volume(&self, name: &str) -> Result<bool, Refusal> {
        self.ensure_volume(name).map(|()| true)
    }
    fn release_volume(&self, _name: &str) {}
    fn container_blocked(&self, _id: &str) -> bool {
        self.blocked
    }
    fn container_pins(&self, _id: &str) -> Vec<String> {
        self.inherited.clone()
    }
    fn image_volumes(&self, _image: &str) -> Vec<String> {
        self.image_volumes.clone()
    }
    fn current_limits(&self, _id: &str) -> Result<CurrentLimits, Refusal> {
        Ok(self.current)
    }
    fn socket_aliases(&self) -> &[String] {
        &[]
    }
    fn registry_allowed(&self, image: &str) -> bool {
        !image.starts_with("evil.example/")
    }
}

fn eval(
    method: &str,
    target: &str,
    extra_headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<Admitted, Refusal> {
    eval_with(&Fake::new(), method, target, extra_headers, body)
}

fn eval_with(
    fake: &Fake,
    method: &str,
    target: &str,
    extra_headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<Admitted, Refusal> {
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: d\r\n");
    for (k, v) in extra_headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        raw.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    raw.push_str("\r\n");
    let head = parse_request_head(raw.as_bytes(), &HttpLimits::default())
        .map_err(|e| Refusal::forbid(format!("{e:?}")))?;
    let t = parse_target(target).map_err(Refusal::forbid)?;
    let m = classify(method, &t)?;
    admit(&m, &head, body.map(<[u8]>::to_vec), fake)
}

fn base_hc() -> Value {
    // What the Docker CLI sends for `docker run busybox`, zero values and all.
    serde_json::from_str(
        r#"{"Binds":null,"ContainerIDFile":"","LogConfig":{"Type":"","Config":{}},"NetworkMode":"default",
        "PortBindings":{},"RestartPolicy":{"Name":"no","MaximumRetryCount":0},"AutoRemove":false,
        "VolumeDriver":"","VolumesFrom":null,"ConsoleSize":[0,0],"CapAdd":null,"CapDrop":null,
        "CgroupnsMode":"","Dns":null,"DnsOptions":[],"DnsSearch":[],"ExtraHosts":null,"GroupAdd":null,
        "IpcMode":"","Cgroup":"","Links":null,"OomScoreAdj":0,"PidMode":"","Privileged":false,
        "PublishAllPorts":false,"ReadonlyRootfs":false,"SecurityOpt":null,"UTSMode":"","UsernsMode":"",
        "ShmSize":0,"Isolation":"","CpuShares":0,"Memory":0,"NanoCpus":0,"CgroupParent":"",
        "BlkioWeight":0,"BlkioWeightDevice":[],"BlkioDeviceReadBps":[],"BlkioDeviceWriteBps":[],
        "BlkioDeviceReadIOps":[],"BlkioDeviceWriteIOps":[],"CpuPeriod":0,"CpuQuota":0,
        "CpuRealtimePeriod":0,"CpuRealtimeRuntime":0,"CpusetCpus":"","CpusetMems":"","Devices":[],
        "DeviceCgroupRules":null,"DeviceRequests":null,"MemoryReservation":0,"MemorySwap":0,
        "MemorySwappiness":-1,"OomKillDisable":false,"PidsLimit":0,"Ulimits":null,"CpuCount":0,
        "CpuPercent":0,"IOMaximumIOps":0,"IOMaximumBandwidth":0,"MaskedPaths":null,"ReadonlyPaths":null}"#,
    )
    .unwrap()
}

fn create_body(patch_hc: &Value, patch_top: &Value) -> Vec<u8> {
    let mut hc = base_hc();
    for (k, v) in patch_hc.as_object().unwrap() {
        hc[k] = v.clone();
    }
    let mut top: Value = serde_json::from_str(
        r#"{"Hostname":"","Domainname":"","User":"","AttachStdin":false,"AttachStdout":true,
        "AttachStderr":true,"Tty":false,"OpenStdin":false,"StdinOnce":false,"Env":null,
        "Cmd":["true"],"Image":"busybox","Volumes":{},"WorkingDir":"","Entrypoint":null,"Labels":{},
        "NetworkingConfig":{"EndpointsConfig":{"default":{"IPAMConfig":null,"Links":null,"Aliases":null,
        "DriverOpts":null,"GwPriority":0,"NetworkID":"","EndpointID":"","Gateway":"","IPAddress":"",
        "MacAddress":"","IPPrefixLen":0,"IPv6Gateway":"","GlobalIPv6Address":"","GlobalIPv6PrefixLen":0,
        "DNSNames":null}}}}"#,
    )
    .unwrap();
    // The CLI keys EndpointsConfig by the network it was asked for.
    let mode = hc["NetworkMode"].as_str().unwrap_or("default").to_string();
    if mode != "default" {
        let ep = top["NetworkingConfig"]["EndpointsConfig"]["default"].clone();
        top["NetworkingConfig"]["EndpointsConfig"] = if mode.starts_with("container:") {
            json!({})
        } else {
            json!({ mode.clone(): ep })
        };
    }
    top["HostConfig"] = hc;
    for (k, v) in patch_top.as_object().unwrap() {
        top[k] = v.clone();
    }
    serde_json::to_vec(&top).unwrap()
}

fn create(hc: &Value) -> Result<Value, Refusal> {
    let a = eval(
        "POST",
        "/v1.47/containers/create",
        &[],
        Some(&create_body(hc, &json!({}))),
    )?;
    let ReqBody::Json(b) = a.body else {
        panic!("create without a JSON body")
    };
    Ok(serde_json::from_slice(&b).unwrap())
}

fn refused(r: &Result<Value, Refusal>) -> bool {
    matches!(r, Err(e) if e.status == 403 || e.status == 404)
}

#[test]
fn the_cli_default_create_is_allowed_and_rewritten() {
    let v = create(&json!({})).unwrap();
    let hc = &v["HostConfig"];
    // The ID, not the name: Docker resolves an exact ID before any name.
    assert_eq!(hc["NetworkMode"], NET_SCOPE);
    assert_eq!(hc["Memory"], 4i64 << 30);
    assert_eq!(hc["NanoCpus"], 4_000_000_000i64);
    assert_eq!(hc["PidsLimit"], 4096);
    assert_eq!(v["Labels"]["dev.marsh.scope"], "lane-a");
    assert_eq!(v["Labels"]["dev.marsh.attempt"], "att1");
}

#[test]
fn create_refusals() {
    let cases: Vec<(&str, Value)> = vec![
        ("privileged", json!({"Privileged": true})),
        ("privileged null", json!({"Privileged": null})),
        ("privileged string", json!({"Privileged": "true"})),
        ("root bind", json!({"Binds": ["/:/host"]})),
        ("docker lib bind", json!({"Binds": ["/var/lib/docker:/x"]})),
        (
            "bind outside project",
            json!({"Binds": ["/proj/../etc:/x"]}),
        ),
        ("sibling of project", json!({"Binds": ["/proj-evil:/x"]})),
        (
            "relative bind source with slash",
            json!({"Binds": ["./x:/x"]}),
        ),
        ("bad bind option", json!({"Binds": ["/proj/a:/a:rshared"]})),
        (
            "mount bind root",
            json!({"Mounts": [{"Type": "bind", "Source": "/", "Target": "/h"}]}),
        ),
        (
            "mount create mountpoint",
            json!({"Mounts": [{"Type": "bind", "Source": "/proj/a", "Target": "/h", "BindOptions": {"CreateMountpoint": true}}]}),
        ),
        (
            "mount propagation",
            json!({"Mounts": [{"Type": "bind", "Source": "/proj/a", "Target": "/h", "BindOptions": {"Propagation": "rshared"}}]}),
        ),
        (
            "mount volume subpath",
            json!({"Mounts": [{"Type": "volume", "Source": "mine", "Target": "/v", "VolumeOptions": {"Subpath": "x"}}]}),
        ),
        (
            "mount volume driver opts",
            json!({"Mounts": [{"Type": "volume", "Source": "mine", "Target": "/v", "VolumeOptions": {"DriverConfig": {"Name": "local", "Options": {"device": "/", "o": "bind", "type": "none"}}}}]}),
        ),
        (
            "mount image type",
            json!({"Mounts": [{"Type": "image", "Source": "x", "Target": "/v"}]}),
        ),
        ("foreign volume", json!({"Binds": ["theirs:/v"]})),
        ("reserved volume name", json!({"Binds": ["marsh-pool:/v"]})),
        ("network host", json!({"NetworkMode": "host"})),
        ("default bridge", json!({"NetworkMode": "bridge"})),
        ("foreign network", json!({"NetworkMode": "othernet"})),
        (
            "foreign container namespace",
            json!({"NetworkMode": "container:other"}),
        ),
        ("pid host", json!({"PidMode": "host"})),
        (
            "pid foreign container",
            json!({"PidMode": "container:other"}),
        ),
        ("ipc host", json!({"IpcMode": "host"})),
        ("uts host", json!({"UTSMode": "host"})),
        ("userns host", json!({"UsernsMode": "host"})),
        ("cgroupns host", json!({"CgroupnsMode": "host"})),
        ("volumes-from foreign", json!({"VolumesFrom": ["other"]})),
        ("cap SYS_ADMIN", json!({"CapAdd": ["SYS_ADMIN"]})),
        ("cap ALL", json!({"CapAdd": ["ALL"]})),
        ("cap SYS_MODULE", json!({"CapAdd": ["CAP_SYS_MODULE"]})),
        ("cap SYS_RESOURCE", json!({"CapAdd": ["SYS_RESOURCE"]})),
        ("lowercase cap", json!({"CapAdd": ["net_admin"]})),
        (
            "device",
            json!({"Devices": [{"PathOnHost": "/dev/loop0", "PathInContainer": "/dev/loop0", "CgroupPermissions": "rwm"}]}),
        ),
        (
            "device cgroup rule",
            json!({"DeviceCgroupRules": ["b 7:* rwm"]}),
        ),
        (
            "gpus",
            json!({"DeviceRequests": [{"Count": -1, "Capabilities": [["gpu"]]}]}),
        ),
        (
            "seccomp unconfined",
            json!({"SecurityOpt": ["seccomp=unconfined"]}),
        ),
        (
            "custom seccomp",
            json!({"SecurityOpt": ["seccomp={\"defaultAction\":\"SCMP_ACT_ALLOW\"}"]}),
        ),
        (
            "apparmor unconfined",
            json!({"SecurityOpt": ["apparmor=unconfined"]}),
        ),
        (
            "systempaths unconfined",
            json!({"SecurityOpt": ["systempaths=unconfined"], "MaskedPaths": [], "ReadonlyPaths": []}),
        ),
        ("masked paths emptied", json!({"MaskedPaths": []})),
        ("masked paths replaced", json!({"MaskedPaths": ["/x"]})),
        ("readonly paths emptied", json!({"ReadonlyPaths": []})),
        ("legacy capabilities", json!({"Capabilities": ["ALL"]})),
        ("cgroup parent", json!({"CgroupParent": "/"})),
        (
            "log driver syslog",
            json!({"LogConfig": {"Type": "syslog", "Config": {}}}),
        ),
        (
            "log option path",
            json!({"LogConfig": {"Type": "json-file", "Config": {"labels": "x", "tag": "y"}}}),
        ),
        ("runtime", json!({"Runtime": "runc-x"})),
        ("negative oom score", json!({"OomScoreAdj": -1000})),
        ("oom kill disable", json!({"OomKillDisable": true})),
        (
            "annotations",
            json!({"Annotations": {"org.systemd.property.DeviceAllow": "x"}}),
        ),
        ("storage opt", json!({"StorageOpt": {"size": "1g"}})),
        ("memory above maximum", json!({"Memory": 1i64 << 50})),
        ("pids above maximum", json!({"PidsLimit": 1_000_000})),
        (
            "cpu quota above maximum",
            json!({"CpuQuota": 1_000_000_000i64}),
        ),
        ("shm above maximum", json!({"ShmSize": 1i64 << 50})),
        (
            "nonstandard sysctl",
            json!({"Sysctls": {"kernel.core_pattern": "|/x"}}),
        ),
        (
            "tmpfs option",
            json!({"Tmpfs": {"/t": "size=1m,exec,bind=/x"}}),
        ),
        ("unknown field", json!({"Whatever": 1})),
        (
            "restart policy name",
            json!({"RestartPolicy": {"Name": "sometimes", "MaximumRetryCount": 0}}),
        ),
        (
            "port binding key",
            json!({"PortBindings": {"80/ftp": [{"HostIp": "", "HostPort": "80"}]}}),
        ),
        (
            "port binding key without a port",
            json!({"PortBindings": {"/tcp": [{"HostIp": "", "HostPort": "80"}]}}),
        ),
        (
            "port binding key that is not a number",
            json!({"PortBindings": {"web": [{"HostIp": "", "HostPort": "80"}]}}),
        ),
        (
            "port host ip",
            json!({"PortBindings": {"80/tcp": [{"HostIp": "not-an-ip", "HostPort": "80"}]}}),
        ),
    ];
    for (name, hc) in cases {
        let r = create(&hc);
        assert!(refused(&r), "{name} must be refused, got {r:?}");
    }
}

#[test]
fn create_allowed_rows() {
    for (name, hc) in [
        ("cap NET_ADMIN", json!({"CapAdd": ["NET_ADMIN"]})),
        (
            "cap prefixed",
            json!({"CapAdd": ["CAP_SYS_PTRACE", "NET_RAW", "NET_BIND_SERVICE", "SYS_NICE"]}),
        ),
        ("cap drop all", json!({"CapDrop": ["ALL"]})),
        ("project bind ro", json!({"Binds": ["/proj/sub:/s:ro"]})),
        ("project bind rw", json!({"Binds": ["/proj/sub:/s"]})),
        ("project root bind", json!({"Binds": ["/proj:/p"]})),
        ("named volume", json!({"Binds": ["mine:/v"]})),
        ("unknown named volume", json!({"Binds": ["fresh:/v"]})),
        ("anonymous volume", json!({"Binds": ["/data"]})),
        ("tmpfs", json!({"Tmpfs": {"/t": "size=64m,mode=1777"}})),
        (
            "tmpfs volume",
            json!({"Mounts": [{"Type": "volume", "Source": "mine", "Target": "/v", "VolumeOptions": {"DriverConfig": {"Name": "local", "Options": {"type": "tmpfs", "device": "tmpfs", "o": "size=64m,mode=1777,uid=1000"}}}}]}),
        ),
        (
            "mount bind",
            json!({"Mounts": [{"Type": "bind", "Source": "/proj/d", "Target": "/b", "ReadOnly": true}]}),
        ),
        ("user network", json!({"NetworkMode": "mynet"})),
        (
            "own container namespace",
            json!({"NetworkMode": "container:own"}),
        ),
        ("volumes-from own", json!({"VolumesFrom": ["own:ro"]})),
        ("none network", json!({"NetworkMode": "none"})),
        (
            "resources within maxima",
            json!({"Memory": 1i64 << 30, "NanoCpus": 2_000_000_000i64, "PidsLimit": 100, "ShmSize": 256 << 20}),
        ),
        (
            "restart always",
            json!({"RestartPolicy": {"Name": "always", "MaximumRetryCount": 0}}),
        ),
        (
            "init and extra hosts",
            json!({"Init": true, "ExtraHosts": ["x:1.2.3.4"], "ReadonlyRootfs": true}),
        ),
        (
            "no-new-privileges",
            json!({"SecurityOpt": ["no-new-privileges"]}),
        ),
        (
            "net sysctl",
            json!({"Sysctls": {"net.ipv4.ip_forward": "1"}}),
        ),
        (
            "json-file log opts",
            json!({"LogConfig": {"Type": "json-file", "Config": {"max-size": "10m"}}}),
        ),
    ] {
        let r = create(&hc);
        assert!(r.is_ok(), "{name} must be allowed, got {r:?}");
    }
}

#[test]
fn binds_are_rewritten_to_pins_and_protected_paths_become_read_only() {
    let v = create(&json!({"Binds": ["/proj/sub:/s", "/proj/.git/hooks:/g", "/var/run/docker.sock:/var/run/docker.sock", "/run/docker.sock:/d:ro"]})).unwrap();
    let binds: Vec<&str> = v["HostConfig"]["Binds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap())
        .collect();
    assert_eq!(binds[0], "/pins/0:/s");
    assert_eq!(binds[1], "/pins/1:/g:ro");
    assert_eq!(
        binds[2],
        "/run/marsh/sockets/lane-a/att1/docker.sock:/var/run/docker.sock"
    );
    assert_eq!(binds[3], "/run/marsh/sockets/lane-a/att1/docker.sock:/d:ro");
}

#[test]
fn a_port_key_without_a_protocol_is_tcp_and_still_loopback() {
    // dockerode (Testcontainers): {"5432": [{"HostPort": "0"}]}.
    let v = create(&json!({"PortBindings": {"5432": [{"HostPort": "0"}]}})).unwrap();
    assert_eq!(
        v["HostConfig"]["PortBindings"]["5432"][0]["HostIp"],
        "127.0.0.1"
    );
}

#[test]
fn published_ports_are_forced_to_loopback() {
    let v = create(&json!({"PortBindings": {"80/tcp": [{"HostIp": "0.0.0.0", "HostPort": "8080"}, {"HostIp": "", "HostPort": "8081"}, {"HostIp": "::1", "HostPort": "8082"}, {"HostIp": "10.1.2.3", "HostPort": ""}]}})).unwrap();
    let ips: Vec<&str> = v["HostConfig"]["PortBindings"]["80/tcp"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["HostIp"].as_str().unwrap())
        .collect();
    assert_eq!(ips, ["127.0.0.1", "127.0.0.1", "::1", "127.0.0.1"]);
}

#[test]
fn reserved_labels_and_names() {
    for patch in [
        json!({"Labels": {"dev.marsh.scope": "x"}}),
        json!({"Labels": {"dev.marsh.attempt": "x"}}),
        json!({"Labels": {"dev.marsh.internal": "1"}}),
        json!({"Labels": {"dev.marsh.foo": "1"}}),
        json!({"Labels": {"DEV.MARSH.scope": "1"}}),
    ] {
        let r = eval(
            "POST",
            "/containers/create",
            &[],
            Some(&create_body(&json!({}), &patch)),
        );
        assert!(matches!(&r, Err(e) if e.status == 403), "{patch}: {r:?}");
    }
    for name in [
        "marsh-svc-engine",
        "nf-poolhelper",
        "Marsh-x",
        "bad name",
        "-x",
    ] {
        let r = eval(
            "POST",
            &format!("/containers/create?name={name}"),
            &[],
            Some(&create_body(&json!({}), &json!({}))),
        );
        assert!(r.is_err(), "{name}");
    }
    // Compose labels are just labels.
    let r = eval(
        "POST",
        "/containers/create",
        &[],
        Some(&create_body(
            &json!({}),
            &json!({"Labels": {"com.docker.compose.project": "x"}}),
        )),
    );
    assert!(r.is_ok());
}

#[test]
fn json_and_header_edge_cases() {
    let good = create_body(&json!({}), &json!({}));
    let mut with_trailing = good.clone();
    with_trailing.extend_from_slice(b" xyz");
    let mut dup = String::from_utf8(good.clone()).unwrap();
    dup = dup.replacen("{\"", "{\"Image\":\"x\",\"", 1);
    let upper = String::from_utf8(good.clone())
        .unwrap()
        .replace("\"Privileged\":false", "\"PRIVILEGED\":true");
    let lower_hc = String::from_utf8(good.clone())
        .unwrap()
        .replace("\"HostConfig\"", "\"hostconfig\"")
        .replace("\"Privileged\":false", "\"privileged\":true");
    for (name, body) in [
        ("trailing data", with_trailing),
        ("duplicate key", dup.into_bytes()),
        ("upper-case privileged", upper.into_bytes()),
        (
            "lower-case hostconfig with privileged",
            lower_hc.into_bytes(),
        ),
        ("not an object", b"[1]".to_vec()),
        ("empty", Vec::new()),
        ("bom", [b"\xef\xbb\xbf".as_slice(), &good].concat()),
        ("utf16", good.iter().flat_map(|b| [*b, 0]).collect()),
    ] {
        let r = eval("POST", "/containers/create", &[], Some(&body));
        assert!(matches!(&r, Err(e) if e.status == 403), "{name}: {r:?}");
    }
    // Key casing alone is not an attack when the rest is allowed: it is
    // rebuilt with Docker's spelling.
    let lower = String::from_utf8(good)
        .unwrap()
        .replace("\"Image\"", "\"image\"");
    let a = eval("POST", "/containers/create", &[], Some(lower.as_bytes())).unwrap();
    let ReqBody::Json(b) = a.body else { panic!() };
    assert!(
        String::from_utf8(b)
            .unwrap()
            .contains("\"Image\":\"busybox\"")
    );
    // Duplicate allowed headers are not collapsed silently.
    let r = eval(
        "POST",
        "/containers/create",
        &[
            ("Content-Type", "application/json"),
            ("Content-Type", "text/plain"),
        ],
        Some(&create_body(&json!({}), &json!({}))),
    );
    assert!(matches!(&r, Err(e) if e.status == 400 && e.close));
}

#[test]
fn route_level_refusals() {
    for (m, t, b) in [
        ("GET", "/containers/json", Some(&b"{}"[..])),
        ("POST", "/images/create?fromSrc=http://x/y.tar", None),
        ("POST", "/images/create?fromImage=evil.example/x", None),
        ("POST", "/images/x/push", None),
        ("POST", "/auth", None),
        ("POST", "/commit?container=other&repo=x", None),
        ("POST", "/commit?container=own&repo=marsh-svc-engine", None),
        ("POST", "/build?remote=http://x/ctx.tar", None),
        ("POST", "/build?networkmode=host", None),
        ("POST", "/build?networkmode=container:x", None),
        ("POST", "/build?version=1&cgroupparent=/", None),
        ("POST", "/build?cgroupparent=/", None),
        ("POST", "/build?outputs=%5B%7B%22Type%22%3A%22local%22%7D%5D", None),
        ("GET", "/containers/other/json", None),
        ("GET", "/containers/own/json?x=1", None),
        ("DELETE", "/containers/own?link=1", None),
        ("POST", "/containers/prune?filters=%7B%22bogus%22%3A%5B%22x%22%5D%7D", None),
        ("POST", "/exec/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/start", Some(&b"{}"[..])),
        ("GET", "/networks/other", None),
        ("GET", "/volumes/theirs", None),
        ("DELETE", "/volumes/theirs", None),
        ("POST", "/volumes/create", Some(&br#"{"Name":"v","DriverOpts":{"type":"none","o":"bind","device":"/var/lib/docker"}}"#[..])),
        ("POST", "/volumes/create", Some(&br#"{"Name":"v","DriverOpts":{"type":"none","o":"bind","device":"etc"}}"#[..])),
        ("POST", "/volumes/create", Some(&br#"{"Name":"v","DriverOpts":{"type":"overlay","o":"lowerdir=/"}}"#[..])),
        ("POST", "/volumes/create", Some(&br#"{"Name":"v","DriverOpts":{"type":"nfs"}}"#[..])),
        ("POST", "/volumes/create", Some(&br#"{"Name":"v","Driver":"other"}"#[..])),
        ("POST", "/volumes/create", Some(&br#"{"Name":"v","Labels":{"dev.marsh.scope":"x"}}"#[..])),
        ("POST", "/networks/create", Some(&br#"{"Name":"n","Driver":"macvlan","Options":{"parent":"eth0"}}"#[..])),
        ("POST", "/networks/create", Some(&br#"{"Name":"n","Options":{"com.docker.network.bridge.enable_ip_masquerade":"false"}}"#[..])),
        ("POST", "/networks/create", Some(&br#"{"Name":"host"}"#[..])),
        ("POST", "/networks/create", Some(&br#"{"Name":"n","Labels":{"dev.marsh.scope":"x"}}"#[..])),
        ("POST", "/networks/mynet/connect", Some(&br#"{"Container":"other"}"#[..])),
        ("POST", "/networks/other/connect", Some(&br#"{"Container":"own"}"#[..])),
        ("POST", "/containers/own/exec", Some(&br#"{"Cmd":["id"],"Privileged":true}"#[..])),
        ("POST", "/containers/other/exec", Some(&br#"{"Cmd":["id"]}"#[..])),
        ("POST", "/containers/own/update", Some(&br#"{"Devices":[{"PathOnHost":"/dev/sda"}]}"#[..])),
        ("POST", "/containers/own/update", Some(&br#"{"Memory":1125899906842624}"#[..])),
        ("POST", "/containers/own/update", Some(&br#"{"PidsLimit":-1}"#[..])),
        ("POST", "/containers/own/rename?name=marsh-x", None),
        ("GET", "/events?filters=%7B%22bogus%22%3A%5B%22x%22%5D%7D", None),
        ("PUT", "/containers/own/archive?path=x", None),
        ("GET", "/system/df", None),
    ] {
        let r = eval(m, t, &[], b);
        assert!(r.is_err(), "{m} {t} must be refused, got {:?}", r.map(|a| a.path));
    }
}

#[test]
fn route_level_allowed_rows() {
    for (m, t, b) in [
        ("GET", "/_ping", None),
        ("HEAD", "/v1.56/_ping", None),
        ("GET", "/v1.47/containers/json?all=1&filters=%7B%22label%22%3A%5B%22a%3Db%22%5D%7D", None),
        ("GET", "/containers/own/json", None),
        ("POST", "/containers/own/start", None),
        ("POST", "/containers/own/stop", Some(&b"{}"[..])),
        ("POST", "/containers/own/exec", Some(&br#"{"Cmd":["id"],"User":"0","AttachStdout":true}"#[..])),
        ("POST", "/exec/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee/start", Some(&br#"{"Detach":false,"Tty":false}"#[..])),
        ("POST", "/containers/own/update", Some(&br#"{"Memory":536870912,"MemorySwap":1073741824}"#[..])),
        ("POST", "/volumes/create", Some(&br#"{"Name":"v","Driver":"local","DriverOpts":{"type":"tmpfs","device":"tmpfs","o":"size=64m,mode=1777,uid=1000"}}"#[..])),
        ("POST", "/networks/create", Some(&br#"{"Name":"n1","Driver":"bridge","IPAM":{"Driver":"default"},"Labels":{"a":"b"}}"#[..])),
        ("POST", "/networks/mynet/connect", Some(&br#"{"Container":"own"}"#[..])),
        ("POST", "/images/create?fromImage=busybox&tag=latest", None),
        // dockerode (Testcontainers) repeats the pull's query as a JSON body.
        ("POST", "/images/create?fromImage=postgres&tag=17", Some(&br#"{"fromImage":"postgres","tag":"17"}"#[..])),
        ("POST", "/images/create?fromImage=postgres&tag=17&authconfig=e30", Some(&br#"{"fromImage":"postgres","tag":"17"}"#[..])),
        ("POST", "/build?dockerfile=Dockerfile&t=app&version=2&session=abc&networkmode=default", None),
        ("POST", "/session", None),
        ("POST", "/grpc", None),
        ("PUT", "/containers/own/archive?path=%2Ftmp", None),
        ("DELETE", "/containers/own?force=1&v=1", None),
        ("POST", "/containers/prune", None),
        ("GET", "/events?since=1&filters=%7B%22type%22%3A%7B%22container%22%3Atrue%7D%7D", None),
    ] {
        let hdrs: &[(&str, &str)] = if t.contains("/archive") || t.starts_with("/build") { &[("Content-Type", "application/x-tar"), ("Content-Length", "0")] } else { &[] };
        // Streamed bodies carry their own framing header in this fake.
        let hdrs: Vec<(&str, &str)> = hdrs.iter().copied().filter(|(k, _)| *k != "Content-Length").collect();
        let r = eval(m, t, &hdrs, b);
        assert!(r.is_ok(), "{m} {t} must be allowed, got {:?}", r.err());
    }
}

#[test]
fn list_queries_are_pinned_to_the_scope() {
    for t in [
        "/containers/json",
        "/networks",
        "/volumes",
        "/containers/prune",
        "/volumes/prune",
        "/networks/prune",
    ] {
        let m = if t.ends_with("prune") { "POST" } else { "GET" };
        let a = eval(m, t, &[], None).unwrap();
        let f = a
            .query
            .iter()
            .find(|(k, _)| k == "filters")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(f.contains("dev.marsh.scope=lane-a"), "{t}: {f}");
    }
    // A user's own label filter is kept and ANDed.
    let a = eval(
        "GET",
        "/containers/json?filters=%7B%22label%22%3A%5B%22a%3Db%22%5D%7D",
        &[],
        None,
    )
    .unwrap();
    let f = &a.query.iter().find(|(k, _)| k == "filters").unwrap().1;
    assert!(
        f.contains("a=b") && f.contains("dev.marsh.scope=lane-a"),
        "{f}"
    );
}

#[test]
fn body_fields_repeated_in_the_query_are_dropped() {
    // dockerode sends `POST /containers/create?Image=..&name=x` with the same
    // fields in the body; the engine ignores them and so must we, silently.
    let a = eval(
        "POST",
        "/containers/create?name=x&Image=busybox&HostConfig=%7B%7D",
        &[],
        Some(&create_body(&json!({}), &json!({}))),
    )
    .unwrap();
    assert_eq!(a.query, vec![("name".to_string(), "x".to_string())]);
    // An unknown key that is not a body field is still refused.
    assert!(
        eval(
            "POST",
            "/containers/create?name=x&bogus=1",
            &[],
            Some(&create_body(&json!({}), &json!({})))
        )
        .is_err()
    );
}

#[test]
fn upgrade_header_handling() {
    // Plain routes ignore (never forward) a harmless Upgrade, refuse h2c.
    let a = eval(
        "GET",
        "/_ping",
        &[("Upgrade", "tcp"), ("Connection", "Upgrade")],
        None,
    )
    .unwrap();
    assert!(a.upgrade.is_none() && a.hijack.is_none());
    {
        let h = ("Upgrade", "h2c");
        let r = eval("GET", "/_ping", &[h], None);
        assert!(matches!(&r, Err(e) if e.close), "{r:?}");
    }
    // Hijack routes forward exactly the expected protocol.
    let a = eval(
        "POST",
        "/containers/own/attach?stream=1&stdout=1",
        &[("Upgrade", "tcp"), ("Connection", "Upgrade")],
        None,
    )
    .unwrap();
    assert_eq!(a.upgrade.as_deref(), Some("tcp"));
    let a = eval(
        "POST",
        "/session",
        &[("Upgrade", "h2c"), ("Connection", "Upgrade")],
        None,
    )
    .unwrap();
    assert_eq!(a.upgrade.as_deref(), Some("h2c"));
    assert!(
        eval(
            "POST",
            "/containers/own/attach",
            &[("Upgrade", "h2c")],
            None
        )
        .is_err()
    );
}

/// Recorded Docker CLI 29, Compose v5 and buildx traffic (`e3/corpus/cli.jsonl`,
/// from `e3/record.sh`): every request must be admitted, and every create
/// must come out with the scope's labels. Objects are treated as owned.
#[test]
fn recorded_client_traffic_is_admitted() {
    use base64::Engine as _;
    let corpus = include_str!("../../e3/corpus/cli.jsonl");
    let mut n = 0;
    for line in corpus.lines() {
        let r: Value = serde_json::from_str(line).unwrap();
        let method = r["method"].as_str().unwrap();
        // The corpus was recorded against a workspace named `nfx`; commits are
        // admitted only under the caller's own workspace.
        let target = &r["target"]
            .as_str()
            .unwrap()
            .replace("repo=docker.io%2Fnfx%2Fc1", "repo=docker.io%2Flane-a%2Fc1");
        let body = base64::engine::general_purpose::STANDARD
            .decode(r["body_b64"].as_str().unwrap())
            .unwrap();
        let keep = [
            "upgrade",
            "connection",
            "content-type",
            "user-agent",
            "x-docker-expose-session-uuid",
            "x-docker-expose-session-name",
            "x-docker-expose-session-sharedkey",
            "x-docker-expose-session-grpc-method",
        ];
        let hdrs: Vec<(String, String)> = r["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h[0].as_str().unwrap().to_string(),
                    h[1].as_str().unwrap().to_string(),
                )
            })
            .filter(|(k, _)| keep.contains(&k.to_ascii_lowercase().as_str()))
            .collect();
        let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let is_json = body.first() == Some(&b'{') && !target.contains("/build");
        let mut fake = Fake::new();
        fake.permissive = true;
        fake.auth.project_root = PathBuf::from("/private/tmp/nfx-e3/proj");
        let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: d\r\n");
        for (k, v) in &hdrs {
            raw.push_str(&format!("{k}: {v}\r\n"));
        }
        if is_json {
            raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        raw.push_str("\r\n");
        let head = parse_request_head(raw.as_bytes(), &HttpLimits::default()).unwrap();
        let t = parse_target(target).unwrap();
        let m = classify(method, &t).unwrap_or_else(|e| panic!("{method} {target}: {e:?}"));
        let a = admit(&m, &head, is_json.then(|| body.clone()), &fake).unwrap_or_else(|e| {
            panic!(
                "{method} {target}: {e:?} body={}",
                String::from_utf8_lossy(&body)
            )
        });
        if target.contains("/containers/create") {
            let ReqBody::Json(b) = a.body else { panic!() };
            let v: Value = serde_json::from_slice(&b).unwrap();
            assert_eq!(v["Labels"]["dev.marsh.scope"], "lane-a");
        }
        n += 1;
    }
    assert!(n > 200, "corpus shrank: {n}");
}

// --- round 2 witnesses -------------------------------------------------------------
//
// `e3/witness.sh` runs each of these again with the defect it guards put back
// (`E3_TEST_BREAKS`) and requires it to fail.

fn body_of(a: &Admitted) -> Value {
    let ReqBody::Json(b) = &a.body else {
        panic!("no JSON body")
    };
    serde_json::from_slice(b).unwrap()
}

fn mounts_create(mounts: &Value) -> Result<Value, Refusal> {
    let a = eval(
        "POST",
        "/v1.47/containers/create",
        &[],
        Some(&create_body(&json!({"Mounts": mounts}), &json!({}))),
    )?;
    Ok(body_of(&a))
}

#[test]
fn non_recursive_bind_mounts_are_refused() {
    let bind = |opts: Value| json!([{"Type": "bind", "Source": "/proj", "Target": "/w", "BindOptions": opts}]);
    for (name, opts) in [
        ("non-recursive", json!({"NonRecursive": true})),
        (
            "non-recursive with propagation",
            json!({"NonRecursive": true, "Propagation": "rprivate"}),
        ),
    ] {
        let r = mounts_create(&bind(opts));
        assert!(
            matches!(&r, Err(e) if e.status == 403),
            "{name} must be refused (it drops the read-only .git/.marsh), got {r:?}"
        );
    }
    // Controls: the recursive spellings are allowed and pinned.
    for opts in [
        json!({}),
        json!({"NonRecursive": false}),
        json!({"Propagation": "rprivate"}),
    ] {
        let v = mounts_create(&bind(opts.clone())).unwrap_or_else(|e| panic!("{opts}: {e:?}"));
        assert_eq!(v["HostConfig"]["Mounts"][0]["Source"], "/pins/0");
    }
}

#[test]
fn private_propagation_is_refused_everywhere() {
    let r = mounts_create(
        &json!([{"Type": "bind", "Source": "/proj/a", "Target": "/w",
        "BindOptions": {"Propagation": "private"}}]),
    );
    assert!(matches!(&r, Err(e) if e.status == 403), "{r:?}");
    let r = create(&json!({"Binds": ["/proj/a:/w:private"]}));
    assert!(matches!(&r, Err(e) if e.status == 403), "{r:?}");
    assert!(create(&json!({"Binds": ["/proj/a:/w:rprivate"]})).is_ok());
}

#[test]
fn commits_go_only_into_the_workspaces_own_repository() {
    for (repo, ok) in [
        ("busybox", false),
        ("busybox%3Alatest", false),
        ("docker.io%2Flibrary%2Fbusybox", false),
        ("lane-b%2Fapp", false),
        ("docker.io%2Flane-b%2Fapp", false),
        ("lane-a", false),
        ("lane-a%2F", false),
        ("lane-a%2Fapp", true),
        ("docker.io%2Flane-a%2Fapp", true),
        ("LANE-A%2FApp", true),
    ] {
        let r = eval(
            "POST",
            &format!("/commit?container=own&repo={repo}"),
            &[],
            None,
        );
        assert_eq!(
            r.is_ok(),
            ok,
            "commit repo {repo}: {:?}",
            r.map(|a| a.query)
        );
    }
}

fn update(fake: &Fake, patch: &str) -> Result<Value, Refusal> {
    eval_with(
        fake,
        "POST",
        "/containers/own/update",
        &[],
        Some(patch.as_bytes()),
    )
    .map(|a| body_of(&a))
}

#[test]
fn update_limits_are_validated_after_merging_with_the_current_ones() {
    let max_mem = 4i64 << 30;
    let mut f = Fake::new();
    f.limits = ResourceLimits {
        max_memory: max_mem,
        max_nano_cpus: 4_000_000_000,
        max_pids: 32_768,
        ..ResourceLimits::default()
    };
    f.current = CurrentLimits {
        memory: 1 << 30,
        memory_swap: 2 << 30,
        nano_cpus: 0,
        cpu_quota: 10_000,
        cpu_period: 100_000,
        pids: 1000,
    };
    // A partial update: the quota stays, the period shrinks: 10 CPUs now.
    let r = update(&f, r#"{"CpuPeriod":1000}"#);
    assert!(
        matches!(&r, Err(e) if e.status == 403),
        "merged quota/period: {r:?}"
    );
    // The same period with a quota that keeps it under the maximum is fine.
    assert!(update(&f, r#"{"CpuPeriod":1000,"CpuQuota":1000}"#).is_ok());
    // Boundaries: exactly the maximum passes, one more does not.
    assert!(
        update(
            &f,
            &format!(r#"{{"Memory":{max_mem},"MemorySwap":{max_mem}}}"#)
        )
        .is_ok()
    );
    for patch in [
        format!(r#"{{"Memory":{}}}"#, max_mem + 1),
        format!(r#"{{"MemorySwap":{}}}"#, max_mem + 1),
        r#"{"NanoCpus":4000000001}"#.to_string(),
        r#"{"PidsLimit":32769}"#.to_string(),
        r#"{"CpuQuota":400001}"#.to_string(),
    ] {
        let r = update(&f, &patch);
        assert!(matches!(&r, Err(e) if e.status == 403), "{patch}: {r:?}");
    }
    assert!(update(&f, r#"{"NanoCpus":4000000000,"PidsLimit":32768}"#).is_ok());
}

#[test]
fn update_refuses_every_spelling_of_unlimited() {
    let f = Fake::new();
    for patch in [
        r#"{"PidsLimit":0}"#,
        r#"{"PidsLimit":-1}"#,
        r#"{"MemorySwap":-1}"#,
    ] {
        let r = update(&f, patch);
        assert!(
            matches!(&r, Err(e) if e.status == 403),
            "{patch} must be refused, got {r:?}"
        );
    }
    // A container that is unlimited now gets the defaults when anything is
    // updated, instead of staying unlimited.
    let v = update(&f, r#"{"NanoCpus":1000000000}"#).unwrap();
    assert_eq!(v["Memory"], 4i64 << 30, "{v}");
    assert_eq!(v["PidsLimit"], 4096, "{v}");
    // `null` means unchanged (the CLI sends it).
    assert!(update(&f, r#"{"PidsLimit":null,"CpuShares":512}"#).is_ok());
}

#[test]
fn update_does_not_change_the_restart_policy() {
    let f = Fake::new();
    let r = update(
        &f,
        r#"{"RestartPolicy":{"Name":"always","MaximumRetryCount":0}}"#,
    );
    assert!(matches!(&r, Err(e) if e.status == 403), "{r:?}");
    // The zero value is what the CLI sends when no policy is given.
    assert!(
        update(
            &f,
            r#"{"RestartPolicy":{"Name":"","MaximumRetryCount":0},"Memory":536870912}"#
        )
        .is_ok()
    );
}

#[test]
fn network_create_takes_no_options() {
    for opts in [
        r#"{"com.docker.network.driver.mtu":"1400"}"#,
        r#"{"com.docker.network.bridge.name":"x"}"#,
    ] {
        let body = format!(r#"{{"Name":"n1","Driver":"bridge","Options":{opts}}}"#);
        let r = eval("POST", "/networks/create", &[], Some(body.as_bytes()));
        assert!(
            matches!(&r, Err(e) if e.status == 403),
            "{opts}: {:?}",
            r.map(|a| a.path)
        );
    }
    assert!(
        eval(
            "POST",
            "/networks/create",
            &[],
            Some(br#"{"Name":"n1","Options":{}}"#)
        )
        .is_ok()
    );
}

#[test]
fn build_outputs_is_a_closed_schema_rebuilt_before_forwarding() {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>()
    };
    let build = |o: &str| {
        eval(
            "POST",
            &format!("/build?outputs={}", enc(o)),
            &[("Content-Type", "application/x-tar")],
            None,
        )
    };
    for o in [
        r#"[{"Type":"moby","Unexpected":1}]"#,
        r#"[{"Type":"moby","Attrs":{"push":"true"}}]"#,
        r#"[{"Type":"moby","Attrs":{"registry.insecure":"true"}}]"#,
        r#"[{"Type":"moby","Attrs":{"name":1}}]"#,
        r#"[{"Type":"local","Attrs":{"dest":"/x"}}]"#,
        r#"[{"type":"moby"}]"#,
        r#"[{"Type":"moby","Attrs":"x"}]"#,
        r#"["moby"]"#,
        r#""moby""#,
        r#"[{"Type":"moby"},{"Type":"moby"},{"Type":"moby"},{"Type":"moby"},{"Type":"moby"}]"#,
    ] {
        let r = build(o);
        assert!(
            matches!(&r, Err(e) if e.status == 403),
            "{o} must be refused, got {:?}",
            r.map(|a| a.query)
        );
    }
    let a = build(r#"[ {"Type":"moby", "Attrs": {"name":"app:1", "unpack":"true"}} ]"#).unwrap();
    let sent = &a.query.iter().find(|(k, _)| k == "outputs").unwrap().1;
    assert_eq!(
        sent,
        r#"[{"Type":"moby","Attrs":{"name":"app:1","unpack":"true"}}]"#
    );
}

#[test]
fn names_that_look_like_a_full_id_are_refused() {
    let id = "ab".repeat(32);
    for (what, r) in [
        (
            "container",
            eval(
                "POST",
                &format!("/containers/create?name={id}"),
                &[],
                Some(&create_body(&json!({}), &json!({}))),
            ),
        ),
        (
            "network",
            eval(
                "POST",
                "/networks/create",
                &[],
                Some(format!(r#"{{"Name":"{id}"}}"#).as_bytes()),
            ),
        ),
        (
            "volume",
            eval(
                "POST",
                "/volumes/create",
                &[],
                Some(format!(r#"{{"Name":"{id}"}}"#).as_bytes()),
            ),
        ),
        (
            "rename",
            eval(
                "POST",
                &format!("/containers/own/rename?name={id}"),
                &[],
                None,
            ),
        ),
    ] {
        assert!(
            matches!(&r, Err(e) if e.status == 403),
            "{what} named like an ID: {:?}",
            r.map(|a| a.path)
        );
    }
    // Control: a 63-character name and a non-hex 64-character one are names.
    for n in ["a".repeat(63), format!("{}z", "a".repeat(63))] {
        let r = eval(
            "POST",
            &format!("/containers/create?name={n}"),
            &[],
            Some(&create_body(&json!({}), &json!({}))),
        );
        assert!(r.is_ok(), "{n}: {:?}", r.err());
    }
}

#[test]
fn networks_are_forwarded_by_id() {
    let v = create(&json!({"NetworkMode": "mynet"})).unwrap();
    assert_eq!(v["HostConfig"]["NetworkMode"], NET_MY);
    assert!(
        v["NetworkingConfig"]["EndpointsConfig"]
            .get(NET_MY)
            .is_some(),
        "{v}"
    );
    let v = create(&json!({})).unwrap();
    assert_eq!(v["HostConfig"]["NetworkMode"], NET_SCOPE);
    assert!(
        v["NetworkingConfig"]["EndpointsConfig"]
            .get(NET_SCOPE)
            .is_some(),
        "{v}"
    );
}

#[test]
fn volumes_from_pins_are_inherited_by_the_new_container() {
    let mut f = Fake::new();
    f.inherited = vec!["p7".into()];
    let a = eval_with(
        &f,
        "POST",
        "/containers/create",
        &[],
        Some(&create_body(
            &json!({"VolumesFrom": ["own:ro"]}),
            &json!({}),
        )),
    )
    .unwrap();
    let super::admit::Post::ContainerCreate { inherited, .. } = &a.post else {
        panic!("not a create")
    };
    assert_eq!(inherited, &vec!["p7".to_string()]);
}

#[test]
fn anonymous_volumes_become_labelled_mounts() {
    let mut f = Fake::new();
    f.image_volumes = vec!["/fromimage".into()];
    let body = create_body(
        &json!({"Binds": ["/anon", "/proj/x:/bound"]}),
        &json!({"Volumes": {"/declared": {}}}),
    );
    let a = eval_with(&f, "POST", "/containers/create", &[], Some(&body)).unwrap();
    let v = body_of(&a);
    let mounts = v["HostConfig"]["Mounts"].as_array().unwrap();
    let targets: Vec<&str> = mounts
        .iter()
        .map(|m| m["Target"].as_str().unwrap())
        .collect();
    for t in ["/anon", "/declared", "/fromimage"] {
        let m = mounts
            .iter()
            .find(|m| m["Target"] == t)
            .unwrap_or_else(|| panic!("{t} not mounted: {targets:?}"));
        assert_eq!(m["Type"], "volume");
        assert_eq!(
            m["VolumeOptions"]["Labels"]["dev.marsh.scope"], "lane-a",
            "{t}"
        );
        assert_eq!(
            m["VolumeOptions"]["Labels"]["dev.marsh.attempt"], "att1",
            "{t}"
        );
    }
    // The bind stays a bind, and the declaration is not left to Docker.
    assert_eq!(v["HostConfig"]["Binds"], json!(["/pins/0:/bound"]));
}

#[test]
fn a_websocket_attach_carries_its_handshake_key_and_version_only() {
    let url = "/v1.47/containers/own/attach/ws?stream=1&stdout=1";
    let hs = |k: &str, v: &str| {
        vec![
            ("Connection", "Upgrade"),
            ("Upgrade", "websocket"),
            ("Sec-WebSocket-Version", v.to_string().leak() as &str),
            ("Sec-WebSocket-Key", k.to_string().leak() as &str),
            ("Sec-WebSocket-Extensions", "permessage-deflate"),
            ("Origin", "http://evil.example"),
        ]
    };
    let a = eval("GET", url, &hs("dGhlIHNhbXBsZSBub25jZQ==", "13"), None).unwrap();
    let names: Vec<&str> = a.headers.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        ["Sec-WebSocket-Key", "Sec-WebSocket-Version"],
        "{:?}",
        a.headers
    );
    for (what, k, v) in [
        ("short key", "abc", "13"),
        ("key with a space", "dGhlIHNhbXBsZSBub25 jZQ==", "13"),
        ("not base64", "!!!!!!!!!!!!!!!!!!!!!!==", "13"),
        ("old version", "dGhlIHNhbXBsZSBub25jZQ==", "8"),
    ] {
        let r = eval("GET", url, &hs(k, v), None);
        assert!(r.is_err(), "{what} must be refused");
    }
    // No key at all.
    let r = eval(
        "GET",
        url,
        &[("Connection", "Upgrade"), ("Upgrade", "websocket")],
        None,
    );
    assert!(r.is_err());
}
