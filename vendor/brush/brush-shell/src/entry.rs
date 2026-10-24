//! Implements the command-line interface for the `brush` shell.

use crate::args::CommandLineArgs;
use crate::args::InputBackendType;
use crate::brushctl::ShellBuilderBrushBuiltinExt as _;
use crate::bundled;

static MARSH_EXTENSIONS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether to run on a single-threaded runtime (see [`use_current_thread_runtime`]).
static CURRENT_THREAD_RUNTIME: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static MARSH_EXTERNAL_COMMANDS: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

/// Enables the marsh parser and evaluator additions for subsequently created shells.
/// Embedders must opt in explicitly; ordinary Brush behavior is unchanged.
pub fn enable_marsh_extensions() {
    MARSH_EXTENSIONS.store(true, std::sync::atomic::Ordering::Release);
}

/// Runs the shell on a single-threaded Tokio runtime, whose I/O and signal
/// drivers then only run when the shell waits. Tests use it to make the
/// runtime's dispatch of a signal come at a known point.
pub fn use_current_thread_runtime() {
    CURRENT_THREAD_RUNTIME.store(true, std::sync::atomic::Ordering::Release);
}

/// Export a session's command directory and authority context to descendants.
/// The directory is also restored after startup files have run.
pub fn install_marsh_external_commands(path: String, session: String) {
    let _ = MARSH_EXTERNAL_COMMANDS.set((path, session));
}
use crate::config;
use crate::error_formatter;
use crate::events;
use crate::productinfo;
use brush_builtins::ShellBuilderExt as _;
#[cfg(feature = "experimental-builtins")]
use brush_experimental_builtins::ShellBuilderExt as _;
use clap::CommandFactory;
use std::io::IsTerminal;
use std::sync::LazyLock;
use std::{path::Path, sync::Arc};
use tokio::sync::Mutex;

static TRACE_EVENT_CONFIG: LazyLock<Arc<tokio::sync::Mutex<Option<events::TraceEventConfig>>>> =
    LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(None)));

type BrushShellExtensions = brush_core::extensions::ShellExtensionsImpl<error_formatter::Formatter>;
type BrushShell = brush_core::Shell<BrushShellExtensions>;

// WARN: this implementation shadows `clap::Parser::parse_from` one so it must be defined
// after the `use clap::Parser`
/// Applies Bash's startup option order to the `set` and `shopt` options: like
/// `set`, `-x`/`+x` letters, `-o`/`+o NAME` and `-O`/`+O NAME` (also inside a
/// cluster, as in `-oe errexit`) take effect left to right, so each option gets
/// its final state, which this passes on as `-o NAME`/`--+o NAME` (and `-O`/
/// `--+O`) for clap. Other letters (`c`, `s`, `i`, `l`, ...) stay, whichever
/// sign they had, and a repeated `c` is the same flag. A lone `-` ends the
/// options (as `--`); a lone `+` does nothing. The scan stops at the first
/// operand or `--`, leaving command strings and script arguments untouched.
/// An unknown `-o` or `-O` name, or a `-c` with no command, is Bash's error
/// (returned for the caller to print; the shell then exits with status 2).
fn bash_startup_options(args: &mut Vec<String>) -> Result<(), String> {
    const LONG_VALUE_OPTIONS: &[&str] = &[
        "--config",
        "--inherit-fd",
        "--closed-fd",
        "--rcfile",
        "--init-file",
        "--xtrace-file",
        "--input-backend",
        "--load",
        "--debug",
        "--log-enable",
        "--disable-event",
        "--log-disable",
    ];
    // `set`'s option letters, by the `set -o` names Brush knows them as.
    const LETTERS: &[(char, &str)] = &[
        ('a', "allexport"),
        ('b', "notify"),
        ('e', "errexit"),
        ('f', "noglob"),
        ('h', "hashall"),
        ('k', "keyword"),
        ('m', "monitor"),
        ('n', "noexec"),
        ('p', "privileged"),
        ('t', "onecmd"),
        ('u', "nounset"),
        ('v', "verbose"),
        ('x', "xtrace"),
        ('B', "braceexpand"),
        ('C', "noclobber"),
        ('E', "errtrace"),
        ('H', "histexpand"),
        ('P', "physical"),
        ('T', "functrace"),
    ];

    let mut kept: Vec<String> = vec![args[0].clone()];
    let mut set_o: Vec<(String, bool)> = vec![];
    let mut shopt: Vec<(String, bool)> = vec![];
    let mut seen_c = false;
    // `-i`/`+i`: the last one wins.
    let mut interactive = None;
    let mut index = 1;
    while let Some(arg) = args.get(index).cloned() {
        index += 1;
        if arg == "--" || !(arg.starts_with('-') || arg.starts_with('+')) {
            index -= 1;
            break;
        }
        if arg == "-" {
            kept.push("--".to_owned());
            break;
        }
        if arg == "+" {
            continue;
        }
        if arg.starts_with("--") {
            let takes_value = LONG_VALUE_OPTIONS.contains(&arg.as_str());
            kept.push(arg);
            if takes_value && let Some(value) = args.get(index) {
                kept.push(value.clone());
                index += 1;
            }
            continue;
        }
        let on = arg.starts_with('-');
        let mut rest = String::from("-");
        for letter in arg.chars().skip(1) {
            if let Some((_, name)) = LETTERS.iter().find(|(l, _)| *l == letter) {
                set_o.push(((*name).to_owned(), on));
            } else if letter == 'o' || letter == 'O' {
                let Some(name) = args.get(index).cloned() else {
                    rest.push(letter);
                    continue;
                };
                index += 1;
                if letter == 'o' {
                    set_o.push((name, on));
                } else {
                    shopt.push((name, on));
                }
            } else if letter == 'c' {
                if !std::mem::replace(&mut seen_c, true) {
                    rest.push(letter);
                }
            } else if letter == 'i' {
                interactive = Some(on);
            } else {
                rest.push(letter);
            }
        }
        if rest.len() > 1 {
            kept.push(rest);
        }
    }

    let name = args[0].as_str();
    let set_o_names =
        brush_core::namedoptions::options(brush_core::namedoptions::ShellOptionKind::SetO);
    if let Some((bad, _)) = set_o
        .iter()
        .find(|(option, _)| set_o_names.get(option).is_none())
    {
        return Err(std::format!(
            "{name}: line 0: {name}: {bad}: invalid option name"
        ));
    }
    let shopt_names =
        brush_core::namedoptions::options(brush_core::namedoptions::ShellOptionKind::Shopt);
    if let Some((bad, _)) = shopt
        .iter()
        .find(|(option, _)| shopt_names.get(option).is_none())
    {
        return Err(std::format!(
            "{name}: line 0: {bad}: invalid shell option name"
        ));
    }
    let command_follows = match args.get(index).map(String::as_str) {
        Some("--") => args.len() > index + 1,
        operand => operand.is_some(),
    };
    if seen_c && !command_follows {
        return Err(std::format!("{name}: -c: option requires an argument"));
    }
    if interactive == Some(true) {
        kept.push("-i".to_owned());
    }

    // Each option's final state, in the order the options first appear.
    let finals = |ops: Vec<(String, bool)>| {
        let mut finals: Vec<(String, bool)> = vec![];
        for (name, on) in ops {
            match finals.iter_mut().find(|(seen, _)| *seen == name) {
                Some(entry) => entry.1 = on,
                None => finals.push((name, on)),
            }
        }
        finals
    };
    for (name, on) in finals(set_o) {
        kept.push(if on { "-o" } else { "--+o" }.to_owned());
        kept.push(name);
    }
    for (name, on) in finals(shopt) {
        kept.push(if on { "-O" } else { "--+O" }.to_owned());
        kept.push(name);
    }
    kept.extend(args.drain(index.min(args.len())..));
    *args = kept;
    Ok(())
}

