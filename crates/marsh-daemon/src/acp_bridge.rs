//! Byte-faithful ACP stdio over the existing Kit job attachment.

use crate::{AttachmentFrame, ClientExecution, DaemonError};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    thread,
};

const CHUNK: usize = 16 * 1024;
const MAX_DIAGNOSTIC: usize = 64 * 1024;

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
        let (io, bridge_io) = UnixStream::pair()?;
        let mut input = bridge_io.try_clone()?;
        let input_execution = execution.clone();
        thread::spawn(move || {
            let mut bytes = [0_u8; CHUNK];
            loop {
                match input.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if input_execution
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
            let _ = input_execution.send(&AttachmentFrame::Signal {
                signal: "terminate".into(),
            });
        });

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
            let _ = output.shutdown(Shutdown::Both);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerAttachment;
    use std::time::Duration;

    #[test]
    fn bridge_preserves_acp_bytes_and_separate_diagnostics() {
        let (server, client) = ServerAttachment::pair().unwrap();
        let mut bridge = AcpAttachmentBridge::new(client).unwrap();
        let mut io = bridge.take_stream().unwrap();
        io.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
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
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(bridge.status().job_id.as_deref(), Some("job-1"));
        assert_eq!(bridge.status().stderr, b"diagnostic");
        assert_eq!(bridge.status().terminal, Some(Ok(0)));
    }
}
