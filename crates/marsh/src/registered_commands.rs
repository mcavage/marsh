//! Generic registered-command dispatch through Brush's process shim.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fmt,
    os::unix::ffi::OsStrExt as _,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// One expanded registered-command invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub command: String,
    pub args: Vec<OsString>,
    pub placement: marsh_daemon::Placement,
    pub environment: marsh_contracts::ExportedEnvironment,
    /// Physical cwd of the actual command process, independent of session authority.
    pub working_directory: PathBuf,
}

/// Executes a registered command using the process's inherited standard streams.
pub trait RegisteredCommandExecutor: Send + Sync + 'static {
    fn execute(&self, invocation: Invocation) -> i32;
}

/// Validated shell names from the user's command-to-native-kit mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredCommands(Vec<String>);

impl RegisteredCommands {
    /// Validate and deduplicate registered command names.
    ///
    /// # Errors
    /// Rejects empty, path-like, oversized, non-ASCII, or duplicate names.
    pub fn new(names: impl IntoIterator<Item = String>) -> Result<Self, InstallError> {
        let mut names: Vec<_> = names.into_iter().collect();
        if names.is_empty() || names.len() > 256 {
            return Err(InstallError::InvalidRegistry);
        }
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1])
            || names.iter().any(|name| {
                name.is_empty()
                    || name.len() > 128
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                    })
            })
        {
            return Err(InstallError::InvalidRegistry);
        }
        Ok(Self(names))
    }

    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallError {
    InvalidRegistry,
    AlreadyInstalled,
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRegistry => formatter.write_str("invalid registered command registry"),
            Self::AlreadyInstalled => formatter.write_str("registered commands already installed"),
        }
    }
}

impl std::error::Error for InstallError {}

static EXECUTOR: OnceLock<Arc<dyn RegisteredCommandExecutor>> = OnceLock::new();

/// Install mapped command names before entering Brush.
///
/// Brush exposes these as non-special process shims. Existing builtins remain
/// authoritative, functions shadow the shims, and explicit paths bypass them.
///
/// # Errors
/// Installation is process-global and may happen only once.
pub fn install_registered_commands(
    commands: RegisteredCommands,
    executor: Arc<dyn RegisteredCommandExecutor>,
) -> Result<(), InstallError> {
    EXECUTOR
        .set(executor)
        .map_err(|_| InstallError::AlreadyInstalled)?;
    let dispatch = dispatch_registered as brush_shell::bundled::BundledFn;
    let registry: HashMap<_, _> = commands
        .0
        .into_iter()
        .map(|name| (name, dispatch))
        .collect();
    brush_shell::bundled::install(registry);
    Ok(())
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "Brush's BundledFn ABI owns its argument vector"
)]
fn dispatch_registered(args: Vec<OsString>) -> i32 {
    let Some((command, args)) = args.split_first() else {
        return 125;
    };
    let Some(command) = command.to_str() else {
        return 125;
    };
    let Some(executor) = EXECUTOR.get() else {
        return 125;
    };
    let invocation = match invocation_from_environment(command.to_owned(), args.to_vec()) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("marsh: {error}");
            return 125;
        }
    };
    executor.execute(invocation)
}

/// Build the same invocation for a process reached through the session PATH.
///
/// # Errors
/// Rejects an invalid placement selector or an unavailable process cwd.
pub fn invocation_from_environment(
    command: String,
    args: Vec<OsString>,
) -> Result<Invocation, &'static str> {
    let placement = match std::env::var_os("MARSH_PLACE") {
        None => marsh_daemon::Placement::Local,
        Some(value) if value == "local" => marsh_daemon::Placement::Local,
        Some(_) => return Err("MARSH_PLACE must be local"),
    };
    // Brush launches the shim through its ordinary external-command cwd path;
    // descendant PATH commands likewise inherit their actual process cwd. PWD
    // is user-controlled shell data and cannot select a worker directory.
    let working_directory = std::env::current_dir()
        .map_err(|_| "cannot determine registered command working directory")?;
    let environment = exported_environment();
    Ok(Invocation {
        command,
        args,
        placement,
        environment,
        working_directory,
    })
}

fn exported_environment() -> marsh_contracts::ExportedEnvironment {
    let (environment, omitted) = collect_exported_environment(std::env::vars_os());
    for name in omitted {
        eprintln!(
            "marsh: omitted unsupported exported variable {}",
            name.escape_debug()
        );
    }
    environment
}

