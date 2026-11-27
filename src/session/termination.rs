use super::*;

impl Session {
    pub(super) fn claim_orphan_cleanup(&self, now: u64) -> Result<bool> {
        if self.cleanup_claimed.load(Ordering::Acquire)
            || self.cleanup_committed.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if !inner.orphan_cleanup_due(now) {
            return Ok(false);
        }
        // Claim while the session lock is still held. Input and phase changes
        // take the same lock, so an idle session cannot become Running between
        // the eligibility check and the claim.
        Ok(self
            .cleanup_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok())
    }

    /// Recheck the lease and phase immediately before cleanup becomes
    /// irreversible. The caller holds the host registry lock, serializing this
    /// commit with every lease refresh — Codex keepalives and inbound WebUI
    /// terminal commands alike — so a refresh that lands first cancels the
    /// provisional claim.
    pub(super) fn commit_orphan_cleanup(&self, now: u64) -> Result<bool> {
        if !self.cleanup_claimed.load(Ordering::Acquire)
            || self.cleanup_committed.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if !self.cleanup_claimed.load(Ordering::Acquire) {
            return Ok(false);
        }
        if !inner.orphan_cleanup_due(now) {
            self.cleanup_claimed.store(false, Ordering::Release);
            return Ok(false);
        }
        Ok(self
            .cleanup_committed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok())
    }

