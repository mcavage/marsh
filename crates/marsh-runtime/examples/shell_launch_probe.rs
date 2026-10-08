//! Native decoder oracle. Root runs this against an owned private guest file;
//! this does not replace the public Brush admission and stock shell UAT.
use marsh_runtime::byte_exec::{Payload, decode_shell_launch};
use std::{collections::BTreeMap, io::Write, os::unix::ffi::OsStrExt};

fn probe() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let before = std::env::current_dir()?;
    let identity = (rustix::process::getuid(), rustix::process::geteuid());
    let (argv, cwd) = decode_shell_launch(&arguments)?;
    if std::env::current_dir()? != before
        || (rustix::process::getuid(), rustix::process::geteuid()) != identity
    {
        return Err("decoder changed process state".into());
    }
    let payload = Payload {
        argv: argv.iter().map(|word| word.as_bytes().to_vec()).collect(),
        environment: BTreeMap::new(),
        working_directory: cwd.map_or_else(Vec::new, |path| path.as_os_str().as_bytes().to_vec()),
    };
    // Independent Python producer checks exact output and untouched stdin.
    let mut output = std::io::stdout().lock();
    output.write_all(&payload.encode()?)?;
    std::io::copy(&mut std::io::stdin().lock(), &mut output)?;
    Ok(())
}

fn main() {
    if probe().is_err() {
        eprintln!("shell launch decoder probe rejected");
        std::process::exit(125);
    }
}
