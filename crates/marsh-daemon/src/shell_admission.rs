//! Generation-owned `OpenShell` admission. Public detach cannot retire an active
//! owner; only that request's completion can settle its retained authority.
use crate::{DaemonError, DaemonStore, ShellState, State};

pub(crate) struct ShellAdmission {
    store: DaemonStore,
    session: String,
    generation: String,
    started: bool,
    finished: bool,
}

impl DaemonStore {
    pub(crate) fn begin_shell_attachment(
        &self,
        session: &str,
    ) -> Result<ShellAdmission, DaemonError> {
        let mut state = self.lock();
        if state
            .shells
            .get(session)
            .is_some_and(|shell| shell.state == ShellState::CleanupUncertain)
        {
            return Err(DaemonError::ShellCleanupUncertain(format!(
                "{session}; exit other shells and use host `marsh reset` or `marsh stop`"
            )));
        }
        if !state
            .shells
            .get(session)
            .is_some_and(|shell| shell.state == ShellState::Attached)
            || !state.session_authorities.contains_key(session)
        {
            return Err(DaemonError::NotFound(session.into()));
        }
        if state.shell_attachments.contains_key(session) {
            return Err(DaemonError::ShellAttachmentBusy(session.into()));
        }
        let generation = uuid::Uuid::new_v4().to_string();
        state
            .shell_attachments
            .insert(session.into(), generation.clone());
        Ok(ShellAdmission {
            store: self.clone(),
            session: session.into(),
            generation,
            started: false,
            finished: false,
        })
    }
}

impl ShellAdmission {
    pub(crate) fn started(&mut self) {
        self.started = true;
    }

    pub(crate) fn finish(mut self) -> Result<(), DaemonError> {
        let mut state = self.store.lock();
        if state.shell_attachments.get(&self.session) != Some(&self.generation) {
            return Err(DaemonError::InvalidState(
                "shell attachment generation changed before completion".into(),
            ));
        }
        state.shell_attachments.remove(&self.session);
        self.finished = true;
        detach_locked(&mut state, &self.session)
    }
}

impl Drop for ShellAdmission {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.store.lock();
        if state.shell_attachments.get(&self.session) != Some(&self.generation) {
            return;
        }
        state.shell_attachments.remove(&self.session);
        if self.started {
            // An unwound/lost backend owner is not evidence of guest absence.
            if let Some(shell) = state.shells.get_mut(&self.session) {
                shell.state = ShellState::CleanupUncertain;
            }
            state
                .authentication_tokens
                .retain(|_, owner| owner.as_deref() != Some(&self.session));
        } else {
            // No backend effect was admitted: failed acknowledgement/configuration
            // can release only this exact registration, never another generation.
            let _ = detach_locked(&mut state, &self.session);
        }
    }
}

pub(super) fn detach_locked(state: &mut State, session: &str) -> Result<(), DaemonError> {
    if state.shell_attachments.contains_key(session) {
        return Err(DaemonError::ShellAttachmentBusy(session.into()));
    }
    let shell = state
        .shells
        .get_mut(session)
        .ok_or_else(|| DaemonError::NotFound(session.into()))?;
    if shell.state == ShellState::CleanupUncertain {
        return Err(DaemonError::ShellCleanupUncertain(session.into()));
    }
    shell.state = ShellState::Detached;
    state
        .authentication_tokens
        .retain(|_, owner| owner.as_deref() != Some(session));
    state.session_authorities.remove(session);
    state.shell_cleanup_vms.remove(session);
    Ok(())
}
