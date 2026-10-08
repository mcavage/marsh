//! Bounded, joined runtime-control owner. The drain loop never performs Docker
//! control IO. A stop preempts a wedged non-terminating control via cancellation.
use crate::drain::PumpEvent;
use crate::{JobSignal, TerminalSize};
use marsh_contracts::ContainerId;
use marsh_runtime::{Cancellation, JobRuntime};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::Duration,
};

const MAX_PENDING: usize = 16;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Signal(JobSignal),
    Resize(TerminalSize),
}
impl Operation {
    fn stopping(self) -> bool {
        matches!(
            self,
            Self::Signal(JobSignal::Terminate | JobSignal::Kill | JobSignal::Hangup)
        )
    }
}
#[derive(Default)]
struct State {
    queued: VecDeque<Operation>,
    stop: Option<Operation>,
    active: Option<(Operation, Cancellation)>,
    closed: bool,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}
pub(crate) struct ControlOwner {
    shared: Arc<Shared>,
    handle: Option<thread::JoinHandle<()>>,
}
impl ControlOwner {
    pub(crate) fn start(
        runtime: Arc<dyn JobRuntime>,
        container: ContainerId,
        events: mpsc::Sender<PumpEvent>,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        });
        let worker = shared.clone();
        let handle = thread::spawn(move || run(&worker, runtime.as_ref(), &container, &events));
        Self {
            shared,
            handle: Some(handle),
        }
    }
    /// Nonblocking admission. Queue rejection is a reported control error, not
    /// an instruction to kill the workload.
    pub(crate) fn enqueue(&self, operation: Operation) -> bool {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return false;
        }
        if operation.stopping() {
            let kill = Operation::Signal(JobSignal::Kill);
            if state.stop == Some(kill) || state.active.as_ref().is_some_and(|(op, _)| *op == kill)
            {
                return true;
            }
            if let Some((_, cancel)) = &state.active {
                cancel.cancel();
            }
            state.queued.clear();
            state.stop = Some(operation);
        } else if matches!(operation, Operation::Resize(_)) {
            if let Some(pending) = state
                .queued
                .iter_mut()
                .find(|op| matches!(op, Operation::Resize(_)))
            {
                *pending = operation;
            } else if state.queued.len() < MAX_PENDING {
                state.queued.push_back(operation);
            } else {
                return false;
            }
        } else if state.queued.len() < MAX_PENDING {
            state.queued.push_back(operation);
        } else {
            return false;
        }
        self.shared.ready.notify_one();
        true
    }
    pub(crate) fn finish(&mut self) -> bool {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.queued.clear();
            state.stop = None;
            if let Some((_, cancel)) = &state.active {
                cancel.cancel();
            }
            self.shared.ready.notify_all();
        }
        self.handle
            .take()
            .is_none_or(|handle| handle.join().is_ok())
    }
}
impl Drop for ControlOwner {
    fn drop(&mut self) {
        self.finish();
    }
}
fn run(
    shared: &Shared,
    runtime: &dyn JobRuntime,
    container: &ContainerId,
    events: &mpsc::Sender<PumpEvent>,
) {
    loop {
        let (operation, cancel) = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if state.closed {
                    return;
                }
                if let Some(operation) = state.stop.take().or_else(|| state.queued.pop_front()) {
                    let cancel = Cancellation::default().with_timeout(Duration::from_secs(2));
                    state.active = Some((operation, cancel.clone()));
                    break (operation, cancel);
                }
                state = shared
                    .ready
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let result = match operation {
            Operation::Signal(signal) => runtime.signal_cancellable(container, signal, &cancel),
            Operation::Resize(size) => runtime.resize_cancellable(container, size, &cancel),
        };
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = None;
        // Cancellation of an obsolete operation by stop/shutdown is deliberate.
        // Other errors remain observable, but never imply job termination.
        if !state.closed && state.stop.is_none() {
            let _ = events.send(PumpEvent::Control(result.is_ok()));
        }
    }
}
