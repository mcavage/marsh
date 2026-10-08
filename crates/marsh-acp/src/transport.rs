//! Stdio line-delimited JSON-RPC transport with frame bounds and subprocess supervision.

#[cfg(feature = "test-support")]
use crate::client::{AcpClient, AcpClientConfig, PermissionHandler};
use crate::error::AcpError;
use crate::protocol::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
#[cfg(feature = "test-support")]
use std::process::Stdio;
#[cfg(feature = "test-support")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "test-support")]
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(feature = "test-support")]
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
#[cfg(feature = "test-support")]
use tokio::sync::Mutex as TokioMutex;

/// Default maximum frame size for incoming ACP JSON-RPC messages (1 MiB).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Maximum bytes kept in memory for subprocess stderr diagnostic capture (64 KiB).
#[cfg(feature = "test-support")]
pub const MAX_STDERR_CAPTURE_BYTES: usize = 64 * 1024;

/// Reads a single newline-delimited line from an async reader, enforcing a strict byte ceiling.
///
/// # Errors
/// Returns [`AcpError::FrameTooLarge`] if line length exceeds `max_frame_bytes` before newline.
/// Returns [`AcpError::Io`] on stream IO error.
/// Returns [`AcpError::Protocol`] on invalid UTF-8.
pub async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> Result<Option<String>, AcpError> {
    let mut line_bytes = Vec::new();

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line_bytes.is_empty() {
                return Ok(None);
            }
            break;
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if line_bytes.len() + pos > max_frame_bytes {
                let size = line_bytes.len() + pos;
                reader.consume(pos + 1);
                return Err(AcpError::FrameTooLarge {
                    size,
                    max: max_frame_bytes,
                });
            }
            line_bytes.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            break;
        }

        if line_bytes.len() + available.len() > max_frame_bytes {
            let size = line_bytes.len() + available.len();
            return Err(AcpError::FrameTooLarge {
                size,
                max: max_frame_bytes,
            });
        }

        line_bytes.extend_from_slice(available);
        let len = available.len();
        reader.consume(len);
    }

    if line_bytes.ends_with(b"\r") {
        line_bytes.pop();
    }

    let line_str = String::from_utf8(line_bytes)
        .map_err(|e| AcpError::Protocol(format!("invalid UTF-8 in frame: {e}")))?;
    Ok(Some(line_str))
}

/// Reads JSON-RPC messages from an async stream with strict byte bounds.
pub struct MessageReader<R> {
    reader: BufReader<R>,
    max_frame_bytes: usize,
}

impl<R: AsyncRead + Unpin> MessageReader<R> {
    #[must_use]
    pub fn new(reader: R, max_frame_bytes: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            max_frame_bytes,
        }
    }

    /// Receives the next JSON-RPC message from the input stream, skipping blank lines.
    ///
    /// # Errors
    /// Returns [`AcpError::FrameTooLarge`] if line size exceeds ceiling.
    /// Returns [`AcpError::Serialization`] if JSON parsing fails.
    /// Returns [`AcpError::Io`] on stream IO error.
    pub async fn recv_message(&mut self) -> Result<Option<JsonRpcMessage>, AcpError> {
        loop {
            let line = read_bounded_line(&mut self.reader, self.max_frame_bytes).await?;
            let Some(line) = line else {
                return Ok(None);
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: JsonRpcMessage = serde_json::from_str(trimmed)?;
            return Ok(Some(msg));
        }
    }
}

/// Writes JSON-RPC messages to an async stream with newline termination.
pub struct MessageWriter<W> {
    writer: W,
}

impl<W: AsyncWrite + Unpin> MessageWriter<W> {
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Sends a JSON-RPC message terminated with a newline and flushes the writer.
    ///
    /// # Errors
    /// Returns [`AcpError::Serialization`] or [`AcpError::Io`] on write failure.
    pub async fn send_message(&mut self, message: &JsonRpcMessage) -> Result<(), AcpError> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

/// Bidirectional JSON-RPC framing transport over arbitrary async streams.
pub struct StdioTransport<R, W> {
    reader: MessageReader<R>,
    writer: MessageWriter<W>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> StdioTransport<R, W> {
    #[must_use]
    pub fn new(reader: R, writer: W, max_frame_bytes: usize) -> Self {
        Self {
            reader: MessageReader::new(reader, max_frame_bytes),
            writer: MessageWriter::new(writer),
        }
    }

    /// Splits transport into independent reader and writer components.
    #[must_use]
    pub fn into_split(self) -> (MessageReader<R>, MessageWriter<W>) {
        (self.reader, self.writer)
    }

    /// Receives the next JSON-RPC message from the input stream.
    ///
    /// # Errors
    /// Returns [`AcpError::FrameTooLarge`] if line size exceeds ceiling.
    /// Returns [`AcpError::Serialization`] if JSON parsing fails.
    /// Returns [`AcpError::Io`] on stream IO error.
    pub async fn recv_message(&mut self) -> Result<Option<JsonRpcMessage>, AcpError> {
        self.reader.recv_message().await
    }

    /// Sends a JSON-RPC message terminated with a newline and flushes the writer.
    ///
    /// # Errors
    /// Returns [`AcpError::Serialization`] or [`AcpError::Io`] on write failure.
    pub async fn send_message(&mut self, message: &JsonRpcMessage) -> Result<(), AcpError> {
        self.writer.send_message(message).await
    }

    /// Sends a request.
    ///
    /// # Errors
    /// Returns [`AcpError`] on serialization or transport failure.
    pub async fn send_request(&mut self, req: JsonRpcRequest) -> Result<(), AcpError> {
        self.send_message(&JsonRpcMessage::Request(req)).await
    }

    /// Sends a response.
    ///
    /// # Errors
    /// Returns [`AcpError`] on serialization or transport failure.
    pub async fn send_response(&mut self, resp: JsonRpcResponse) -> Result<(), AcpError> {
        self.send_message(&JsonRpcMessage::Response(resp)).await
    }

    /// Sends a notification.
    ///
    /// # Errors
    /// Returns [`AcpError`] on serialization or transport failure.
    pub async fn send_notification(&mut self, notif: JsonRpcNotification) -> Result<(), AcpError> {
        self.send_message(&JsonRpcMessage::Notification(notif))
            .await
    }
}

/// Supervisor for a real child process speaking ACP over stdio pipes.
///
/// # Security Policy
/// Host subprocess execution is strictly restricted to test harnesses.
/// Production ACP workloads MUST supply already-governed IO (e.g. from sandboxed
/// native Kit workers/VMs).
/// Any spawned subprocess explicitly executes with `env_clear()` to ensure
/// zero host environment or secret inheritance.
#[cfg(feature = "test-support")]
pub struct SubprocessHandle {
    child: TokioMutex<Option<Child>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
    closed: Arc<AtomicBool>,
}

#[cfg(feature = "test-support")]
impl SubprocessHandle {
    /// Spawns the child process and returns piped stdout, stdin, and the supervisor handle.
    ///
    /// # Errors
    /// Returns [`AcpError::Io`] if process spawn fails.
    pub fn spawn(mut command: Command) -> Result<(ChildStdout, ChildStdin, Self), AcpError> {
        // SECURITY ENFORCEMENT: Explicit zero host env inheritance.
        command.env_clear();

        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Io(std::io::Error::other("failed to capture child stdout")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Io(std::io::Error::other("failed to capture child stdin")))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| AcpError::Io(std::io::Error::other("failed to capture child stderr")))?;

        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buffer);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_clone = Arc::clone(&closed);

