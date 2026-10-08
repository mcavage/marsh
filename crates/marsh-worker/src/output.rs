//! Output adapters with an explicit wake contract, separate from writer locks.
use marsh_runtime::{CancellableFile, Cancellation};
use std::{
    fs::File,
    io::{self, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

pub struct InterruptibleWriter {
    writer: Writer,
    cancel: Cancellation,
}

enum Writer {
    Fd(CancellableFile),
    Adapter(Box<dyn Write + Send>),
}

impl InterruptibleWriter {
    /// The adapter MUST wake every write/flush after `cancel.cancel()`, including
    /// lock acquisition. This is an explicit adapter contract, not a guarantee
    /// provided by `Write`. Production fd users should use `from_file`.
    #[must_use]
    pub fn new(writer: Box<dyn Write + Send>, cancel: Cancellation) -> Self {
        Self {
            writer: Writer::Adapter(writer),
            cancel,
        }
    }
    /// Takes ownership of a pipe/socket description safe to make nonblocking.
    /// # Errors
    /// Rejects other fd kinds, or failure to set nonblocking mode.
    pub fn from_file(file: File, cancel: Cancellation) -> io::Result<Self> {
        Ok(Self {
            writer: Writer::Fd(CancellableFile::from_file(file, &cancel)?),
            cancel,
        })
    }
    #[must_use]
    pub fn sink() -> Self {
        Self::new(Box::new(io::sink()), Cancellation::default())
    }
    #[must_use]
    pub fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }

    pub(crate) fn scoped_write(
        &mut self,
        bytes: &[u8],
        cancel: &Cancellation,
    ) -> io::Result<usize> {
        self.cancel.check()?;
        cancel.check()?;
        match &mut self.writer {
            Writer::Fd(writer) => writer.write_cancellable(bytes, cancel),
            Writer::Adapter(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "retained transport requires scoped fd cancellation",
            )),
        }
    }
}
/// A joined deadline owner, also covering pre-supervision event publication and
/// post-supervision terminal/credit writes on a retained transport.
pub(crate) struct OutputDeadline {
    stop: mpsc::Sender<()>,
    task: Option<thread::JoinHandle<()>>,
}
impl OutputDeadline {
    pub(crate) fn new(timeout: Duration, cancel: Cancellation) -> Self {
        let (stop, receive) = mpsc::channel();
        let task = thread::spawn(move || {
            if receive.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
                cancel.cancel();
            }
        });
        Self {
            stop,
            task: Some(task),
        }
    }
}
impl Drop for OutputDeadline {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

impl Write for InterruptibleWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.cancel.check()?;
        match &mut self.writer {
            Writer::Fd(writer) => writer.write(bytes),
            Writer::Adapter(writer) => writer.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.cancel.check()?;
        match &mut self.writer {
            Writer::Fd(writer) => writer.flush(),
            Writer::Adapter(writer) => writer.flush(),
        }
    }
}