impl CommandLineArgs {
    // This function takes precedence over [`clap::Parser::parse_from`]
    pub(crate) fn try_parse_from(
        itr: impl IntoIterator<Item = String>,
    ) -> Result<Self, clap::Error> {
        let (mut this, script_args) =
            brush_clap_adapter::try_parse_before_double_dash::<Self>(itr)?;

        // Collect any args from after `--` (handled by try_parse_before_double_dash) into
        // script_args, which become positional parameters ($0, $1, ...).
        let mut script_args = script_args.into_iter().peekable();

        // Bash stops option parsing at the first non-option word: a script path
        // or the `-c` command string. A `--` seen before either is the option
        // terminator and is dropped (`bash -s -- a`, `bash -- script.sh`); one
        // seen after is an ordinary positional (`bash script.sh -- a`).
        if this.script_args.is_empty() {
            script_args.next_if(|a| a == "--");
        }
        this.script_args.extend(script_args);

        // With `-c`, bash takes the command string from the first operand; the
        // operands after it keep their usual meaning, so the next one sets `$0`
        // and the rest become positional parameters.
        if this.command_string_mode {
            if this.script_args.is_empty() {
                return Err(Self::command().error(
                    clap::error::ErrorKind::MissingRequiredArgument,
                    "-c: option requires an argument",
                ));
            }
            this.command = Some(this.script_args.remove(0));
        }

        Ok(this)
    }
}

/// Selects the executable (with leading arguments) for ENOEXEC files.
///
/// [`run`] selects the running executable; embedders whose executable does
/// not accept Brush arguments directly install their own.
pub fn install_script_interpreter(path: &Path, prefix_args: &[std::ffi::OsString]) {
    #[cfg(unix)]
    let _ = brush_core::sys::commands::set_script_interpreter(path, prefix_args);
    #[cfg(not(unix))]
    let _ = (path, prefix_args);
}

/// Main entry point for the `brush` shell.
pub fn run() {
    if let Ok(path) = std::env::current_exe() {
        install_script_interpreter(&path, &[]);
    }
    std::process::exit(run_from(
        std::env::args_os()
            .map(|arg| brush_core::bytes::from_os_str(&arg).into_owned())
            .collect(),
    ));
}

