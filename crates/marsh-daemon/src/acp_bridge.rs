//! Byte-faithful ACP stdio over the existing Kit job attachment.

use crate::{AttachmentFrame, ClientExecution, DaemonError};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

const CHUNK: usize = 16 * 1024;
const MAX_DIAGNOSTIC: usize = 64 * 1024;
// macOS can leave a thread blocked in `read` on a Unix socket after a
// `shutdown` (a lost wakeup; data and a full close always wake it). The input
// pump wakes this often to notice that the output side has finished, so the
// bridge's end is fully closed instead of held open by a stranded reader.
const READ_WAKE: Duration = Duration::from_millis(100);

/// Last observed state of the underlying Kit process. The job receipt remains
/// authoritative for cleanup; this snapshot only reports attachment progress.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpAttachmentStatus {
    pub job_id: Option<String>,
    pub stderr: Vec<u8>,
    pub terminal: Option<Result<i32, String>>,
}

/// Connects ACP's raw line stream to a separately supervised Kit job.
pub struct AcpAttachmentBridge {
    io: Option<UnixStream>,
    execution: ClientExecution,
    status: Arc<Mutex<AcpAttachmentStatus>>,
}

impl AcpAttachmentBridge {
    /// Starts byte pumps. The caller owns the returned stream and must close it
    /// on failed negotiation; closure sends a terminate signal to the Kit job.
    pub fn new(execution: ClientExecution) -> Result<Self, DaemonError> {
        Self::start(execution, true)
    }

    /// `wake`: shut the bridge's end down after a terminal frame. Only tests
    /// pass `false`, to model a kernel that never wakes the input pump.
    fn start(execution: ClientExecution, wake: bool) -> Result<Self, DaemonError> {
        let (io, bridge_io) = marsh_runtime::unix_stream_pair()?;
        // EINVAL (macOS): the peer has already closed, so reads cannot block.
        if let Err(error) = bridge_io.set_read_timeout(Some(READ_WAKE))
            && error.raw_os_error() != Some(rustix::io::Errno::INVAL.raw_os_error())
        {
            return Err(error.into());
        }
        let input = bridge_io.try_clone()?;
        let input_execution = execution.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let output_finished = Arc::clone(&finished);
        thread::spawn(move || pump_input(input, &input_execution, &finished));

        let status = Arc::new(Mutex::new(AcpAttachmentStatus::default()));
        let output_status = Arc::clone(&status);
        let output_execution = execution.clone();
        thread::spawn(move || {
            let mut output = bridge_io;
            loop {
                match output_execution.receive() {
                    Ok(AttachmentFrame::Stdout { bytes }) => {
                        if output.write_all(&bytes).is_err() {
                            let _ = output_execution.send(&AttachmentFrame::Signal {
                                signal: "terminate".into(),
                            });
                            // Keep draining until the backend publishes its terminal receipt.
                        }
                    }
                    Ok(AttachmentFrame::Stderr { bytes }) => {
                        let mut state = output_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let room = MAX_DIAGNOSTIC.saturating_sub(state.stderr.len());
                        state
                            .stderr
                            .extend_from_slice(&bytes[..bytes.len().min(room)]);
                    }
                    Ok(AttachmentFrame::JobStarted { job_id }) => {
                        output_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .job_id = Some(job_id);
                    }
                    Ok(AttachmentFrame::Exited { code }) => {
                        output_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .terminal = Some(Ok(code));
                        break;
                    }
                    Ok(AttachmentFrame::Failed { message }) => {
                        output_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .terminal = Some(Err(message));
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        output_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .terminal = Some(Err(error.to_string()));
                        break;
                    }
                }
            }
            // Wake both ACP halves and the input pump after a terminal frame.
            output_finished.store(true, Ordering::Release);
            if wake {
                let _ = output.shutdown(Shutdown::Both);
            }
        });
        Ok(Self {
            io: Some(io),
            execution,
            status,
        })
    }

