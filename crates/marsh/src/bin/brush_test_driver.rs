//! Test-only executable for isolating Brush compatibility from daemon tests.

use marsh::registered_commands::{
    Invocation, RegisteredCommandExecutor, RegisteredCommands, install_registered_commands,
};
use std::{io::Read, sync::Arc};

struct PlacementProbe;

impl RegisteredCommandExecutor for PlacementProbe {
    fn execute(&self, invocation: Invocation) -> i32 {
        if invocation.command == "env_probe" {
            use std::io::Write as _;
            let mut output = std::io::stdout().lock();
            let value = invocation
                .environment
                .get("PROBE_VALUE")
                .map_or(b"-".as_slice(), Vec::as_slice);
            let written = output.write_all(value).and_then(|()| {
                write!(
                    output,
                    ":{}:{}:{};",
                    invocation.environment.contains_key("MARSH_PLACE"),
                    invocation.environment.contains_key("MARSH_DAEMON_TOKEN"),
                    invocation.environment.contains_key("MCP_GATEWAY_URL")
                )
            });
            return if written.is_ok() { 0 } else { 125 };
        }
        let mut input = String::new();
        if std::io::stdin().read_to_string(&mut input).is_err() {
            return 125;
        }
        let placement = match invocation.placement {
            marsh_daemon::Placement::Local => "local",
        };
        print!("{placement}:{}:{input}", invocation.command);
        0
    }
}

fn main() {
    if let Ok(path) = std::env::var("MARSH_TEST_EXTERNAL_COMMANDS") {
        brush_shell::entry::install_marsh_external_commands(path, "test-session".into());
    }
    if std::env::var_os("MARSH_TEST_REGISTER_PLACE").is_some() {
        install_registered_commands(
            RegisteredCommands::new(["place_probe".into(), "env_probe".into()])
                .expect("valid probe names"),
            Arc::new(PlacementProbe),
        )
        .expect("install placement probe");
    }
    if std::env::var_os("MARSH_TEST_EXTENSIONS").is_some() {
        brush_shell::entry::enable_marsh_extensions();
    }
    brush_shell::entry::run();
}