/// Main entry point with an explicitly supplied argument vector.
///
/// Embedders use this after consuming their own leading options. The first
/// element remains the shell's `argv[0]`.
pub fn run_from(mut args: Vec<String>) -> i32 {
    //
    // Install the bundled-command registry so it's available both for
    // bundled dispatch (handled next) and for builtin shim registration
    // during shell construction. With no bundled-providing features enabled
    // the registry is empty and both code paths become no-ops.
    //
    bundled::install_default_providers();

    //
    // If we were invoked as `brush <DISPATCH_FLAG> <name> [args...]`, run the
    // bundled command and exit before doing any shell setup. This is the
    // hot path for in-binary coreutils invocations.
    //
    if let Some(code) = bundled::maybe_dispatch_from(args.iter().cloned()) {
        return code;
    }

    //
    // Install panic handlers to clean up on panic.
    //
    install_panic_handlers();

    //
    // Parse args.
    //
    // Bash's startup option order (and `+` options, which clap lacks).
    if let Err(message) = bash_startup_options(&mut args) {
        eprintln!("{message}");
        return 2;
    }

    let parsed_args = match CommandLineArgs::try_parse_from(args.iter().cloned()) {
        Ok(parsed_args) => parsed_args,
        Err(e) => {
            let _ = e.print();

            // Check for whether this is something we'd truly consider fatal. clap returns
            // errors for `--help`, `--version`, etc.
            let exit_code = match e.kind() {
                clap::error::ErrorKind::DisplayVersion => 0,
                clap::error::ErrorKind::DisplayHelp => 0,
                _ => 2,
            };

            return exit_code;
        }
    };

    //
    // Run.
    //
    #[cfg(any(unix, windows))]
    let mut builder = if CURRENT_THREAD_RUNTIME.load(std::sync::atomic::Ordering::Acquire) {
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    #[cfg(not(any(unix, windows)))]
    let mut builder = tokio::runtime::Builder::new_current_thread();

    // Real-process subshells need to know which descriptors the runtime opens.
    brush_core::subshell::note_descriptors_before_runtime();
    let Ok(runtime) = builder.enable_all().build() else {
        tracing::error!("error: failed to create Tokio runtime");
        return 1;
    };
    brush_core::subshell::note_descriptors_after_runtime();

    let result = runtime.block_on(run_async(&args, parsed_args));
    // A canceled asynchronous stdin read may still have a blocking read in
    // Tokio's stdin worker. The shell has finished; do not wait for another
    // input line during runtime shutdown (notably after an idle TERM trap).
    runtime.shutdown_background();

    let exit_code = match result {
        Ok(code) => code,
        Err(err) => {
            tracing::error!("error: {err:#}");
            1
        }
    };

    i32::from(exit_code)
}

/// Installs panic handlers to report our panic and cleanly exit on panic.
fn install_panic_handlers() {
    //
    // Set up panic handler. On release builds, it will capture panic details to a
    // temporary .toml file and report a human-readable message to the screen.
    //
    human_panic::setup_panic!(
        human_panic::Metadata::new(productinfo::PRODUCT_NAME, productinfo::PRODUCT_VERSION)
            .homepage(env!("CARGO_PKG_HOMEPAGE"))
            .support("please post a GitHub issue at https://github.com/reubeno/brush/issues/new")
    );

    //
    // If stdout is connected to a terminal, then register a new panic handler that
    // resets the terminal and then invokes the previously registered handler. In
    // dev/debug builds, the previously registered handler will be the default
    // handler; in release builds, it will be the one registered by `human_panic`.
    //
    if std::io::stdout().is_terminal() {
        let original_panic_handler = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            // Best-effort attempt to reset the terminal to defaults.
            let _ = try_reset_terminal_to_defaults();

            // Invoke the original handler
            original_panic_handler(panic_info);
        }));
    }
}

#[cfg(feature = "experimental")]
pub(crate) const DEFAULT_ENABLE_HIGHLIGHTING: bool = true;
#[cfg(not(feature = "experimental"))]
pub(crate) const DEFAULT_ENABLE_HIGHLIGHTING: bool = false;

/// Run the brush shell. Returns the exit code.
///
/// # Arguments
///
/// * `cli_args` - The command-line arguments to the shell, in string form.
/// * `args` - The already-parsed command-line arguments.
#[doc(hidden)]
async fn run_async(
    cli_args: &[String],
    args: CommandLineArgs,
) -> Result<u8, brush_interactive::ShellError> {
    // Initializing tracing.
    let mut event_config = TRACE_EVENT_CONFIG.lock().await;
    *event_config = Some(events::TraceEventConfig::init(
        &args.enabled_debug_events,
        &args.disabled_events,
    ));
    drop(event_config);

    // Load configuration file.
    let file_config = config::load_config(args.no_config, args.config_file.as_deref())
        .into_config_or_log()
        .map_err(|e| brush_interactive::ShellError::IoError(std::io::Error::other(e)))?;

    // Instantiate an appropriately configured shell and wrap it in an `Arc`. Note that we do
    // *not* run any code in the shell yet. We'll delay loading profiles and such until after
    // we've set up everything else (in `run_in_shell`).
    let shell: BrushShell = instantiate_shell(&args, cli_args).await?;
    let shell = Arc::new(Mutex::new(shell));

    // Run with the selected input backend. Each branch instantiates the concrete
    // backend type and calls `run_in_shell`, preserving static dispatch.
    let default_backend = get_default_input_backend_type(&args);
    // Reedline needs a terminal; without one, read as the minimal backend does.
    let selected_backend = match args.input_backend {
        Some(InputBackendType::Reedline) if !std::io::stdin().is_terminal() => {
            InputBackendType::Minimal
        }
        selected => selected.unwrap_or(default_backend),
    };

    // Build UI options by merging config file with CLI args.
    #[allow(unused_variables, reason = "not used when no backend features enabled")]
    let ui_options = file_config.to_ui_options(&args);

    let result = match selected_backend {
        #[cfg(reedline_backend)]
        InputBackendType::Reedline => {
            let mut input_backend =
                brush_interactive::ReedlineInputBackend::new(&ui_options, &shell)?;
            run_in_shell(&shell, args, &mut input_backend, &ui_options).await
        }
        #[cfg(not(reedline_backend))]
        InputBackendType::Reedline => Err(brush_interactive::ShellError::InputBackendNotSupported),

        #[cfg(basic_backend)]
        InputBackendType::Basic => {
            let mut input_backend = brush_interactive::VtInputBackend::default().with_basic_style();
            let mut locked_shell = shell.lock().await;
            input_backend.install_key_bindings(&mut locked_shell);
            drop(locked_shell);
            run_in_shell(&shell, args, &mut input_backend, &ui_options).await
        }
        #[cfg(not(basic_backend))]
        InputBackendType::Basic => Err(brush_interactive::ShellError::InputBackendNotSupported),

        #[cfg(minimal_backend)]
        InputBackendType::Minimal => {
            let mut input_backend = brush_interactive::MinimalInputBackend::default();
            run_in_shell(&shell, args, &mut input_backend, &ui_options).await
        }
        #[cfg(not(minimal_backend))]
        InputBackendType::Minimal => Err(brush_interactive::ShellError::InputBackendNotSupported),

        #[cfg(vt_backend)]
        InputBackendType::Vt => {
            let mut input_backend =
                brush_interactive::VtInputBackend::default().with_ui_options(&ui_options);
            let mut locked_shell = shell.lock().await;
            input_backend.install_key_bindings(&mut locked_shell);
            drop(locked_shell);
            run_in_shell(&shell, args, &mut input_backend, &ui_options).await
        }
        #[cfg(not(vt_backend))]
        InputBackendType::Vt => Err(brush_interactive::ShellError::InputBackendNotSupported),
    };

    // Display any error that percolated up.
    let exit_code = match result {
        Ok(code) => code,
        Err(brush_interactive::ShellError::ShellError(e)) => {
            let shell = shell.lock().await;
            let mut stderr = shell.stderr();
            let _ = shell.display_error(&mut stderr, &e);
            drop(shell);
            1
        }
        Err(err) => {
            tracing::error!("error: {err:#}");
            1
        }
    };

    Ok(exit_code)
}

