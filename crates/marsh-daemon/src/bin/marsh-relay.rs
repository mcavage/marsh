use std::path::PathBuf;

const USAGE: &str = "usage: marsh-relay SOCKET TOKEN [DOCKER-SOCKET]";

fn main() {
    let _ = marsh_provenance::embed!();
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let mut arguments = arguments.into_iter().map(PathBuf::from);
    let Some(socket) = arguments.next() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let Some(token) = arguments.next() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    // The shell's services endpoint (E2): only for a project with services on.
    let endpoint = arguments.next();
    if arguments.next().is_some() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    if let Err(error) = marsh_daemon::relay::run_guest_with_endpoint(
        std::io::stdin(),
        std::io::stdout(),
        &socket,
        &token,
        endpoint.as_deref(),
    ) {
        eprintln!("marsh-relay: {error}");
        std::process::exit(1);
    }
}
