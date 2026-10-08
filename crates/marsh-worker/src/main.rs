use marsh_contracts::WORKER_CONTAINER_CAPACITY;
use marsh_runtime::DockerCliRuntime;
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
    if mode.as_deref() != Some(std::ffi::OsStr::new("--serve")) {
        return Err("usage: marsh-worker --serve GENERATION".into());
    }
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
    let worker = Arc::new(Worker::new(
        Arc::new(DockerCliRuntime::system()),
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