/// Runs the shell according to the provided command-line arguments.
/// Also responsible for loading profiles and rc files as appropriate.
///
/// # Arguments
///
/// * `shell_ref` - A reference to the shell to run.
/// * `args` - The parsed command-line arguments.
/// * `input_backend` - The input backend to use.
/// * `ui_options` - The user interface options to use.
async fn run_in_shell(
    shell_ref: &brush_interactive::ShellRef<impl brush_core::ShellExtensions>,
    args: CommandLineArgs,
    input_backend: &mut impl brush_interactive::InputBackend,
    ui_options: &brush_interactive::UIOptions,
) -> Result<u8, brush_interactive::ShellError> {
    let read_commands_from_stdin = args.will_read_commands_from_stdin();
    let interactive_options: brush_interactive::InteractiveOptions = ui_options.into();

    // Before config files run, so they (and what they source) observe it -- and only for a
    // shell that goes on to read commands interactively below, since only such a shell
    // dispatches hooks.
    if read_commands_from_stdin {
        brush_interactive::init_zsh_style_hooks(
            &mut *shell_ref.lock().await,
            &interactive_options,
        )?;
    }

    // Like Bash, an interactive shell without a terminal (standard error is not
    // one and there is no controlling terminal) says so and runs without job
    // control.
    #[cfg(unix)]
    {
        let mut shell = shell_ref.lock().await;
        if shell.options().interactive
            && !std::io::stderr().is_terminal()
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .is_err()
        {
            shell.options_mut().enable_job_control = false;
            let name = shell.current_shell_name().unwrap_or_default().into_owned();
            let name = name.rsplit('/').next().unwrap_or_default();
            let _ = std::io::Write::write_fmt(
                &mut shell.stderr(),
                format_args!(
                    "{name}: cannot set terminal process group (-1): Inappropriate ioctl for device\n\
                 {name}: no job control in this shell\n"
                ),
            );
        }
    }

    // Load profile and rc files as appropriate.
    initialize_shell(shell_ref, &args).await?;
    bundled::notify_initialized();

    // An interactive shell running `-c` or a script operand still uses job
    // control, so it must own the terminal before starting foreground process
    // groups. (The stdin-reading path acquires it in `InteractiveShell`.)
    let _terminal_control = if !read_commands_from_stdin
        && shell_ref.lock().await.options().interactive
        && std::io::stdin().is_terminal()
    {
        Some(brush_core::terminal::TerminalControl::acquire()?)
    } else {
        None
    };

    // If a command was specified via -c, then run that command and then exit. Its result
    // is also the shell's last exit status, which is what's returned below for every mode.
    if let Some(command) = args.command {
        let _ = shell_ref.lock().await.run_dash_c_command(command).await?;

    // Otherwise read commands interactively: -s was given (positional args become parameters;
    // a script named among them is *not* run), or nothing to run was specified.
    } else if read_commands_from_stdin {
        brush_interactive::InteractiveShell::new(shell_ref, input_backend, &interactive_options)?
            .run_interactively()
            .await?;

    // Otherwise a script path was given; run it. As with -c, its status is returned below.
    } else {
        let _ = shell_ref
            .lock()
            .await
            .run_script(
                brush_core::bytes::to_path(&args.script_args[0]),
                args.script_args.iter().skip(1),
            )
            .await?;
    }

    // Make sure to return the last result observed in the shell.
    let result = shell_ref.lock().await.last_exit_status();

    Ok(result)
}

/// Initializes a shell by loading profile and rc files as appropriate.
///
/// # Arguments
///
/// * `shell_ref` - A reference to the shell to initialize.
/// * `args` - The parsed command-line arguments.
async fn initialize_shell(
    shell_ref: &brush_interactive::ShellRef<impl brush_core::ShellExtensions>,
    args: &CommandLineArgs,
) -> Result<(), brush_interactive::ShellError> {
    // Compute desired profile-loading behavior.
    let profile = if args.no_profile {
        brush_core::ProfileLoadBehavior::Skip
    } else {
        brush_core::ProfileLoadBehavior::LoadDefault
    };

    // Compute desired rc-loading behavior.
    let rc = if args.no_rc {
        brush_core::RcLoadBehavior::Skip
    } else if let Some(rc_file) = &args.rc_file {
        brush_core::RcLoadBehavior::LoadCustom(rc_file.clone())
    } else {
        brush_core::RcLoadBehavior::LoadDefault
    };

    shell_ref.lock().await.load_config(&profile, &rc).await?;
    if let Some((path, _)) = MARSH_EXTERNAL_COMMANDS.get() {
        let mut shell = shell_ref.lock().await;
        let current = shell.env_str("PATH").map(|value| value.into_owned());
        let value = prepend_external_path(path, current.as_deref());
        let mut variable = brush_core::ShellVariable::new(value);
        variable.export();
        shell.set_env_global("PATH", variable)?;
    }

    Ok(())
}