        // Supervise stderr in background without blocking stdout
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = tokio::io::AsyncReadExt::read(&mut stderr, &mut buf).await {
                if n == 0 {
                    break;
                }
                if let Ok(mut guard) = stderr_buf_clone.lock() {
                    let remaining = MAX_STDERR_CAPTURE_BYTES.saturating_sub(guard.len());
                    let to_copy = n.min(remaining);
                    guard.extend_from_slice(&buf[..to_copy]);
                }
            }
            closed_clone.store(true, Ordering::Release);
        });

        let handle = Self {
            child: TokioMutex::new(Some(child)),
            stderr_buffer,
            closed,
        };

        Ok((stdout, stdin, handle))
    }

    /// Spawns the child process and binds a newly connected [`AcpClient`].
    ///
    /// # Errors
    /// Returns [`AcpError::Io`] if process spawn fails.
    pub fn spawn_client(
        command: Command,
        config: AcpClientConfig,
        permission_handler: Option<Arc<dyn PermissionHandler>>,
    ) -> Result<(AcpClient, Self), AcpError> {
        let (stdout, stdin, handle) = Self::spawn(command)?;
        let client = AcpClient::new(stdout, stdin, config, permission_handler);
        Ok((client, handle))
    }

    /// Returns true if stderr capture stream has closed.
    #[must_use]
    pub fn is_stderr_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Reads accumulated stderr bytes.
    #[must_use]
    pub fn stderr_output(&self) -> Vec<u8> {
        self.stderr_buffer
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.clone())
    }

    /// Returns loss diagnostic if child has exited.
    pub async fn check_exit(&self) -> Option<String> {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            let stderr = String::from_utf8_lossy(&self.stderr_output()).to_string();
            return Some(format!(
                "subprocess exited with status {status}; captured stderr: {stderr}"
            ));
        }
        None
    }

    /// Terminate subprocess.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if child kill fails.
    pub async fn kill(&self) -> Result<(), std::io::Error> {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            child.kill().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_read_bounded_line_normal_and_crlf() {
        let data = b"line 1\nline 2\r\nline 3\n";
        let mut cursor = Cursor::new(data);

        let l1 = read_bounded_line(&mut cursor, 100).await.unwrap();
        assert_eq!(l1.as_deref(), Some("line 1"));

        let l2 = read_bounded_line(&mut cursor, 100).await.unwrap();
        assert_eq!(l2.as_deref(), Some("line 2"));

        let l3 = read_bounded_line(&mut cursor, 100).await.unwrap();
        assert_eq!(l3.as_deref(), Some("line 3"));

        let l4 = read_bounded_line(&mut cursor, 100).await.unwrap();
        assert_eq!(l4, None);
    }

    #[tokio::test]
    async fn test_recv_message_skips_blank_lines() {
        let data = b"\n\r\n   \n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n  \r\n{\"jsonrpc\":\"2.0\",\"method\":\"notif\"}\n\n";
        let mut reader = MessageReader::new(Cursor::new(data), 1024);

        let msg1 = reader.recv_message().await.unwrap();
        assert!(matches!(msg1, Some(JsonRpcMessage::Response(_))));

        let msg2 = reader.recv_message().await.unwrap();
        assert!(matches!(msg2, Some(JsonRpcMessage::Notification(_))));

        let msg3 = reader.recv_message().await.unwrap();
        assert!(msg3.is_none());
    }

    #[tokio::test]
    async fn test_read_bounded_line_oversized() {
        let data = b"this is a very long line that exceeds the limit\n";
        let mut cursor = Cursor::new(data);

        let err = read_bounded_line(&mut cursor, 10).await.unwrap_err();
        match err {
            AcpError::FrameTooLarge { size, max } => {
                assert!(size > 10);
                assert_eq!(max, 10);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }
}
