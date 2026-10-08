use super::{
    CleanupOutcome, ControlSource, DeliveryOutcome, ExecutionOutcome, InterruptibleWriter,
    JobStreams, MAX_STREAM_CHUNK, ResourceLimit, SupervisionLimits, Supervisor, ThreadSupervisor,
    WorkerControl, WorkerError, classify_runtime_exit, reserve_output,
};
use crate::control::{ControlOwner, Operation};
use marsh_contracts::{ContainerId, JobSignal};
use marsh_runtime::{
    Attachment, Cancellation, JobRuntime, RuntimeError, RuntimeExit, finish_owned_process,
};
use std::{
    io::{self, Read, Write},
    sync::{Arc, atomic::AtomicU64, mpsc},
    thread,
    time::{Duration, Instant},
};
const TICK: Duration = Duration::from_millis(10);

/// Independent execution, delivery, cleanup and nonfatal control observations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupervisionReport {
    pub execution: ExecutionOutcome,
    pub delivery: DeliveryOutcome,
    pub cleanup: CleanupOutcome,
    pub control_errors: u32,
}
impl SupervisionReport {
    #[must_use]
    pub fn complete(execution: ExecutionOutcome) -> Self {
        Self {
            execution,
            delivery: DeliveryOutcome::Complete,
            cleanup: CleanupOutcome::Verified,
            control_errors: 0,
        }
    }
}
impl Supervisor for ThreadSupervisor {
    #[allow(clippy::too_many_lines)] // Explicit spawn, cancellation, join and reap ordering.
    fn supervise(
        &self,
        runtime: Arc<dyn JobRuntime>,
        container: &ContainerId,
        mut attachment: Attachment,
        controls: &mut dyn ControlSource,
        streams: JobStreams,
        limits: SupervisionLimits,
    ) -> SupervisionReport {
        let deadline = Instant::now().checked_add(limits.wall_time);
        let (events, receive) = mpsc::channel();
        let mut control_owner =
            ControlOwner::start(runtime.clone(), container.clone(), events.clone());
        if !attachment.process.supports_io_cancellation() {
            control_owner.enqueue(Operation::Signal(JobSignal::Kill));
            let clean = finish_owned_process(attachment.process, self.termination_grace);
            control_owner.finish();
            return SupervisionReport {
                execution: ExecutionOutcome::SupervisionFailed,
                delivery: DeliveryOutcome::Failed,
                cleanup: if clean {
                    CleanupOutcome::Verified
                } else {
                    CleanupOutcome::Uncertain
                },
                control_errors: 0,
            };
        }
        let output_cancel = [streams.stdout.cancellation(), streams.stderr.cancellation()];
        let used = Arc::new(AtomicU64::new(0));
        let stdout = spawn_copy(
            attachment.stdout,
            streams.stdout,
            used.clone(),
            limits.output_bytes,
            events.clone(),
        );
        let stderr = spawn_copy(
            attachment.stderr,
            streams.stderr,
            used,
            limits.output_bytes,
            events.clone(),
        );
        let (input_send, input_receive) = mpsc::channel::<Vec<u8>>();
        let input_events = events.clone();
        let input = thread::spawn(move || {
            let mut writer = attachment.stdin;
            let result = (|| {
                while let Ok(bytes) = input_receive.recv() {
                    writer.write_all(&bytes)?;
                    writer.flush()?;
                    let _ = input_events.send(PumpEvent::Consumed(bytes.len()));
                }
                Ok(())
            })();
            drop(input_receive);
            drop(writer);
            let _ = input_events.send(PumpEvent::Input(result));
        });
        let wait_cancel = Cancellation::default();
        let waiting_cancel = wait_cancel.clone();
        let waiting_container = container.clone();
        let waiter = thread::spawn(move || {
            let _ = events.send(PumpEvent::Wait(
                runtime.wait_cancellable(&waiting_container, &waiting_cancel),
            ));
        });
        let mut state = DrainState {
            input: Some(input_send),
            input_done: false,
            streams_done: 0,
            wait_done: false,
            execution: None,
            delivery: DeliveryOutcome::Complete,
            stopping: None,
            runtime_kill_requested: false,
            carrier_terminated: false,
            stop_signal: None,
            control_errors: 0,
            control_closed: false,
        };
        let mut clean = true;
        let mut carrier_status = None;
        loop {
            // Controls are nonblocking here. Pacing belongs to the event channel,
            // so a disconnected source cannot spin for the termination grace.
            if !state.control_closed {
                state.control(controls, &control_owner);
            }
            while let Ok(event) = receive.try_recv() {
                state.event(event, controls, limits.writable_bytes);
            }
            if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                state.fail(DeliveryOutcome::LimitExceeded {
                    resource: ResourceLimit::Wall,
                });
            }
            if state.delivery != DeliveryOutcome::Complete && state.stopping.is_none() {
                state.stopping = Some(Instant::now());
                drop(state.input.take());
                controls.release_all_input();
                clean &= attachment.process.cancel_io().is_ok();
                for cancel in &output_cancel {
                    cancel.cancel();
                }
                if state.execution.is_none() {
                    let signal = state.stop_signal.unwrap_or(JobSignal::Terminate);
                    state.request(&control_owner, Operation::Signal(signal));
                    state.runtime_kill_requested = signal == JobSignal::Kill;
                }
            }
            if let Some(stopping) = state.stopping {
                if stopping.elapsed() >= self.termination_grace {
                    if !state.runtime_kill_requested && state.execution.is_none() {
                        state.request(&control_owner, Operation::Signal(JobSignal::Kill));
                        state.runtime_kill_requested = true;
                    }
                    // Runtime KILL is not a carrier-termination observation.
                    if !state.carrier_terminated {
                        clean &= attachment.process.terminate().is_ok();
                        state.carrier_terminated = true;
                    }
                }
                if stopping.elapsed() >= self.termination_grace.saturating_mul(2) {
                    wait_cancel.cancel();
                }
            }
            let pumps_done = state.wait_done && state.streams_done == 2 && state.input_done;
            if pumps_done {
                if state.stopping.is_some() {
                    break;
                }
                match attachment.process.try_wait() {
                    Ok(Some(code)) => {
                        carrier_status = Some(code);
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => state.fail(DeliveryOutcome::Failed),
                }
            }
            if !pumps_done
                && input.is_finished()
                && waiter.is_finished()
                && stdout.is_finished()
                && stderr.is_finished()
            {
                if let Ok(event) = receive.try_recv() {
                    state.event(event, controls, limits.writable_bytes);
                } else {
                    clean = false;
                    state.fail(DeliveryOutcome::Failed);
                    break;
                }
            }
            if let Ok(event) = receive.recv_timeout(TICK) {
                state.event(event, controls, limits.writable_bytes);
            }
        }
        drop(state.input.take());
        clean &= input.join().is_ok();
        clean &= waiter.join().is_ok();
        clean &= stdout.join().is_ok();
        clean &= stderr.join().is_ok();
        clean &= control_owner.finish();
        while let Ok(event) = receive.try_recv() {
            state.event(event, controls, limits.writable_bytes);
        }
        controls.release_all_input();
        let execution = state
            .execution
            .take()
            .unwrap_or(ExecutionOutcome::SupervisionFailed);
        if let Some(code) = carrier_status
            && let ExecutionOutcome::Exited { code: expected } = execution
            && code != 0
            && code != expected
        {
            state.fail(DeliveryOutcome::Failed);
        }
        if carrier_status.is_none() {
            clean &= finish_owned_process(attachment.process, self.termination_grace);
        }
        SupervisionReport {
            execution,
            delivery: state.delivery,
            cleanup: if clean {
                CleanupOutcome::Verified
            } else {
                CleanupOutcome::Uncertain
            },
            control_errors: state.control_errors,
        }
    }
}
#[allow(clippy::struct_excessive_bools)] // Independent drain progress facts, not a state machine.
struct DrainState {
    input: Option<mpsc::Sender<Vec<u8>>>,
    input_done: bool,
    streams_done: usize,
    wait_done: bool,
    execution: Option<ExecutionOutcome>,
    delivery: DeliveryOutcome,
    stopping: Option<Instant>,
    runtime_kill_requested: bool,
    carrier_terminated: bool,
    stop_signal: Option<JobSignal>,
    control_errors: u32,
    control_closed: bool,
}
impl DrainState {
    fn fail(&mut self, delivery: DeliveryOutcome) {
        if self.delivery == DeliveryOutcome::Complete
            || (self.delivery == DeliveryOutcome::Failed && delivery != DeliveryOutcome::Failed)
        {
            if delivery
                == (DeliveryOutcome::LimitExceeded {
                    resource: ResourceLimit::Output,
                })
            {
                self.stop_signal = Some(JobSignal::Kill);
            }
            self.delivery = delivery;
        }
    }
    fn request(&mut self, owner: &ControlOwner, operation: Operation) {
        if !owner.enqueue(operation) {
            self.control_errors = self.control_errors.saturating_add(1);
        }
    }
    fn control(&mut self, controls: &mut dyn ControlSource, owner: &ControlOwner) {
        match controls.receive(Duration::ZERO) {
            Ok(Some(WorkerControl::Input { bytes })) => {
                let length = bytes.len();
                if self.input.is_none() {
                    controls.release_input(length);
                } else if length > MAX_STREAM_CHUNK {
                    controls.release_input(length);
                    self.fail(DeliveryOutcome::Failed);
                } else if self
                    .input
                    .as_ref()
                    .is_some_and(|input| input.send(bytes).is_err())
                {
                    controls.release_input(length);
                    drop(self.input.take());
                }
            }
            Ok(Some(WorkerControl::CloseInput)) => {
                drop(self.input.take());
            }
            Ok(Some(WorkerControl::Signal { signal })) => {
                if matches!(
                    signal,
                    JobSignal::Terminate | JobSignal::Kill | JobSignal::Hangup
                ) {
                    self.fail(DeliveryOutcome::Cancelled);
                    self.stop_signal = Some(signal);
                    if self.stopping.is_some()
                        && signal == JobSignal::Kill
                        && !self.runtime_kill_requested
                        && self.execution.is_none()
                    {
                        self.request(owner, Operation::Signal(signal));
                        self.runtime_kill_requested = true;
                    }
                } else if self.execution.is_none() {
                    self.request(owner, Operation::Signal(signal));
                }
            }
            Ok(Some(WorkerControl::Resize { size })) => {
                if self.execution.is_none() {
                    self.request(owner, Operation::Resize(size));
                }
            }
            Ok(None) => {}
            Err(WorkerError::ControlClosed) => {
                self.control_closed = true;
                self.fail(DeliveryOutcome::Failed);
            }
            Err(_) => self.fail(DeliveryOutcome::Failed),
        }
    }
    fn event(&mut self, event: PumpEvent, controls: &mut dyn ControlSource, writable_limit: u64) {
        match event {
            PumpEvent::Consumed(bytes) => controls.release_input(bytes),
            PumpEvent::Input(result) => {
                self.input_done = true;
                drop(self.input.take());
                controls.release_all_input();
                if let Err(error) = result
                    && self.stopping.is_none()
                    && !matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::NotConnected
                    )
                {
                    self.fail(DeliveryOutcome::Failed);
                }
            }
            PumpEvent::Output(complete) => {
                self.streams_done += 1;
                if !complete {
                    self.fail(DeliveryOutcome::Failed);
                }
            }
            PumpEvent::Limit => self.fail(DeliveryOutcome::LimitExceeded {
                resource: ResourceLimit::Output,
            }),
            PumpEvent::Wait(result) => {
                self.wait_done = true;
                drop(self.input.take());
                match result {
                    Ok(exit) => self.execution = Some(classify_runtime_exit(exit, writable_limit)),
                    Err(_) => self.fail(DeliveryOutcome::Failed),
                }
            }
            PumpEvent::Control(success) => {
                if !success {
                    self.control_errors = self.control_errors.saturating_add(1);
                }
            }
        }
    }
}
pub(super) enum PumpEvent {
    Consumed(usize),
    Input(io::Result<()>),
    Output(bool),
    Limit,
    Wait(Result<RuntimeExit, RuntimeError>),
    Control(bool),
}
pub(super) fn spawn_copy(
    mut input: Box<dyn Read + Send>,
    mut output: InterruptibleWriter,
    used: Arc<AtomicU64>,
    limit: u64,
    events: mpsc::Sender<PumpEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let mut bytes = [0; 8192];
            loop {
                let count = input.read(&mut bytes)?;
                if count == 0 {
                    return output.flush();
                }
                let allowed = reserve_output(&used, limit, count);
                if allowed < count {
                    let _ = events.send(PumpEvent::Limit);
                }
                output.write_all(&bytes[..allowed])?;
                output.flush()?;
                if allowed < count {
                    return Ok(());
                }
            }
        })();
        drop(output);
        drop(input);
        let _ = events.send(PumpEvent::Output(result.is_ok()));
    })
}