/// Instantiates a shell from command-line arguments. Does *not* run any code in the shell.
///
/// # Arguments
///
/// * `args` - The parsed command-line arguments.
/// * `cli_args` - The raw command-line arguments.
async fn instantiate_shell(
    args: &CommandLineArgs,
    cli_args: &[String],
) -> Result<BrushShell, brush_interactive::ShellError> {
    #[cfg(feature = "experimental-load")]
    let mut shell = if let Some(load_file) = &args.load_file {
        instantiate_shell_from_file(load_file.as_path())?
    } else {
        instantiate_shell_from_args(args, cli_args).await?
    };

    #[cfg(not(feature = "experimental-load"))]
    let mut shell = instantiate_shell_from_args(args, cli_args).await?;

    // Register shims for any bundled commands in the installed registry.
    // Done here (not inside the inner instantiators) so both paths are
    // covered from a single site.
    bundled::register_shims(&mut shell);

    Ok(shell)
}

#[cfg(feature = "experimental-load")]
fn instantiate_shell_from_file(
    file_path: &Path,
) -> Result<BrushShell, brush_interactive::ShellError> {
    let mut shell: BrushShell = serde_json::from_reader(std::fs::File::open(file_path)?)
        .map_err(|e| brush_interactive::ShellError::IoError(std::io::Error::other(e)))?;

    // NOTE: We need to manually register builtins because we can't serialize/deserialize them.
    // TODO(serde): we should consider whether we could/should at least track *which* are enabled.
    let builtin_set = if shell.options().sh_mode {
        brush_builtins::BuiltinSet::ShMode
    } else {
        brush_builtins::BuiltinSet::BashMode
    };

    let builtins = brush_builtins::default_builtins(builtin_set);

    for (builtin_name, builtin) in builtins {
        shell.builtins_mut().register(builtin_name, builtin);
    }

    // Add experimental builtins (if enabled).
    #[cfg(feature = "experimental-builtins")]
    for (builtin_name, builtin) in brush_experimental_builtins::experimental_builtins() {
        shell.builtins_mut().register(builtin_name, builtin);
    }

    Ok(shell)
}

/// Instantiates a shell from command-line arguments. Does *not* run any code in the shell.
///
/// # Arguments
///
/// * `args` - The parsed command-line arguments.
/// * `cli_args` - The raw command-line arguments.
async fn instantiate_shell_from_args(
    args: &CommandLineArgs,
    cli_args: &[String],
) -> Result<BrushShell, brush_interactive::ShellError> {
    // Compute login flag.
    let login = args.login || cli_args.first().is_some_and(|argv0| argv0.starts_with('-'));

    // Compute shell name.
    let shell_name = if args.command.is_some() && !args.script_args.is_empty() {
        Some(args.script_args[0].clone())
    } else if !cli_args.is_empty() {
        Some(cli_args[0].clone())
    } else if args.sh_mode {
        // Simulate having been run as "sh".
        Some(String::from("sh"))
    } else {
        None
    };

    // Compute positional shell arguments.
    let shell_args = if args.command.is_some() {
        Some(args.script_args.iter().skip(1).cloned().collect())
    } else if args.read_commands_from_stdin {
        Some(args.script_args.clone())
    } else {
        None
    };

    // Commands are read from stdin if -s was provided, or if no command was specified (either via
    // -c or as a positional argument).
    let read_commands_from_stdin = args.will_read_commands_from_stdin();

    let builtin_set = if args.sh_mode {
        brush_builtins::BuiltinSet::ShMode
    } else {
        brush_builtins::BuiltinSet::BashMode
    };

    // Standard descriptors the parent closed stay closed.
    for fd in &args.closed_fds {
        brush_core::subshell::mark_standard_descriptor_closed(*fd);
    }

    // Identify the file descriptors to inherit.
    let fds = args
        .inherited_fds
        .iter()
        .filter_map(|&fd| brush_core::sys::fd::try_get_file_for_open_fd(fd).map(|file| (fd, file)))
        .collect();

    // Select parser implementation to use.
    #[cfg(feature = "experimental-parser")]
    let parser_impl = if args.experimental_parser {
        brush_core::parser::ParserImpl::Winnow
    } else {
        brush_core::parser::ParserImpl::Peg
    };

    #[cfg(not(feature = "experimental-parser"))]
    let parser_impl = brush_core::parser::ParserImpl::Peg;

    // Set up the shell builder with the requested options.
    // NOTE: We skip loading profile and rc files here; that will be handled later after we've
    // fully instantiated everything we want set before running any code.
    let shell = brush_core::Shell::builder_with_extensions::<BrushShellExtensions>()
        .marsh_extensions(MARSH_EXTENSIONS.load(std::sync::atomic::Ordering::Acquire))
        .disable_options(args.disabled_options.clone())
        .disable_shopt_options(args.disabled_shopt_options.clone())
        .disallow_overwriting_regular_files_via_output_redirection(
            args.disallow_overwriting_regular_files_via_output_redirection,
        )
        .enable_options(args.enabled_options.clone())
        .enable_shopt_options(args.enabled_shopt_options.clone())
        .do_not_execute_commands(args.do_not_execute_commands)
        .exit_after_one_command(args.exit_after_one_command)
        .login(login)
        .interactive(args.is_interactive())
        .command_string_mode(args.command.is_some())
        .no_editing(args.no_editing)
        .profile(brush_core::ProfileLoadBehavior::Skip)
        .rc(brush_core::RcLoadBehavior::Skip)
        .do_not_inherit_env(args.do_not_inherit_env)
        .fds(fds)
        .maybe_shell_args(shell_args)
        .posix(args.posix || args.sh_mode)
        .print_commands_and_arguments(args.print_commands_and_arguments)
        .read_commands_from_stdin(read_commands_from_stdin)
        .maybe_shell_name(shell_name)
        .shell_product_display_str(productinfo::get_product_display_str())
        .sh_mode(args.sh_mode)
        .treat_unset_variables_as_error(args.treat_unset_variables_as_error)
        .exit_on_nonzero_command_exit(args.exit_on_nonzero_command_exit)
        .disable_pathname_expansion(args.disable_pathname_expansion)
        .verbose(args.verbose)
        .parser(parser_impl)
        .error_formatter(new_error_behavior(args))
        .shell_version(env!("CARGO_PKG_VERSION").to_string());

    // Add builtins.
    let shell = shell.default_builtins(builtin_set).brush_builtins();

    // Add experimental builtins (if enabled).
    #[cfg(feature = "experimental-builtins")]
    let shell = shell.experimental_builtins();

    // Build the shell.
    let mut shell = shell.build().await?;

    // Like Bash, the shell ignores SIGQUIT.
    #[cfg(unix)]
    brush_core::traps::ignore_quit();

    if let Some((path, session)) = MARSH_EXTERNAL_COMMANDS.get() {
        let current = shell.env_str("PATH").map(|value| value.into_owned());
        let value = prepend_external_path(path, current.as_deref());
        let mut variable = brush_core::ShellVariable::new(value);
        variable.export();
        shell.set_env_global("PATH", variable)?;
        let mut variable = brush_core::ShellVariable::new(session.clone());
        variable.export();
        shell.set_env_global("MARSH_EXTERNAL_SESSION", variable)?;
    }

    // Make adjustments.
    if let Some(xtrace_file_path) = &args.xtrace_file_path {
        enable_xtrace_to_file(&mut shell, xtrace_file_path)?;
    }

    Ok(shell)
}

