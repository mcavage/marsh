use std::path::PathBuf;

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let mut arguments = arguments.into_iter().map(PathBuf::from);
    let Some(socket) = arguments.next() else {
        eprintln!("usage: marsh-relay SOCKET TOKEN");
        std::process::exit(2);
    };
    let Some(token) = arguments.next() else {
        eprintln!("usage: marsh-relay SOCKET TOKEN");
        std::process::exit(2);
    };
    if arguments.next().is_some() {
        eprintln!("usage: marsh-relay SOCKET TOKEN");
        std::process::exit(2);
    }
    if let Err(error) =
        marsh_daemon::relay::run_guest(std::io::stdin(), std::io::stdout(), &socket, &token)
    {
        eprintln!("marsh-relay: {error}");
        std::process::exit(1);
    }
}
