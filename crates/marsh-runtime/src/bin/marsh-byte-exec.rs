//! Fixed native final-boundary decoder; contains no shell implementation.
use marsh_runtime::byte_exec::{self, ExecError, Payload, PayloadFilePolicy};
use std::io::Write;

fn launch() -> Result<std::convert::Infallible, ExecError> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--payload")) {
        return Err(std::io::Error::other("invalid native byte launch").into());
    }
    let path = arguments
        .next()
        .ok_or_else(|| std::io::Error::other("missing native byte payload"))?;
    // Runtime supplies its source UID, never an exported environment value.
    // Direct native callers default to euid. Shell uses its root-only policy.
    let owner = match arguments.next() {
        None => rustix::process::geteuid().as_raw(),
        Some(flag) if flag == "--owner" => arguments
            .next()
            .and_then(|value| value.to_str().and_then(|value| value.parse::<u32>().ok()))
            .ok_or_else(|| std::io::Error::other("invalid native byte owner"))?,
        Some(_) => return Err(std::io::Error::other("invalid native byte launch").into()),
    };
    if arguments.next().is_some() {
        return Err(std::io::Error::other("invalid native byte launch").into());
    }
    let file = byte_exec::open_private_payload(
        rustix::fs::CWD,
        std::path::Path::new(&path),
        PayloadFilePolicy::Worker { owner },
    )?;
    let payload = Payload::read(&file)?;
    drop(file); // never leak a payload descriptor to the image command
    byte_exec::exec(payload)
}

fn main() {
    let error = match launch() {
        Ok(never) => match never {},
        Err(error) => error,
    };
    // Never display I/O paths, argv or env. Only native errno is safe.
    // A closed diagnostic consumer must not turn 125/126/127 into panic 101.
    let mut stderr = std::io::stderr().lock();
    let _ = match &error {
        ExecError::Preparation(_) => writeln!(stderr, "marsh: native byte launch failed"),
        ExecError::Execute(errno) => {
            writeln!(
                stderr,
                "marsh: native byte exec failed (errno {})",
                *errno as i32
            )
        }
    };
    std::process::exit(error.exit_code());
}
