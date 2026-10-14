//! Stock `sbx` outputs per supported version, from `tests/data/sbx` (see the
//! README there for what was captured and what was derived).

/// How a version renders `sbx ls --json` rows for VMs the model owns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InventoryShape {
    /// `{"sandboxes": [...]}` with upper-case UUIDs (0.46).
    Envelope046,
    /// `{"sandboxes": [...]}` (0.47.0).
    Envelope047,
    /// Bare `[...]` rows, some with `detached` (dev builds after 0.47.0).
    BareArray,
}

pub(super) struct StockVersion {
    pub name: &'static str,
    pub version_output: &'static [u8],
    pub ls_json: &'static [u8],
    pub template_ls_json: &'static [u8],
    pub create_help: &'static str,
    pub shape: InventoryShape,
}

macro_rules! data {
    ($path:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/sbx/",
            $path
        ))
    };
}

macro_rules! text {
    ($path:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/sbx/",
            $path
        ))
    };
}

pub(super) const V046: StockVersion = StockVersion {
    name: "v0.46.0",
    version_output: data!("v0.46.0/version.stdout"),
    ls_json: data!("v0.46.0/ls-json.stdout"),
    template_ls_json: data!("v0.46.0/template-ls-json.stdout"),
    create_help: text!("v0.47.0/create-help.stdout"),
    shape: InventoryShape::Envelope046,
};

pub(super) const V047: StockVersion = StockVersion {
    name: "v0.47.0",
    version_output: data!("v0.47.0/version.stdout"),
    ls_json: data!("v0.47.0/ls-json.stdout"),
    template_ls_json: data!("v0.47.0/template-ls-json.stdout"),
    create_help: text!("v0.47.0/create-help.stdout"),
    shape: InventoryShape::Envelope047,
};

pub(super) const V047_DEV: StockVersion = StockVersion {
    name: "v0.47-dev",
    version_output: data!("v0.47-dev/version.stdout"),
    ls_json: data!("v0.47-dev/ls-json.stdout"),
    template_ls_json: data!("v0.47-dev/template-ls-json.stdout"),
    create_help: text!("v0.47-dev/create-help.stdout"),
    shape: InventoryShape::BareArray,
};

// Includes historical formats for parser/model coverage, not startup eligibility.
pub(super) const VERSIONS: [&StockVersion; 3] = [&V046, &V047, &V047_DEV];

/// Captured from v0.47.0: `sbx inspect --json marsh-nonexistent-zz9`, exit 1.
pub(super) const MISSING_VM_STDERR: &str = text!("v0.47.0/inspect-missing.stderr");
pub(super) const MISSING_VM_CAPTURED_NAME: &str = "marsh-nonexistent-zz9";

pub(super) const EXEC_HELP: &str = text!("v0.47.0/exec-help.stdout");
pub(super) const RM_HELP: &str = text!("v0.47.0/rm-help.stdout");
pub(super) const STOP_HELP: &str = text!("v0.47.0/stop-help.stdout");
pub(super) const CP_HELP: &str = text!("v0.47.0/cp-help.stdout");
pub(super) const MOUNT_HELP: &str = text!("v0.47.0/mount-help.stdout");
pub(super) const UMOUNT_HELP: &str = text!("v0.47.0/umount-help.stdout");
pub(super) const INSPECT_HELP: &str = text!("v0.47.0/inspect-help.stdout");
pub(super) const LS_HELP: &str = text!("v0.47.0/ls-help.stdout");
pub(super) const TEMPLATE_LS_HELP: &str = text!("v0.47.0/template-ls-help.stdout");
pub(super) const MCP_LOAD_HELP: &str = text!("v0.47.0/mcp-load-help.stdout");

/// Not captured (needs a mutating command); see the README.
pub(super) const UNVERIFIED_ALREADY_MOUNTED: &str =
    text!("unverified/mount-already-mounted.stderr");
pub(super) const UNVERIFIED_UMOUNT_NOT_EXPOSED: &str =
    text!("unverified/umount-not-exposed.stderr");
pub(super) const UNVERIFIED_UMOUNT_NOT_BOUND: &str = text!("unverified/umount-not-bound.stderr");

/// The stock missing-VM diagnostic for `name`.
pub(super) fn missing_vm_stderr(name: &str) -> Vec<u8> {
    MISSING_VM_STDERR
        .replace(MISSING_VM_CAPTURED_NAME, name)
        .into_bytes()
}

/// One flag accepted by a stock verb, read from its `--help` text.
#[derive(Clone, Debug)]
pub(super) struct Flag {
    pub short: Option<char>,
    pub long: String,
    pub takes_value: bool,
}

/// Flags listed under `Flags:` and `Global Flags:` in a cobra help text.
pub(super) fn flags(help: &str) -> Vec<Flag> {
    help.lines()
        .filter_map(|line| {
            // `  -e, --env stringArray   Set ...`: the flag column ends at the
            // first double space; cobra prints a type word only for flags
            // that take a value.
            let head = line.trim_start().split("  ").next()?;
            let (short, rest) = match head.strip_prefix('-') {
                Some(rest) if !rest.starts_with('-') => {
                    let short = rest.chars().next()?;
                    (Some(short), rest.get(1..)?.strip_prefix(", ")?)
                }
                _ => (None, head),
            };
            let mut words = rest.strip_prefix("--")?.split(' ');
            let long = words.next()?.to_owned();
            Some(Flag {
                short,
                long,
                takes_value: words.next().is_some(),
            })
        })
        .collect()
}