    pub(super) fn cancel_uncommitted_cleanup_for_client(
        &self,
        client_session_id: &str,
    ) -> Result<bool> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if inner.client_session_id.as_deref() != Some(client_session_id) {
            return Ok(false);
        }
        Ok(self.cancel_uncommitted_cleanup_locked())
    }

    pub(super) fn cancel_uncommitted_orphan_cleanup(&self) -> Result<bool> {
        let _inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        Ok(self.cancel_uncommitted_cleanup_locked())
    }

    pub(super) fn cancel_uncommitted_cleanup_locked(&self) -> bool {
        !self.cleanup_committed.load(Ordering::Acquire)
            && self.cleanup_claimed.swap(false, Ordering::AcqRel)
    }

    pub(super) fn reset_orphan_cleanup(&self) {
        self.cleanup_committed.store(false, Ordering::Release);
        self.cleanup_claimed.store(false, Ordering::Release);
    }

    /// One bounded close attempt. Marks the session as shutdown-requested,
    /// escalates termination of the owned process scope within `deadline`, and
    /// finalizes once termination is verified. On `Timeout`/`Failed` the
    /// process and PTY ownership is retained so a later attempt can continue
    /// from observed state (see `TerminationLevel`).
    pub(super) fn close_attempt(&self, deadline: Instant) -> CloseOutcome {
        let _close_guard = match self.try_acquire_close_lock(deadline) {
            Ok(guard) => guard,
            Err(outcome) => return outcome,
        };
        self.shutdown.store(true, Ordering::Release);
        if self.process_is_done_or_terminal() {
            return self.finalize_now();
        }
        match self.escalate_until_done(deadline) {
            EscalationResult::Done => self.finalize_now(),
            EscalationResult::Timeout => CloseOutcome::Timeout,
            EscalationResult::Failed(message) => CloseOutcome::Failed(message),
        }
    }

    pub(super) fn try_acquire_close_lock(
        &self,
        deadline: Instant,
    ) -> std::result::Result<MutexGuard<'_, ()>, CloseOutcome> {
        loop {
            match self.close_lock.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(CloseOutcome::Failed(
                        "session close lock was poisoned".to_owned(),
                    ));
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(CloseOutcome::Timeout);
                    }
                    thread::sleep(remaining.min(Duration::from_millis(10)));
                }
            }
        }
    }

    /// True only when the owned process scope is *verified* gone. A terminal
    /// phase alone is not proof: on Unix the waiter observing the root exit
    /// does not prove that a descendant inside the same process group
    /// disappeared, and on Windows the root exit does not prove the descendant
    /// tree is gone. The immutable `scope_id` drives the liveness probe; a
    /// probe failure is never treated as done. Always asks for the stable
    /// verification because this decision finalizes the record.
    pub(super) fn process_is_done_or_terminal(&self) -> bool {
        matches!(self.probe_scope(true), ScopeAlive::Gone)
    }

    /// Probe the owned process scope. Unix uses the immutable process-group id;
    /// Windows queries the Job Object that contains the gated launcher and all
    /// of its descendants. `stable_verification` is passed through to the Unix
    /// probe so the escalation loop can skip the expensive all-zombie scan
    /// until its final confirmation window.
    pub(super) fn probe_scope(&self, stable_verification: bool) -> ScopeAlive {
        // Metadata-only fixtures own no OS scope and must never probe or signal one.
        #[cfg(test)]
        if self.scope_id == 0 {
            return ScopeAlive::Gone;
        }
        #[cfg(unix)]
        {
            process_scope_alive(self.scope_id, stable_verification)
        }
        #[cfg(windows)]
        {
            let _ = stable_verification;
            self.scope_job.as_ref().map_or_else(
                || process_alive(self.scope_id),
                WindowsJob::process_scope_alive,
            )
        }
    }

    /// Classify the owned process scope from the immutable `scope_id` and the
    /// platform liveness probe. `Gone` requires the probe to verify the whole
    /// scope is gone; the waiter observing the root exit alone is never
    /// treated as the whole scope being gone.
    pub(super) fn scope_state(&self, stable_verification: bool) -> ScopeState {
        match self.probe_scope(stable_verification) {
            ScopeAlive::Gone => ScopeState::Gone,
            ScopeAlive::Alive => ScopeState::Alive,
            ScopeAlive::Unknown => ScopeState::Unknown,
        }
    }

    /// Record that the owned process scope has been verified gone. The waiter
    /// waits only on the root process, so the liveness probe is the authority
    /// for the whole group; the exit code stays unknown when the waiter never
    /// reported one.
    pub(super) fn record_verified_process_done(&self) {
        if let Ok(mut inner) = self.inner.lock()
            && !inner.process_done
        {
            inner.process_done = true;
            inner.updated_at_ms = now_millis();
        }
    }

    /// Send HUP, then TERM, then KILL to the owned process scope within one
    /// monotonic absolute deadline, verifying the whole scope is gone (not
    /// just the root). Escalation milestones honor a minimum grace so a
    /// graceful child is not force-killed prematurely; the highest level
    /// already sent is remembered across attempts on Unix. Windows applies the
    /// same bounded schedule to the kernel-owned Job scope, where the HUP and
    /// TERM phases wait for natural exit and only KILL terminates the job.
    pub(super) fn escalate_until_done(&self, deadline: Instant) -> EscalationResult {
        let started_at = Instant::now();
        let budget = deadline.saturating_duration_since(started_at);
        if budget.is_zero() {
            return EscalationResult::Timeout;
        }
        let term_after = escalation_milestone(budget, 1, 3);
        let kill_after = escalation_milestone(budget, 2, 3)
            .max(term_after + Duration::from_millis(TERMINATION_GRACE_MIN_MS));
        let term_at = started_at + term_after;
        let kill_at = started_at + kill_after;
        let stable_scan_at = deadline
            .checked_sub(Duration::from_millis(STABLE_SCOPE_SCAN_LEAD_MS))
            .unwrap_or(deadline);
        let mut last_target = TerminationLevel::None;
        loop {
            let now = Instant::now();
            // Outside the final confirmation window a successful kill(0) probe
            // answers Alive immediately; only near the deadline do we walk
            // /proc (twice) to distinguish a living member from an all-zombie
            // group, so each round costs one syscall instead of two scans.
            match self.scope_state(now >= stable_scan_at) {
                ScopeState::Gone => {
                    self.record_verified_process_done();
                    return EscalationResult::Done;
                }
                ScopeState::Alive | ScopeState::Unknown => {}
            }
            if now >= deadline {
                return EscalationResult::Timeout;
            }
            let target = if now < term_at {
                TerminationLevel::Hup
            } else if now < kill_at {
                TerminationLevel::Term
            } else {
                TerminationLevel::Kill
            };
            let repeat_final_signal = cfg!(unix) && target == TerminationLevel::Kill;
            if target != last_target || repeat_final_signal {
                if let Err(error) = self.escalate_to(target) {
                    return EscalationResult::Failed(format!(
                        "failed to terminate Grok process scope: {error}"
                    ));
                }
                last_target = target;
            }
            let next_milestone = match target {
                TerminationLevel::Hup => term_at,
                TerminationLevel::Term => kill_at,
                TerminationLevel::Kill | TerminationLevel::None => deadline,
            };
            let wait = next_milestone
                .min(deadline)
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50))
                .max(Duration::from_millis(1));
            // Wait on session changes (reader progress, waiter report) without
            // treating a terminal phase as proof the scope is gone: the liveness
            // probe at the top of the loop remains the authority.
            let inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => {
                    return EscalationResult::Failed("session state lock was poisoned".into());
                }
            };
            let _ = self
                .changed
                .wait_timeout(inner, wait)
                .map_err(|_| anyhow::anyhow!("session wait lock was poisoned"));
        }
    }

    /// Final forced termination used by `shutdown_all` for sessions the bounded
    /// group close could not stop. Skips the HUP/TERM ladder and sends KILL
    /// directly (Unix process-group SIGKILL / Windows `TerminateJobObject`),
    /// then reaps until the liveness probe verifies the whole scope is gone or
    /// `deadline` passes. The signal is repeated while the scope stays alive
    /// because a member can fork into the group concurrently with the first
    /// signal. Never reports `Done` without the probe certifying the scope
    /// gone; a transient send failure is remembered and retried.
    pub(super) fn force_terminate_scope(&self, deadline: Instant) -> EscalationResult {
        let stable_scan_at = deadline
            .checked_sub(Duration::from_millis(STABLE_SCOPE_SCAN_LEAD_MS))
            .unwrap_or(deadline);
        let mut last_error: Option<String> = None;
        loop {
            let now = Instant::now();
            if let ScopeAlive::Gone = self.probe_scope(now >= stable_scan_at) {
                self.record_verified_process_done();
                return EscalationResult::Done;
            }
            if now >= deadline {
                return match last_error {
                    Some(message) => EscalationResult::Failed(format!(
                        "final forced termination could not be verified: {message}"
                    )),
                    None => EscalationResult::Timeout,
                };
            }
            if let Err(error) = self.escalate_to(TerminationLevel::Kill) {
                // Transient failures can clear on the next round; keep sending.
                last_error = Some(error.to_string());
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50))
                .max(Duration::from_millis(1));
            let inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => {
                    return EscalationResult::Failed("session state lock was poisoned".into());
                }
            };
            let _ = self
                .changed
                .wait_timeout(inner, wait)
                .map_err(|_| anyhow::anyhow!("session wait lock was poisoned"));
        }
    }

    /// Send the next escalation level once; a level already sent is not sent
    /// again unless a later attempt asks for a stronger one. `SIGKILL` is the
    /// exception on Unix: repeat it while the group remains alive because a
    /// member can fork into the group concurrently with the first signal.
    /// Windows termination targets the Job Object, whose membership is fixed
    /// by the launch gate.
    pub(super) fn escalate_to(&self, target: TerminationLevel) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let mut state = match self.termination.lock() {
                Ok(state) => state,
                Err(_) => {
                    return Err(std::io::Error::other("termination state lock was poisoned"));
                }
            };
            if target < *state || (target == *state && target != TerminationLevel::Kill) {
                return Ok(());
            }
            send_termination_signal(self.scope_id, target)?;
            if target > *state {
                *state = target;
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            match self.scope_job.as_ref() {
                Some(job) => job.terminate(target),
                None => terminate_windows_process(self.scope_id, target),
            }
        }
    }

    /// Finalize a session whose process scope is *verified* terminated. The
    /// caller (`close_attempt`) reaches this only after the liveness probe
    /// reported the whole scope gone (or a previously verified close cleared
    /// the retained scope id). Idempotent and safe on already-complete
    /// sessions; always releases the PTY master and writer so a blocked reader
    /// edge unblocks. Returns `Failed` instead of claiming `Closed` when
    /// finalization could not be completed (e.g. the state lock is poisoned).
    pub(super) fn finalize_now(&self) -> CloseOutcome {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => {
                return CloseOutcome::Failed(
                    "session could not be finalized: state lock was poisoned".to_owned(),
                );
            }
        };
        let already_terminal = phase_is_terminal(inner.phase);
        if !inner.process_done && !already_terminal {
            // The liveness probe verified the whole scope is gone but the
            // waiter has not reported yet; the probe is the authority for the
            // entire process scope.
            inner.process_done = true;
            inner.updated_at_ms = now_millis();
        }
        let finalized = finalize_session(&mut inner, true) || already_terminal;
        if finalized {
            // The record is fully closed: never expose a pid for a finalized
            // session. finalize_session clears it on the transition; this also
            // covers the already-terminal path. The immutable scope_id remains
            // available on Session for any later verification.
            inner.process_id = None;
            inner.updated_at_ms = now_millis();
        }
        drop(inner);
        if finalized {
            self.close_writer();
            self.release_master();
            self.signal_changed();
            CloseOutcome::Closed
        } else {
            CloseOutcome::Failed(
                "session could not be finalized: the process scope was not verifiably terminated"
                    .to_owned(),
            )
        }
    }

    /// Bounded best-effort escalation used by reader/writer/waiter error edges.
    /// Converges finalization only when the scope is verified gone; timeout and
    /// failure keep ownership and state so a later close can retry.
    pub(super) fn run_error_edge_escalation(&self) {
        let deadline = Instant::now() + Duration::from_millis(ERROR_ESCALATION_TIMEOUT_MS);
        let Ok(_close_guard) = self.try_acquire_close_lock(deadline) else {
            return;
        };
        match self.escalate_until_done(deadline) {
            EscalationResult::Done => {
                let shutdown = self.shutdown.load(Ordering::Acquire);
                let finalized = if let Ok(mut inner) = self.inner.lock() {
                    finalize_session(&mut inner, shutdown)
                } else {
                    false
                };
                self.finish_transition(finalized);
            }
            EscalationResult::Timeout => {
                // Scope still live or unverifiable: keep ownership and state.
            }
            EscalationResult::Failed(error) => {
                self.record_secondary_error(format!(
                    "failed to terminate Grok after an I/O edge failed: {error}"
                ));
            }
        }
    }

    pub(super) fn record_secondary_error(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            record_error(&mut inner, message);
        }
        self.signal_changed();
    }

    pub(super) fn close_writer(&self) {
        if let Ok(mut writer) = self.writer_tx.lock() {
            writer.take();
        }
    }

    pub(super) fn release_master(&self) {
        if let Ok(mut master) = self.master.lock() {
            master.take();
        }
    }

    pub(super) fn finish_transition(&self, finalized: bool) {
        if finalized {
            self.close_writer();
            self.release_master();
        }
        self.signal_changed();
    }
}
