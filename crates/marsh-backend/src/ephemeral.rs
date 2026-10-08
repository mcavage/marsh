//! Session-owned ephemeral Kit selectors. Authority is the existing native
//! private-slot token/ledger, not this in-memory cleanup map.
use super::*;

impl StockDaemonBackend {
    pub(super) fn kit_spec_for_session(
        &self,
        command: &str,
        session: &SessionSpec,
        store: &DaemonStore,
    ) -> Result<KitVmSpec, DaemonError> {
        let mut spec = self.kit_spec(command)?;
        if !session.ephemeral_home {
            return Ok(spec);
        }
        let token = store
            .ephemeral_home_token(&session.session_id)
            .ok_or_else(|| {
                DaemonError::InvalidState(
                    "ephemeral Kit requires the immutable host allocation binding".into(),
                )
            })?;
        self.sbx
            .ephemeral_home_grant(&token, &session.home_backing, &session.guest_home)
            .map_err(backend_error)?;
        spec.name = self
            .sbx
            .vm_name(
                VmPurpose::Kit,
                &format!(
                    "{}:ephemeral:{}",
                    spec.workload_kit.identity(),
                    session.session_id
                ),
            )
            .map_err(backend_error)?;
        // Reusing the private leaf also for the implicit Kit Create workspace
        // avoids a new ordinary kit-lifecycle history row for every session.
        spec.lifecycle_workspace.clone_from(&session.home_backing);
        self.ephemeral_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session.session_id.clone())
            .or_default()
            .insert(spec.name.clone(), spec.clone());
        Ok(spec)
    }

    pub(super) fn close_ephemeral_kits(
        &self,
        session: &str,
        store: &DaemonStore,
    ) -> Result<(), DaemonError> {
        if store.ephemeral_home_token(session).is_none() {
            return Ok(());
        }
        let specs = self
            .ephemeral_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .cloned()
            .unwrap_or_default();
        let scope = selected_home_identity(&self.config.daemon_home).map_err(backend_error)?;
        let keys = specs
            .values()
            .map(|spec| format!("{scope}:{}:{}", spec.workload_kit.identity(), spec.name))
            .collect::<Vec<_>>();
        let locks = {
            let mut all = self
                .kit_locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            keys.iter()
                .map(|key| {
                    all.entry(key.clone())
                        .or_insert_with(|| Arc::new(Mutex::new(())))
                        .clone()
                })
                .collect::<Vec<_>>()
        };
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut guards = Vec::new();
        for lock in &locks {
            loop {
                match lock.try_lock() {
                    Ok(guard) => {
                        guards.push(guard);
                        break;
                    }
                    Err(std::sync::TryLockError::Poisoned(error)) => {
                        guards.push(error.into_inner());
                        break;
                    }
                    Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => {
                        return Err(DaemonError::ShellCleanupUncertain(
                            "ephemeral Kit preparation has not drained; private slot retained"
                                .into(),
                        ));
                    }
                }
            }
        }
        let names = specs.keys().cloned().collect::<Vec<_>>();
        store.begin_workers_reset(&names).map_err(|error| {
            DaemonError::ShellCleanupUncertain(format!(
                "ephemeral Kit still active; HOME retained: {error}"
            ))
        })?;
        for (index, (name, spec)) in specs.iter().enumerate() {
            if let Err(error) = self.sbx.reset_kit_vm_before(spec, deadline) {
                let _ = store.finish_worker_reset(name, false);
                store.cancel_workers_reset(&names[index + 1..]);
                return Err(DaemonError::ShellCleanupUncertain(format!(
                    "ephemeral Kit {name} exact UUID removal not verified: {error}"
                )));
            }
            store.finish_worker_reset(name, true)?;
            self.ready_kits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|_, ready| ready.name != *name);
            self.registered_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(name);
        }
        // Closing the native token fenced all late private begins before these
        // locks were acquired. Removing cache selectors never releases authority.
        self.ephemeral_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
        let mut flights = self
            .kit_validation_flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in &keys {
            flights.remove(key);
        }
        let mut all_locks = self
            .kit_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in &keys {
            all_locks.remove(key);
        }
        store.finish_ephemeral_session(session);
        Ok(())
    }

    pub(super) fn remembered_ephemeral_kits(&self) -> Vec<KitVmSpec> {
        self.ephemeral_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flat_map(|specs| specs.values().cloned())
            .collect()
    }
}