pub fn collect_exported_environment(
    variables: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> (marsh_contracts::ExportedEnvironment, Vec<String>) {
    let mut environment = BTreeMap::new();
    let mut omitted = Vec::new();
    for (name, value) in variables {
        let Ok(name) = name.into_string() else {
            omitted.push("<non-UTF-8 name>".into());
            continue;
        };
        if marsh_contracts::reserved_exported_environment_name(&name) {
            continue;
        }
        let value = value.as_bytes().to_vec();
        if marsh_contracts::validate_exported_environment(&BTreeMap::from([(
            name.clone(),
            value.clone(),
        )]))
        .is_err()
        {
            omitted.push(name);
            continue;
        }
        environment.insert(name, value);
    }
    while marsh_contracts::validate_exported_environment(&environment).is_err() {
        let Some(name) = environment
            .iter()
            .max_by_key(|(name, value)| (value.len(), *name))
            .map(|(name, _)| name.clone())
        else {
            break;
        };
        environment.remove(&name);
        omitted.push(name);
    }
    (environment, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeExecutor;

    impl RegisteredCommandExecutor for FakeExecutor {
        fn execute(&self, invocation: Invocation) -> i32 {
            i32::try_from(invocation.args.len()).unwrap()
        }
    }

    #[test]
    fn registry_is_generic_sorted_and_path_free() {
        let commands = RegisteredCommands::new(["z-review".into(), "a_agent".into()]).unwrap();
        assert_eq!(commands.names(), ["a_agent", "z-review"]);
        for invalid in [
            vec![],
            vec!["a/b".into()],
            vec!["same".into(), "same".into()],
        ] {
            assert_eq!(
                RegisteredCommands::new(invalid),
                Err(InstallError::InvalidRegistry)
            );
        }
    }

    #[test]
    fn fake_executor_receives_transport_neutral_invocation() {
        assert_eq!(
            FakeExecutor.execute(Invocation {
                command: "review".into(),
                args: vec!["one".into(), "two".into()],
                placement: marsh_daemon::Placement::Local,
                environment: BTreeMap::new(),
                working_directory: "/fixture".into(),
            }),
            2
        );
    }

    #[test]
    fn exported_values_preserve_every_non_nul_byte_without_omission() {
        use std::os::unix::ffi::OsStringExt as _;
        let bytes: Vec<u8> = (1..=255).collect();
        let (environment, omitted) = collect_exported_environment([
            ("RAW_VALUE".into(), OsString::from_vec(bytes.clone())),
            ("EMPTY_VALUE".into(), OsString::new()),
            (
                "CARGO_BIN_EXE_a-b.c".into(),
                OsString::from_vec(vec![0xff, 0xfe]),
            ),
            ("MARSH_DAEMON_TOKEN".into(), OsString::from_vec(vec![0xff])),
        ]);
        assert!(omitted.is_empty());
        assert_eq!(environment["RAW_VALUE"], bytes);
        assert_eq!(environment["EMPTY_VALUE"], b"");
        assert_eq!(environment["CARGO_BIN_EXE_a-b.c"], [0xff, 0xfe]);
        assert!(!environment.contains_key("MARSH_DAEMON_TOKEN"));
    }

    #[test]
    fn worker_name_policy_does_not_reencode_raw_names_or_nul_values() {
        use std::os::unix::ffi::OsStringExt as _;
        let (environment, omitted) = collect_exported_environment([
            (OsString::from_vec(b"RAW_\xff".to_vec()), "value".into()),
            (
                "INVALID_NUL".into(),
                OsString::from_vec(b"before\0after".to_vec()),
            ),
        ]);
        assert!(environment.is_empty());
        assert_eq!(omitted, ["<non-UTF-8 name>", "INVALID_NUL"]);
    }

    #[test]
    fn oversized_export_does_not_block_other_variables() {
        let (environment, omitted) = collect_exported_environment([
            ("PROBE_VALUE".into(), "kept".into()),
            ("BIG".into(), "x".repeat(17_000).into()),
        ]);
        assert_eq!(
            environment.get("PROBE_VALUE").map(Vec::as_slice),
            Some(b"kept".as_slice())
        );
        assert_eq!(omitted, ["BIG"]);
    }

    #[test]
    fn total_budget_omits_largest_entries_without_blocking_command() {
        let mut variables = vec![("PATH".into(), "/usr/bin:/bin".into())];
        for index in 0..6 {
            variables.push((format!("EXTRA_{index}").into(), "x".repeat(12_000).into()));
        }
        let (environment, omitted) = collect_exported_environment(variables);
        assert_eq!(
            environment.get("PATH").map(Vec::as_slice),
            Some(b"/usr/bin:/bin".as_slice())
        );
        assert!(!omitted.is_empty());
        assert!(marsh_contracts::validate_exported_environment(&environment).is_ok());
    }
}
