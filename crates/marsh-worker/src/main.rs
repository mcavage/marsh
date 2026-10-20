use marsh_contracts::WORKER_CONTAINER_CAPACITY;
use marsh_runtime::{DockerCliRuntime, SbxRuntimeProfile};
use marsh_worker::{ThreadSupervisor, Worker, WorkerTransport, serve_retained};
use std::{fs::File, io, os::fd::AsFd, sync::Arc};

fn main() {
    if let Err(error) = run() {
        eprintln!("marsh-worker: {error}");
        std::process::exit(125);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next();
    let profile = match mode.as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--serve") => SbxRuntimeProfile::Local,
        Some("--serve-cloud-v2") => SbxRuntimeProfile::CloudV2,
        _ => return Err("usage: marsh-worker (--serve|--serve-cloud-v2) GENERATION".into()),
    };
    let generation = arguments
        .next()
        .ok_or("worker generation is required")?
        .to_str()
        .ok_or("worker generation must be UTF-8")?
        .parse::<u64>()?;
    if generation == 0 {
        return Err("worker server requires a positive generation".into());
    }
    if arguments.next().is_some() {
        return Err("unexpected worker arguments".into());
    }
    let runtime = match profile {
        SbxRuntimeProfile::Local => DockerCliRuntime::system(),
        SbxRuntimeProfile::CloudV2 => {
            if !rustix::process::geteuid().is_root() {
                return Err("cloud-v2 worker requires trusted root bootstrap".into());
            }
            DockerCliRuntime::system_with_profile(profile)?
        }
    };
    let worker = Arc::new(Worker::new(
        Arc::new(runtime),
        Arc::new(ThreadSupervisor::default()),
    ));
    let transport = WorkerTransport::from_files(
        File::from(io::stdin().as_fd().try_clone_to_owned()?),
        File::from(io::stdout().as_fd().try_clone_to_owned()?),
    )?;
    serve_retained(
        &worker,
        generation,
        usize::from(WORKER_CONTAINER_CAPACITY),
        transport,
    )?;
    Ok(())
}
