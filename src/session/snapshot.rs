use super::*;

impl Session {
    pub(super) fn state(&self) -> Result<SessionState> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        Ok(inner.to_state(now_millis(), self.cleanup_claimed.load(Ordering::Acquire)))
    }

    /// Next pure time-based client lease transition for this session, if any.
    /// Returns only a future deadline so waiters do not spin after the transition
    /// has already been observed via a subsequent list/show.
    pub(super) fn next_lifecycle_deadline_ms(&self, now: u64) -> Result<Option<u64>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        Ok(inner.next_lifecycle_deadline_ms(now, self.cleanup_claimed.load(Ordering::Acquire)))
    }
}

impl SessionInner {
    pub(super) fn to_state(&self, now: u64, cleanup_claimed: bool) -> SessionState {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (client_state, client_last_seen_at_ms, orphaned_at_ms, auto_close_at_ms) =
            self.client_lifecycle(now, cleanup_claimed);
        SessionState {
            session: self.session.clone(),
            worker: self.worker,
            client_kind: self.client_kind,
            owner: self.owner.clone(),
            client_session_id: self.client_session_id.clone(),
            client_state,
            client_lease_ms: self
                .client_lease
                .as_ref()
                .map(|_| self.orphan_policy.lease_ms),
            orphan_grace_ms: self
                .client_lease
                .as_ref()
                .map(|_| self.orphan_policy.grace_ms),
            client_last_seen_at_ms,
            orphaned_at_ms,
            auto_close_at_ms,
            phase: self.phase,
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            always_approve: self.always_approve,
            sandbox: self.sandbox,
            dangerously_bypass: self.dangerously_bypass,
            process_id: self.process_id,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            exit_code: self.exit_code,
            error: self.error.clone(),
            title: self.title.clone(),
            screen: Some(screen.contents()),
            rows,
            cols,
            screen_ansi_base64: BASE64.encode(screen.contents_formatted()),
            last_cursor: self.next_cursor,
            last_output_at_ms: self.last_output_at_ms,
            activity: projected_activity(self.phase, self.hook.activity),
            hook_event: self.hook.last_event,
            hook_at_ms: self.hook.last_event_at_ms,
            tool_name: self.hook.tool_name.clone(),
            waiting_reason: self.hook.waiting_reason.clone(),
        }
    }

    pub(super) fn next_lifecycle_deadline_ms(
        &self,
        now: u64,
        cleanup_claimed: bool,
    ) -> Option<u64> {
        if cleanup_claimed {
            return None;
        }
        let lease = self.client_lease.as_ref()?;
        let last_seen = lease.load(Ordering::Acquire);
        let lease_expires_at = last_seen.saturating_add(self.orphan_policy.lease_ms);
        // Connected while now < lease_expires_at; at equality the state has already
        // flipped. Schedule wake at lease_expires_at so the due send observes the
        // new Disconnected/Orphaned state (not a second Connected snapshot).
        if now < lease_expires_at {
            Some(lease_expires_at)
        } else {
            None
        }
    }

    pub(super) fn client_lifecycle(
        &self,
        now: u64,
        cleanup_claimed: bool,
    ) -> (ClientLeaseState, Option<u64>, Option<u64>, Option<u64>) {
        let Some(lease) = self.client_lease.as_ref() else {
            return (ClientLeaseState::Unmanaged, None, None, None);
        };
        let last_seen = lease.load(Ordering::Acquire);
        let lease_expires_at = last_seen.saturating_add(self.orphan_policy.lease_ms);
        if cleanup_claimed {
            return (ClientLeaseState::Closing, Some(last_seen), None, None);
        }
        // Inclusive expiry: Connected only strictly before lease_expires_at.
        if now < lease_expires_at {
            return (ClientLeaseState::Connected, Some(last_seen), None, None);
        }
        if !phase_is_safe_for_orphan_cleanup(self.phase) {
            return (ClientLeaseState::Disconnected, Some(last_seen), None, None);
        }
        let orphaned_at = lease_expires_at.max(self.phase_changed_at_ms);
        let auto_close_at = orphaned_at.saturating_add(self.orphan_policy.grace_ms);
        (
            ClientLeaseState::Orphaned,
            Some(last_seen),
            Some(orphaned_at),
            Some(auto_close_at),
        )
    }

    pub(super) fn orphan_cleanup_due(&self, now: u64) -> bool {
        let (_, _, _, auto_close_at) = self.client_lifecycle(now, false);
        auto_close_at.is_some_and(|deadline| now >= deadline)
    }

    pub(super) fn wait_result(
        &self,
        condition: WaitCondition,
        satisfied: bool,
        timed_out: bool,
        blocked_reason: Option<&str>,
    ) -> WaitResult {
        WaitResult {
            session: self.session.clone(),
            condition,
            satisfied,
            timed_out,
            phase: self.phase,
            exit_code: self.exit_code,
            blocked_reason: blocked_reason.map(str::to_owned),
        }
    }
}
