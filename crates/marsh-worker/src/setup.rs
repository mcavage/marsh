//! Cancellable setup while preserving bounded pre-start stdin/control ordering.
use crate::{
    ControlSource, DeliveryOutcome, MAX_STREAM_CHUNK, WORKER_CONTROL_SPOOL_MESSAGES,
    WORKER_INPUT_SPOOL_BYTES, WorkerControl, WorkerError,
};
use marsh_contracts::{ContainerId, JobSignal, JobSpec};
use marsh_runtime::{Cancellation, JobRuntime, RuntimeError};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

pub(crate) struct PreparedControls<'a> {
    source: &'a mut dyn ControlSource,
    pending: VecDeque<WorkerControl>,
    bytes: usize,
    cancel: Cancellation,
    pub(crate) stopped: Option<DeliveryOutcome>,
}
impl<'a> PreparedControls<'a> {
    pub(crate) fn new(source: &'a mut dyn ControlSource) -> Self {
        let cancel = source.cancellation().unwrap_or_default();
        Self {
            source,
            pending: VecDeque::new(),
            bytes: 0,
            cancel,
            stopped: None,
        }
    }
    pub(crate) fn create(
        &mut self,
        runtime: Arc<dyn JobRuntime>,
        spec: &JobSpec,
    ) -> Result<ContainerId, RuntimeError> {
        let spec = spec.clone();
        let cancel = self.cancel.clone();
        let (send, receive) = mpsc::sync_channel(1);
        let creator = thread::spawn(move || {
            let _ = send.send(runtime.create_cancellable(&spec, &cancel));
        });
        let result = loop {
            self.poll_setup();
            match receive.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(io::Error::other("runtime create owner panicked").into());
                }
            }
        };
        // The runtime contract is bounded and cancellable, including cleanup.
        let joined = creator.join().is_ok();
        self.poll_setup();
        if !joined {
            return Err(io::Error::other("runtime create owner panicked").into());
        }
        result
    }
    fn poll_setup(&mut self) {
        if self.stopped.is_some() {
            return;
        }
        match self.source.receive(Duration::ZERO) {
            Ok(Some(
                control @ WorkerControl::Signal {
                    signal: JobSignal::Kill | JobSignal::Terminate | JobSignal::Hangup,
                },
            )) => {
                self.stopped = Some(DeliveryOutcome::Cancelled);
                self.cancel.cancel();
                self.pending.clear();
                self.bytes = 0;
                self.source.release_all_input();
                self.pending.push_back(control);
            }
            Ok(Some(control)) => {
                let bytes = match &control {
                    WorkerControl::Input { bytes } => bytes.len(),
                    _ => 0,
                };
                if bytes > MAX_STREAM_CHUNK
                    || self.pending.len() >= WORKER_CONTROL_SPOOL_MESSAGES
                    || self.bytes.saturating_add(bytes) > WORKER_INPUT_SPOOL_BYTES
                {
                    self.stopped = Some(DeliveryOutcome::Failed);
                    self.cancel.cancel();
                    self.release_all_input();
                } else {
                    self.bytes += bytes;
                    self.pending.push_back(control);
                }
            }
            Ok(None) => {}
            Err(_) => {
                self.stopped = Some(DeliveryOutcome::Failed);
                self.cancel.cancel();
                self.release_all_input();
            }
        }
    }
}
impl ControlSource for PreparedControls<'_> {
    fn cancellation(&self) -> Option<Cancellation> {
        Some(self.cancel.clone())
    }
    fn receive(&mut self, timeout: Duration) -> Result<Option<WorkerControl>, WorkerError> {
        if self.pending.is_empty() {
            return self.source.receive(timeout);
        }
        // Incoming signals/resize remain live even with prefetched stdin.
        if let Some(control) = self.source.receive(Duration::ZERO)? {
            if matches!(
                control,
                WorkerControl::Signal { .. } | WorkerControl::Resize { .. }
            ) {
                return Ok(Some(control));
            }
            let count = match &control {
                WorkerControl::Input { bytes } => bytes.len(),
                _ => 0,
            };
            if self.pending.len() >= WORKER_CONTROL_SPOOL_MESSAGES
                || self.bytes.saturating_add(count) > WORKER_INPUT_SPOOL_BYTES
            {
                return Err(WorkerError::InputSpoolFull);
            }
            self.bytes += count;
            self.pending.push_back(control);
        }
        let control = self.pending.pop_front();
        if let Some(WorkerControl::Input { bytes }) = &control {
            self.bytes = self.bytes.saturating_sub(bytes.len());
        }
        Ok(control)
    }
    fn release_input(&mut self, bytes: usize) {
        self.source.release_input(bytes);
    }
    fn release_all_input(&mut self) {
        self.pending.retain(|control| {
            !matches!(
                control,
                WorkerControl::Input { .. } | WorkerControl::CloseInput
            )
        });
        self.bytes = 0;
        self.source.release_all_input();
    }
}