fn prepend_external_path(directory: &str, current: Option<&str>) -> String {
    let mut entries = vec![directory];
    if let Some(current) = current {
        entries.extend(current.split(':').filter(|entry| *entry != directory));
    }
    entries.join(":")
}

fn enable_xtrace_to_file(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    file_path: &Path,
) -> Result<(), brush_interactive::ShellError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(file_path)
        .map_err(|e| {
            brush_interactive::ShellError::FailedToCreateXtraceFile(file_path.to_path_buf(), e)
        })?;

    let file = brush_core::openfiles::OpenFile::from(file);
    let file_fd = shell.open_files_mut().add(file)?;

    shell.options_mut().print_commands_and_arguments = true;
    shell.set_env_global(
        "BASH_XTRACEFD",
        brush_core::ShellVariable::new(file_fd.to_string()),
    )?;

    Ok(())
}

const fn new_error_behavior(args: &CommandLineArgs) -> error_formatter::Formatter {
    error_formatter::Formatter {
        use_color: !args.disable_color,
    }
}

// The default input backend must be one this build offers.
#[cfg(not(any(reedline_backend, vt_backend, basic_backend, minimal_backend)))]
compile_error!("no input backend is enabled that works on this target; enable `minimal`");

/// Whether this build offers the given input backend; see `build.rs`.
const fn is_input_backend_available(backend: InputBackendType) -> bool {
    match backend {
        InputBackendType::Reedline => cfg!(reedline_backend),
        InputBackendType::Basic => cfg!(basic_backend),
        InputBackendType::Minimal => cfg!(minimal_backend),
        InputBackendType::Vt => cfg!(vt_backend),
    }
}

fn get_default_input_backend_type(args: &CommandLineArgs) -> InputBackendType {
    default_input_backend_type(
        std::io::stdin().is_terminal() && args.will_read_commands_from_stdin(),
    )
}

/// Returns the input backend to use if none is requested: the first this build
/// offers, of the richest for commands typed at a terminal, or else the simplest.
///
/// # Arguments
///
/// * `at_terminal` - Whether commands are typed at a terminal.
fn default_input_backend_type(at_terminal: bool) -> InputBackendType {
    use InputBackendType as B;

    // If stdin isn't a terminal, then `reedline` doesn't do the right thing
    // (reference: https://github.com/nushell/reedline/issues/509), so it comes last.
    let preferred = if at_terminal {
        [B::Reedline, B::Vt, B::Basic, B::Minimal]
    } else {
        [B::Minimal, B::Vt, B::Basic, B::Reedline]
    };

    // Some backend is always offered (see the `compile_error!` above).
    preferred
        .into_iter()
        .find(|backend| is_input_backend_available(*backend))
        .unwrap_or(B::Minimal)
}

pub(crate) fn get_event_config() -> Arc<tokio::sync::Mutex<Option<events::TraceEventConfig>>> {
    TRACE_EVENT_CONFIG.clone()
}