    /// Returns the ACP byte stream exactly once for async conversion.
    #[must_use]
    pub fn take_stream(&mut self) -> Option<UnixStream> {
        self.io.take()
    }

    #[must_use]
    pub fn status(&self) -> AcpAttachmentStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Requests Kit termination. Completion remains uncertain until the job
    /// receipt confirms container cleanup.
    pub fn terminate(&self) -> Result<(), DaemonError> {
        self.execution.send(&AttachmentFrame::Signal {
            signal: "terminate".into(),
        })
    }

    pub fn kill(&self) -> Result<(), DaemonError> {
        self.execution.send(&AttachmentFrame::Signal {
            signal: "kill".into(),
        })
    }
}

/// Forwards ACP bytes as Kit stdin until either side ends (or the output side
/// has finished: see [`READ_WAKE`]), then asks the Kit job to terminate.
fn pump_input(mut input: UnixStream, execution: &ClientExecution, finished: &AtomicBool) {
    let mut bytes = [0_u8; CHUNK];
    loop {
        match input.read(&mut bytes) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                if finished.load(Ordering::Acquire) {
                    break;
                }
            }
            Ok(0) | Err(_) => break,
            Ok(count) => {
                if execution
                    .send(&AttachmentFrame::Stdin {
                        bytes: bytes[..count].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    let _ = execution.send(&AttachmentFrame::Signal {
        signal: "terminate".into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerAttachment;

    #[test]
    fn bridge_preserves_acp_bytes_and_separate_diagnostics() {
        let (server, client) = ServerAttachment::pair().unwrap();
        let mut bridge = AcpAttachmentBridge::new(client).unwrap();
        let mut io = bridge.take_stream().unwrap();
        io.set_read_timeout(Some(crate::test_support::PATIENCE))
            .unwrap();
        io.write_all(b"{\"method\":\"initialize\"}\n").unwrap();
        assert!(matches!(
            server.receive().unwrap(),
            AttachmentFrame::Stdin { bytes } if bytes == b"{\"method\":\"initialize\"}\n"
        ));
        server
            .send(&AttachmentFrame::JobStarted {
                job_id: "job-1".into(),
            })
            .unwrap();
        server
            .send(&AttachmentFrame::Stderr {
                bytes: b"diagnostic".to_vec(),
            })
            .unwrap();
        server
            .send(&AttachmentFrame::Stdout {
                bytes: b"{\"result\":{}}\n".to_vec(),
            })
            .unwrap();
        let mut response = [0_u8; 14];
        io.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"{\"result\":{}}\n");
        server.send(&AttachmentFrame::Exited { code: 0 }).unwrap();
        // The bridge closes the ACP stream only after recording the terminal.
        let mut rest = Vec::new();
        io.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert_eq!(bridge.status().job_id.as_deref(), Some("job-1"));
        assert_eq!(bridge.status().stderr, b"diagnostic");
        assert_eq!(bridge.status().terminal, Some(Ok(0)));
    }

    /// macOS can leave the input pump blocked in `read` after the bridge's
    /// shutdown. The bridge must still fully release its end after the Kit's
    /// terminal, so the ACP side reaches EOF and the Kit is told to stop.
    #[test]
    fn a_terminal_releases_the_acp_stream_even_if_shutdown_wakes_no_reader() {
        let (server, client) = ServerAttachment::pair().unwrap();
        let mut bridge = AcpAttachmentBridge::start(client, false).unwrap();
        let mut io = bridge.take_stream().unwrap();
        server.send(&AttachmentFrame::Exited { code: 0 }).unwrap();
        assert_eq!(crate::test_support::read_to_end(&mut io).unwrap(), b"");
        assert!(matches!(
            crate::test_support::returns("the input pump's terminate", move || server.receive()),
            Ok(AttachmentFrame::Signal { signal }) if signal == "terminate"
        ));
        assert_eq!(bridge.status().terminal, Some(Ok(0)));
    }
}
