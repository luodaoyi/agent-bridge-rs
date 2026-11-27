use super::*;

impl HookDiagnosticsTracker {
    fn record_accepted(&self) {
        self.accepted_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_unmatched(&self, worker: WorkerKind, kind: HookEventKind) {
        self.unmatched_count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.last_unmatched.lock() {
            *guard = Some((worker, kind, now_millis()));
        }
    }

    fn snapshot(&self) -> crate::protocol::HookDiagnostics {
        let last = self.last_unmatched.lock().ok().and_then(|guard| *guard);
        crate::protocol::HookDiagnostics {
            accepted_count: self.accepted_count.load(Ordering::Relaxed),
            unmatched_count: self.unmatched_count.load(Ordering::Relaxed),
            last_unmatched_worker: last.map(|(worker, _, _)| worker),
            last_unmatched_event: last.map(|(_, kind, _)| kind),
            last_unmatched_at_ms: last.map(|(_, _, at)| at),
        }
    }
}

impl SessionHost {
    pub(crate) fn apply_hook_event(
        &self,
        worker: WorkerKind,
        provider_session_id: &str,
        event: HookEvent,
    ) -> Result<bool> {
        let event_kind = event.kind;
        let session = {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            if let Some(handle) = registry.provider_sessions.get(provider_session_id) {
                let Some(session) = registry.sessions.get(handle) else {
                    self.hook_diagnostics.record_unmatched(worker, event_kind);
                    return Ok(false);
                };
                if session.worker() != worker {
                    self.hook_diagnostics.record_unmatched(worker, event_kind);
                    return Ok(false);
                }
                Arc::clone(session)
            } else if worker == WorkerKind::Codex {
                let Some(hook_cwd) = event
                    .cwd
                    .as_deref()
                    .and_then(|cwd| canonical_directory(Path::new(cwd)).ok())
                else {
                    self.hook_diagnostics.record_unmatched(worker, event_kind);
                    return Ok(false);
                };
                let mut candidates = registry
                    .sessions
                    .iter()
                    .filter(|(handle, session)| {
                        !registry.external_bound_handles.contains(*handle)
                            && session.is_active_codex_matching_cwd(&hook_cwd)
                    })
                    .filter_map(|(handle, session)| {
                        session
                            .created_at_ms()
                            .ok()
                            .map(|created| (created, handle.clone(), Arc::clone(session)))
                    })
                    .collect::<Vec<_>>();
                candidates
                    .sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
                let Some((_, matched_handle, matched_session)) = candidates.into_iter().next()
                else {
                    self.hook_diagnostics.record_unmatched(worker, event_kind);
                    return Ok(false);
                };
                registry
                    .provider_sessions
                    .insert(provider_session_id.to_owned(), matched_handle.clone());
                registry.external_bound_handles.insert(matched_handle);
                matched_session
            } else {
                self.hook_diagnostics.record_unmatched(worker, event_kind);
                return Ok(false);
            }
        };
        session.apply_hook_event(event)?;
        self.hook_diagnostics.record_accepted();
        Ok(true)
    }

    pub(crate) fn hook_diagnostics(&self) -> crate::protocol::HookDiagnostics {
        self.hook_diagnostics.snapshot()
    }
}