fn try_reset_terminal_to_defaults() -> Result<(), std::io::Error> {
    #[cfg(any(unix, windows))]
    {
        // Reset the console.
        let exec_result = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::terminal::EnableLineWrap,
            crossterm::style::ResetColor,
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show,
            crossterm::cursor::MoveToNextLine(1),
        );

        let raw_result = crossterm::terminal::disable_raw_mode();

        exec_result?;
        raw_result?;
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use anyhow::Result;
    use pretty_assertions::{assert_eq, assert_matches};

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_empty_args() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush"]))?;
        assert_matches!(parsed_args.script_args.as_slice(), []);
        Ok(())
    }

    #[test]
    fn parse_script_and_args() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&[
            "brush",
            "some-script",
            "-x",
            "1",
            "--option",
        ]))?;
        assert_eq!(
            parsed_args.script_args,
            ["some-script", "-x", "1", "--option"]
        );
        Ok(())
    }

    #[test]
    fn parse_script_and_args_with_double_dash_in_script_args() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "some-script", "--"]))?;
        assert_eq!(parsed_args.script_args, ["some-script", "--"]);
        Ok(())
    }

    #[test]
    fn parse_unknown_args() {
        let result = CommandLineArgs::try_parse_from(args(&["brush", "--unknown-option"]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_c_with_double_dash_separator() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "--", "echo hello", "arg0"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert_eq!(parsed_args.script_args, ["arg0"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_double_dash_no_command() {
        assert!(CommandLineArgs::try_parse_from(args(&["brush", "-c", "--"])).is_err());
    }

    #[test]
    fn parse_c_with_double_dash_command_is_double_dash() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "--", "--", "echo", "hi"]))?;
        assert_eq!(parsed_args.command, Some("--".to_string()));
        assert_eq!(parsed_args.script_args, ["echo", "hi"]);
        Ok(())
    }

    #[test]
    fn parse_ec_with_double_dash_separator() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-ec", "--", "echo hello", "arg0"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert!(parsed_args.exit_on_nonzero_command_exit);
        assert_eq!(parsed_args.script_args, ["arg0"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_value_before_double_dash_unchanged() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "echo hi", "--", "arg0"]))?;
        assert_eq!(parsed_args.command, Some("echo hi".to_string()));
        assert_eq!(parsed_args.script_args, ["--", "arg0"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_option_before_command() -> Result<()> {
        // bash takes -c's command string from the first operand, so options may
        // sit between the two: `bash -c -l 'echo hello' arg0`.
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "-l", "echo hello", "arg0"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert!(parsed_args.login);
        assert_eq!(parsed_args.script_args, ["arg0"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_value_taking_option_before_command() -> Result<()> {
        // -o consumes its own value, so the first operand is still the command.
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "-o", "errexit", "echo hello"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert_eq!(parsed_args.enabled_options, ["errexit"]);
        assert_eq!(parsed_args.script_args, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn parse_ec_with_option_before_command() -> Result<()> {
        // Only the `c` leaves the combined group; -e still applies.
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-ec", "-l", "echo hello"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert!(parsed_args.exit_on_nonzero_command_exit);
        assert!(parsed_args.login);
        Ok(())
    }

    #[test]
    fn parse_c_with_several_options_before_command() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "-l", "-x", "echo hello"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert!(parsed_args.login);
        assert!(parsed_args.print_commands_and_arguments);
        assert_eq!(parsed_args.script_args, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn parse_c_after_value_taking_option() -> Result<()> {
        // "errexit" is -o's value rather than an operand, so option parsing
        // continues and the command is still the first operand.
        let parsed_args = CommandLineArgs::try_parse_from(args(&[
            "brush",
            "-o",
            "errexit",
            "-c",
            "-l",
            "echo hello",
        ]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert_eq!(parsed_args.enabled_options, ["errexit"]);
        assert!(parsed_args.login);
        Ok(())
    }

    #[test]
    fn parse_c_after_script_stays_a_script_argument() -> Result<()> {
        // Option parsing stops at the first operand, so these reach the script:
        // `bash announce.sh -c -l` passes "-c" and "-l" as $1 and $2.
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "announce.sh", "-c", "-l"]))?;
        assert!(parsed_args.command.is_none());
        assert!(!parsed_args.login);
        assert_eq!(parsed_args.script_args, ["announce.sh", "-c", "-l"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_double_dash_and_option_like_command() -> Result<()> {
        // After `--`, the next token is the command even when it looks like an
        // option: `bash -c -- -l 'echo zero'` runs `-l` with $0 of "echo zero".
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "--", "-l", "echo zero"]))?;
        assert_eq!(parsed_args.command, Some("-l".to_string()));
        assert!(!parsed_args.login);
        assert_eq!(parsed_args.script_args, ["echo zero"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_option_and_no_command() {
        // bash: "-c: option requires an argument", exit 2.
        assert!(CommandLineArgs::try_parse_from(args(&["brush", "-c", "-l"])).is_err());
    }

    #[test]
    fn parse_c_with_option_then_double_dash() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "-l", "--", "echo hello"]))?;
        assert_eq!(parsed_args.command, Some("echo hello".to_string()));
        assert!(parsed_args.login);
        assert_eq!(parsed_args.script_args, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn parse_c_with_option_and_doubled_double_dash() -> Result<()> {
        // Only the first `--` terminates options; the second is the command
        // operand, matching `bash -c -l -- -- echo hi`.
        let parsed_args = CommandLineArgs::try_parse_from(args(&[
            "brush", "-c", "-l", "--", "--", "echo", "hi",
        ]))?;
        assert_eq!(parsed_args.command, Some("--".to_string()));
        assert_eq!(parsed_args.script_args, ["echo", "hi"]);
        Ok(())
    }

    #[test]
    fn parse_c_combined_with_other_flags() -> Result<()> {
        // `-c` groups with other short flags in any order, as in `brush -cl "echo hi"`.
        for argv in [
            &["brush", "-cl", "echo hi", "name"][..],
            &["brush", "-lc", "echo hi", "name"][..],
            &["brush", "-c", "-l", "echo hi", "name"][..],
        ] {
            let parsed_args = CommandLineArgs::try_parse_from(args(argv))?;
            assert!(parsed_args.login, "for {argv:?}");
            assert_eq!(
                parsed_args.command,
                Some("echo hi".to_string()),
                "for {argv:?}"
            );
            assert_eq!(parsed_args.script_args, ["name"], "for {argv:?}");
        }
        Ok(())
    }

    #[test]
    fn parse_c_without_command_string() {
        assert!(CommandLineArgs::try_parse_from(args(&["brush", "-c"])).is_err());
    }

    #[test]
    fn parse_c_with_empty_command_string() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "-c", ""]))?;
        assert_eq!(parsed_args.command, Some(String::new()));
        Ok(())
    }

    #[test]
    fn parse_o_with_double_dash_is_not_transformed() {
        // Unlike -c, bash's -o consumes -- as its literal value (invalid option
        // name), not as an option terminator.
        let result = CommandLineArgs::try_parse_from(args(&["brush", "-o", "--"]));
        // try_parse_before_double_dash splits at --, so -o ends up
        // without a value and parsing correctly fails. The key assertion is
        // that we MUST NOT reinterpret -- as an option terminator for -o and
        // then take any later argument as its value.
        assert!(result.is_err());
    }

    #[test]
    fn parse_oc_is_o_with_an_attached_value() -> Result<()> {
        // To brush's parser, -oc is -o with attached value "c", not -o flag + -c
        // flag, so the -- is an ordinary option terminator here. NOTE: bash instead
        // takes the *next* word as -o's option name; see the known-failure compat
        // case "-oc with -- takes its option name from the next word".
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "-oc", "--", "echo"]))?;
        // -o consumed "c" as its value; -- ended the options; no -c command.
        assert!(parsed_args.command.is_none());
        assert_eq!(parsed_args.script_args, ["echo"]);
        Ok(())
    }

    #[test]
    fn parse_bool_flag_before_double_dash_not_transformed() -> Result<()> {
        // -e is a boolean flag, not -c. The -- ends the options, so
        // everything after it is positional (including -c).
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-e", "--", "-c", "echo"]))?;
        assert!(parsed_args.command.is_none());
        assert!(parsed_args.exit_on_nonzero_command_exit);
        assert_eq!(parsed_args.script_args, ["-c", "echo"]);
        Ok(())
    }

    #[test]
    fn parse_s_with_double_dash_separator() -> Result<()> {
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "-s", "--", "--", "a"]))?;
        assert!(parsed_args.read_commands_from_stdin);
        assert_eq!(parsed_args.script_args, ["--", "a"]);
        Ok(())
    }

    #[test]
    fn parse_double_dash_before_script() -> Result<()> {
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "--", "script.sh", "--", "x"]))?;
        assert_eq!(parsed_args.script_args, ["script.sh", "--", "x"]);
        Ok(())
    }

    #[test]
    fn parse_c_with_double_dash_and_later_double_dash() -> Result<()> {
        // After removing the first --, -c gets "echo". The second -- is
        // handled by try_parse_before_double_dash and appears in script_args.
        let parsed_args =
            CommandLineArgs::try_parse_from(args(&["brush", "-c", "--", "echo", "--", "more"]))?;
        assert_eq!(parsed_args.command, Some("echo".to_string()));
        assert_eq!(parsed_args.script_args, ["--", "more"]);
        Ok(())
    }

    #[test]
    fn parse_c_flag_group_edge_cases() -> Result<()> {
        // `-C` is a different flag, and `-oc` is `-o` with the value "c", so
        // neither puts the shell in command mode.
        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "-C", "script.sh"]))?;
        assert!(!parsed_args.command_string_mode);
        assert!(parsed_args.command.is_none());

        let parsed_args = CommandLineArgs::try_parse_from(args(&["brush", "-oc", "script.sh"]))?;
        assert!(!parsed_args.command_string_mode);
        assert_eq!(parsed_args.enabled_options, ["c"]);
        Ok(())
    }

    #[test]
    fn parse_c_in_bash_option_order() -> Result<()> {
        // Bash: `-c` is a flag; the command is the first operand after all options.
        for argv in [
            &["brush", "-c", "-l", "echo x", "a0", "a1"][..],
            &["brush", "-cl", "echo x", "a0", "a1"],
            &["brush", "-lc", "echo x", "a0", "a1"],
            &["brush", "--login", "-c", "echo x", "a0", "a1"],
            &["brush", "-c", "--login", "echo x", "a0", "a1"],
        ] {
            let parsed = CommandLineArgs::try_parse_from(args(argv))?;
            assert_eq!(parsed.command.as_deref(), Some("echo x"), "{argv:?}");
            assert!(parsed.login, "{argv:?}");
            assert_eq!(parsed.script_args, ["a0", "a1"], "{argv:?}");
        }
        let parsed = CommandLineArgs::try_parse_from(args(&["brush", "-ce", "echo x"]))?;
        assert_eq!(parsed.command.as_deref(), Some("echo x"));
        assert!(parsed.exit_on_nonzero_command_exit);
        let parsed = CommandLineArgs::try_parse_from(args(&["brush", "-c", "-e", "echo x"]))?;
        assert_eq!(parsed.command.as_deref(), Some("echo x"));
        assert!(parsed.exit_on_nonzero_command_exit);
        for argv in [
            &["brush", "-c", "-o", "pipefail", "echo x"][..],
            &["brush", "-o", "pipefail", "-c", "echo x"],
            &["brush", "-co", "pipefail", "echo x"],
        ] {
            let parsed = CommandLineArgs::try_parse_from(args(argv))?;
            assert_eq!(parsed.command.as_deref(), Some("echo x"), "{argv:?}");
            assert_eq!(parsed.enabled_options, ["pipefail"], "{argv:?}");
        }
        let parsed = CommandLineArgs::try_parse_from(args(&["brush", "-c", "-l", "--", "-x"]))?;
        assert_eq!(parsed.command.as_deref(), Some("-x"));
        assert!(parsed.login);
        // A command's own arguments are never options.
        let parsed = CommandLineArgs::try_parse_from(args(&["brush", "-c", "echo", "-l"]))?;
        assert_eq!(parsed.command.as_deref(), Some("echo"));
        assert!(!parsed.login);
        assert_eq!(parsed.script_args, ["-l"]);
        assert!(CommandLineArgs::try_parse_from(args(&["brush", "-c", "-l"])).is_err());
        Ok(())
    }

    #[test]
    fn defaults_to_an_available_input_backend() {
        for at_terminal in [true, false] {
            let backend = default_input_backend_type(at_terminal);
            assert!(is_input_backend_available(backend), "{backend:?}");
        }
    }

    #[cfg(reedline_backend)]
    #[test]
    fn defaults_to_reedline_at_a_terminal() {
        assert_eq!(default_input_backend_type(true), InputBackendType::Reedline);
    }

    #[cfg(all(not(reedline_backend), vt_backend))]
    #[test]
    fn defaults_to_vt_at_a_terminal_without_reedline() {
        assert_eq!(default_input_backend_type(true), InputBackendType::Vt);
    }

    #[cfg(minimal_backend)]
    #[test]
    fn defaults_to_minimal_away_from_a_terminal() {
        assert_eq!(default_input_backend_type(false), InputBackendType::Minimal);
    }
}
