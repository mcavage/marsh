//! Internal I/O utilities.

use crate::openfiles;

/// Error type for `FailingReaderWriter`.
#[derive(Clone, Debug, thiserror::Error)]
enum ReaderWriterError {
    /// I/O read error.
    #[error("I/O read error: {0}")]
    Read(&'static str),
    /// I/O write error.
    #[error("I/O write error: {0}")]
    Write(&'static str),
    /// I/O flush error.
    #[error("I/O flush error: {0}")]
    Flush(&'static str),
}

/// An implementation of `std::io::Read` and `std::io::Write` that always fails.
///
/// Also implements `openfiles::Stream` for use with `openfiles::OpenFile`. Useful
/// for providing a valid stream that gracefully fails all operations in cases where
/// no real I/O is possible.
#[derive(Clone)]
pub(crate) struct FailingReaderWriter {
    message: &'static str,
}

impl FailingReaderWriter {
    /// Creates a new `FailingReaderWriter` with the given error message.
    ///
    /// # Arguments
    ///
    /// * `message` - The error message to use for all operations.
    pub const fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl openfiles::Stream for FailingReaderWriter {
    fn clone_box(&self) -> Box<dyn openfiles::Stream> {
        Box::new(self.clone())
    }

    #[cfg(unix)] // ast-grep-ignore: platform-cfg-outside-sys
    fn try_clone_to_owned(&self) -> Result<std::os::fd::OwnedFd, crate::error::Error> {
        Err(crate::error::ErrorKind::CannotConvertToNativeFd.into())
    }

    #[cfg(unix)] // ast-grep-ignore: platform-cfg-outside-sys
    fn try_borrow_as_fd(&self) -> Result<std::os::fd::BorrowedFd<'_>, crate::error::Error> {
        Err(crate::error::ErrorKind::CannotConvertToNativeFd.into())
    }
}

impl std::io::Read for FailingReaderWriter {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(ReaderWriterError::Read(self.message)))
    }
}

impl std::io::Write for FailingReaderWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other(ReaderWriterError::Write(
            self.message,
        )))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other(ReaderWriterError::Flush(
            self.message,
        )))
    }
}

impl From<FailingReaderWriter> for openfiles::OpenFile {
    fn from(frw: FailingReaderWriter) -> Self {
        Self::Stream(Box::new(frw))
    }
}

/// Bash's `check_binary_file`: an ELF header, or a NUL in the sample's first
/// line (its first two when it starts with `#!`).
pub(crate) fn is_binary_sample(sample: &[u8]) -> bool {
    if sample.starts_with(b"\x7fELF") {
        return true;
    }
    let mut lines = if sample.starts_with(b"#!") { 2 } else { 1 };
    for byte in sample {
        match byte {
            b'\n' => {
                lines -= 1;
                if lines == 0 {
                    return false;
                }
            }
            0 => return true,
            _ => {}
        }
    }
    false
}
