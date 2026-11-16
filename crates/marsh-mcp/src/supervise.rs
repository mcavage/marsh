//! Bounded child execution and process-group supervision for host commands.

use super::{
    AsyncRead, AsyncReadExt, Command, CommandSpec, Duration, HostConfig, MAX_CAPTURE_BYTES, Stdio,
    apply_host_environment, watch,
};

#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
}

pub(crate) fn merge_output(mut first: CapturedOutput, second: &CapturedOutput) -> CapturedOutput {
    let stdout_remaining = MAX_CAPTURE_BYTES.saturating_sub(first.stdout.len());
    first
        .stdout
        .extend_from_slice(&second.stdout[..second.stdout.len().min(stdout_remaining)]);
    let stderr_remaining = MAX_CAPTURE_BYTES.saturating_sub(first.stderr.len());
    first
        .stderr
        .extend_from_slice(&second.stderr[..second.stderr.len().min(stderr_remaining)]);
    first.stdout_truncated |= second.stdout_truncated || second.stdout.len() > stdout_remaining;
    first.stderr_truncated |= second.stderr_truncated || second.stderr.len() > stderr_remaining;
    first.exit_code = second.exit_code;
    first
}

pub(crate) async fn run_bounded(
    config: &HostConfig,
    spec: &CommandSpec,
) -> Result<CapturedOutput, String> {
    let (_cancel, cancel_rx) = watch::channel(false);
    run_bounded_cancel(config, spec, cancel_rx).await
}

pub(crate) async fn run_bounded_cancel(
    config: &HostConfig,
    spec: &CommandSpec,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<CapturedOutput, String> {
    let deadline = tokio::time::Instant::now() + spec.timeout;
    spec.validate_home()?;
    if spec.executable == config.marsh {
        config.validate_host_executables()?;
    }
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.arguments)
        .current_dir(&config.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    apply_host_environment(command.as_std_mut(), config, &spec.home);
    // On macOS a descriptor (another command's pipe, a daemon connection)
    // becomes close-on-exec only after creation; never spawn in between.
    let mut child = marsh_runtime::with_host_descriptor_creation_excluded(|| command.spawn())
        .map_err(|error| format!("cannot start {}: {error}", spec.executable.display()))?;
    let pid = child
        .id()
        .ok_or_else(|| "spawned command has no process ID".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "spawned command has no stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "spawned command has no stderr pipe".to_string())?;
    let mut stdout_reader = tokio::spawn(read_bounded_stream(stdout));
    let mut stderr_reader = tokio::spawn(read_bounded_stream(stderr));
    let status = tokio::select! {
        result = child.wait() => {
            result.map_err(|error| format!("command wait failed: {error}"))?
        }
        result = cancel_rx.changed() => {
            let cleanup = terminate_process_group(pid, &mut child).await;
            stdout_reader.abort();
            stderr_reader.abort();
            if result.is_ok() && *cancel_rx.borrow() {
                return Err(format!(
                    "operation cancellation requested; cleanup_uncertain=true; {}",
                    cleanup.err().unwrap_or_else(|| "process group exited".into())
                ));
            }
            return Err("operation cancellation channel closed".into());
        },
        () = tokio::time::sleep_until(deadline) => {
            terminate_process_group(pid, &mut child).await?;
            stdout_reader.abort();
            stderr_reader.abort();
            return Err(format!("command exceeded {} ms", spec.timeout.as_millis()));
        }
    };
    let (stdout, stdout_truncated) =
        if let Ok(result) = tokio::time::timeout_at(deadline, &mut stdout_reader).await {
            result.map_err(|error| format!("stdout reader failed: {error}"))??
        } else {
            terminate_process_group(pid, &mut child).await?;
            stdout_reader.abort();
            stderr_reader.abort();
            return Err(format!(
                "command exceeded {} ms while draining descendant output",
                spec.timeout.as_millis()
            ));
        };
    let (stderr, stderr_truncated) =
        if let Ok(result) = tokio::time::timeout_at(deadline, &mut stderr_reader).await {
            result.map_err(|error| format!("stderr reader failed: {error}"))??
        } else {
            terminate_process_group(pid, &mut child).await?;
            stderr_reader.abort();
            return Err(format!(
                "command exceeded {} ms while draining descendant output",
                spec.timeout.as_millis()
            ));
        };
    Ok(CapturedOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

pub(crate) async fn read_bounded_stream(
    mut reader: impl AsyncRead + Unpin,
) -> Result<(Vec<u8>, bool), String> {
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("cannot read command output: {error}"))?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
        let retained = count.min(remaining);
        captured.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    Ok((captured, truncated))
}

pub(crate) async fn terminate_process_group(
    pid: u32,
    child: &mut tokio::process::Child,
) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| "child process ID overflow".to_string())?;
    signal_process_group(pid, nix::sys::signal::Signal::SIGINT)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(8)).await? {
        return Ok(());
    }
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    Err("command process group cleanup remains uncertain after SIGKILL".into())
}

/// Published pipelines are complete scripts. An interrupt can let Brush run
/// the next `;` command, so cancel them with termination instead.
pub(crate) async fn terminate_pipeline_process_group(
    pid: u32,
    child: &mut tokio::process::Child,
) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| "child process ID overflow".to_string())?;
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    Err("pipeline process group cleanup remains uncertain after SIGKILL".into())
}

pub(crate) async fn wait_for_process_group_exit(
    pid: i32,
    child: &mut tokio::process::Child,
    grace: Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        child
            .try_wait()
            .map_err(|error| format!("cannot reap command leader: {error}"))?;
        if !process_group_exists(pid)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(crate) fn process_group_exists(pid: i32) -> Result<bool, String> {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(format!("cannot inspect command process group: {error}")),
    }
}

pub(crate) fn signal_process_group(
    pid: i32,
    signal: nix::sys::signal::Signal,
) -> Result<(), String> {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(format!("cannot signal command process group: {error}")),
    }
}
