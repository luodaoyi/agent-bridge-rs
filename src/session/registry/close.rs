use super::*;

impl SessionHost {
    pub(crate) fn close(&self, handle: &str) -> std::result::Result<bool, CloseError> {
        let session = {
            let registry = self
                .registry
                .lock()
                .map_err(|_| CloseError::Failed("session registry lock was poisoned".into()))?;
            registry.sessions.get(handle).cloned()
        };
        let Some(session) = session else {
            let registry = self
                .registry
                .lock()
                .map_err(|_| CloseError::Failed("session registry lock was poisoned".into()))?;
            if registry.was_closed(handle) {
                // Idempotent re-close after an earlier successful close.
                return Ok(true);
            }
            return Err(CloseError::NotFound(handle.to_owned()));
        };
        match session.close_attempt(default_close_deadline()) {
            CloseOutcome::Closed => {}
            CloseOutcome::Timeout => {
                // Ownership (killer, PTY master, writer) is retained so a later
                // close attempt can continue termination from observed state.
                return Err(CloseError::Timeout);
            }
            CloseOutcome::Failed(message) => return Err(CloseError::Failed(message)),
        }
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| CloseError::Failed("session registry lock was poisoned".into()))?;
        registry.remove_session(handle, &session);
        registry.remember_closed(handle);
        drop(registry);
        self.notify_revision();
        Ok(true)
    }

    pub(crate) fn close_owner(&self, owner: &str) -> Result<CloseGroupResult> {
        validate_owner(owner)?;
        let sessions = {
            let registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            let mut sessions = Vec::new();
            for (handle, session) in &registry.sessions {
                if session.has_owner(owner)? {
                    sessions.push((handle.clone(), Arc::clone(session)));
                }
            }
            sessions
        };
        let outcomes =
            self.run_close_group(sessions, default_group_deadline(), GROUP_CLOSE_WORKERS)?;
        let (result, _not_closed) = self.apply_close_outcomes(outcomes)?;
        Ok(result)
    }

    pub(crate) fn close_client(&self, client_session_id: &str) -> Result<CloseGroupResult> {
        validate_client_session_id(client_session_id)?;
        let sessions = {
            let registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            let mut sessions = Vec::new();
            for (handle, session) in &registry.sessions {
                if session.has_client(client_session_id)? {
                    sessions.push((handle.clone(), Arc::clone(session)));
                }
            }
            sessions
        };
        let outcomes =
            self.run_close_group(sessions, default_group_deadline(), GROUP_CLOSE_WORKERS)?;
        let (result, _not_closed) = self.apply_close_outcomes(outcomes)?;
        if result.failures.is_empty() && result.timed_out.is_empty() {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            let mut has_sessions = false;
            for session in registry.sessions.values() {
                if session.has_client(client_session_id)? {
                    has_sessions = true;
                    break;
                }
            }
            if !has_sessions {
                registry.clients.remove(client_session_id);
            }
            drop(registry);
            // The client lease map changed after apply_close_outcomes already
            // notified; wake the WebUI again so it observes the removal.
            self.notify_revision();
            return Ok(result);
        }
        // apply_close_outcomes already notifies when sessions matched; wake
        // explicitly when nothing matched.
        if result.matched == 0 {
            self.notify_revision();
        }
        Ok(result)
    }

    pub(crate) fn reap_orphans(&self) -> Result<CloseGroupResult> {
        let now = now_millis();
        let mut failures = Vec::new();
        let candidates = {
            let registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            let mut sessions = Vec::new();
            for (handle, session) in &registry.sessions {
                match session.claim_orphan_cleanup(now) {
                    Ok(true) => sessions.push((handle.clone(), Arc::clone(session))),
                    Ok(false) => {}
                    Err(error) => failures.push(format!("{handle}: {error}")),
                }
            }
            sessions
        };

        let mut sessions = Vec::new();
        for (handle, session) in &candidates {
            let registry = match self.registry.lock() {
                Ok(registry) => registry,
                Err(_) => {
                    failures.push("session registry lock was poisoned".to_owned());
                    break;
                }
            };
            let still_registered = registry
                .sessions
                .get(handle)
                .is_some_and(|current| Arc::ptr_eq(current, session));
            if still_registered {
                match session.commit_orphan_cleanup(now_millis()) {
                    Ok(true) => sessions.push((handle.clone(), Arc::clone(session))),
                    Ok(false) => {}
                    Err(error) => failures.push(format!("{handle}: {error}")),
                }
            }
        }
        // Release this sweep's provisional claims even after a commit error.
        for (_, session) in &candidates {
            session.cancel_uncommitted_cleanup_locked();
        }
        if !sessions.is_empty() {
            // Surface ClientLeaseState::Closing only after the final lease and
            // phase recheck commits cleanup.
            self.notify_revision();
        }
        let outcomes =
            self.run_close_group(sessions, default_group_deadline(), GROUP_CLOSE_WORKERS)?;
        let (mut result, not_closed) = self.apply_close_outcomes(outcomes)?;
        result.failures.extend(failures);
        // Timed-out or failed orphans stay registered so the next reaper tick
        // can claim and retry them from observed state.
        for (_, session) in not_closed {
            session.reset_orphan_cleanup();
        }
        Ok(result)
    }

    /// Close a snapshot of sessions with a fixed worker bound and one absolute
    /// deadline shared by every session, independent of group size.
    fn run_close_group(
        &self,
        sessions: Vec<(String, Arc<Session>)>,
        deadline: Instant,
        workers: usize,
    ) -> Result<Vec<(String, Arc<Session>, CloseOutcome)>> {
        let matched = sessions.len();
        if matched == 0 {
            return Ok(Vec::new());
        }
        let workers = workers.clamp(1, matched);
        let (work_tx, work_rx) = channel::<(String, Arc<Session>)>();
        let (done_tx, done_rx) = channel::<(String, Arc<Session>, CloseOutcome)>();
        let queue = Arc::new(Mutex::new(work_rx));
        let mut pool = Vec::with_capacity(workers);
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let done_tx = done_tx.clone();
            pool.push(thread::spawn(move || {
                loop {
                    let job = match queue.lock() {
                        Ok(queue) => queue.recv(),
                        Err(_) => break,
                    };
                    let Ok((handle, session)) = job else {
                        break;
                    };
                    let outcome = session.close_attempt(deadline);
                    if done_tx.send((handle, session, outcome)).is_err() {
                        break;
                    }
                }
            }));
        }
        // Track every session whose result is still outstanding so a stalled
        // worker can be reported as timed out instead of hanging the collector.
        let mut pending: HashMap<String, Arc<Session>> = sessions
            .iter()
            .map(|(handle, session)| (handle.clone(), Arc::clone(session)))
            .collect();
        for (handle, session) in sessions {
            let _ = work_tx.send((handle, session));
        }
        drop(work_tx);
        drop(done_tx);

        // Workers each honor the shared absolute deadline, but the collector
        // never waits past it plus a small response margin: close_attempt may
        // legitimately finish just after the deadline, and a hung worker must
        // not make the whole group wait forever.
        let recv_deadline = deadline + Duration::from_millis(GROUP_CLOSE_RESPONSE_MARGIN_MS);
        let mut outcomes = Vec::with_capacity(matched);
        while outcomes.len() < matched {
            let remaining = recv_deadline.saturating_duration_since(Instant::now());
            match done_rx.recv_timeout(remaining) {
                Ok((handle, session, outcome)) => {
                    pending.remove(&handle);
                    outcomes.push((handle, session, outcome));
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Report every still-pending session as timed out; each
                    // keeps its process ownership so a later close can retry.
                    for (handle, session) in pending.drain() {
                        outcomes.push((handle, session, CloseOutcome::Timeout));
                    }
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("a group close worker exited without reporting its result");
                }
            }
        }
        // Workers exit as soon as their queue recv fails (work_tx was dropped
        // above). Plain JoinHandle::join is deliberately NOT used: a worker
        // still inside close_attempt at the response margin could block the
        // caller forever. close_attempt is strictly bounded by the shared
        // absolute deadline, so dropping the handles detaches workers that
        // finish on their own shortly after the collector returns.
        drop(pool);
        Ok(outcomes)
    }

    /// Remove closed sessions from the registry, tombstone their handles, and
    /// build the truthful per-session group result. `not_closed` carries the
    /// sessions that timed out or failed so callers can retry or reset state.
    pub(in crate::session) fn apply_close_outcomes(
        &self,
        outcomes: Vec<(String, Arc<Session>, CloseOutcome)>,
    ) -> Result<(CloseGroupResult, CloseGroupNotClosed)> {
        let matched = outcomes.len();
        let mut closed = 0;
        let mut timed_out = Vec::new();
        let mut failures = Vec::new();
        let mut not_closed = Vec::new();
        for (handle, session, outcome) in outcomes {
            match outcome {
                CloseOutcome::Closed => {
                    closed += 1;
                    let mut registry = self
                        .registry
                        .lock()
                        .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
                    registry.remove_session(&handle, &session);
                    registry.remember_closed(&handle);
                }
                CloseOutcome::Timeout => {
                    not_closed.push((handle.clone(), session));
                    timed_out.push(format!(
                        "{handle}: close timed out after the shared group deadline; \
                         retry close to continue termination"
                    ));
                }
                CloseOutcome::Failed(message) => {
                    not_closed.push((handle.clone(), session));
                    failures.push(format!("{handle}: {message}"));
                }
            }
        }
        if closed > 0 || matched > 0 {
            // Removal and Closing (cleanup_claimed) transitions must wake WebUI.
            self.notify_revision();
        }
        Ok((
            CloseGroupResult {
                matched,
                closed,
                timed_out,
                failures,
            },
            not_closed,
        ))
    }

    /// Stop every session and reap every owned process scope before the server
    /// exits. The bounded group close runs first; sessions it could not stop
    /// (Timeout or Failed) keep their process and PTY ownership, and this exit
    /// path then force-terminates and reaps each surviving scope within its own
    /// fresh bounded window. Server exit therefore never orphans a Unix process
    /// group or a Windows job scope, whether it was triggered by ServerStop or
    /// by the accept loop ending. A structural group-close failure (broken
    /// worker channel, poisoned registry) is not fatal to the cleanup: the
    /// snapshot then retains every session and the final forced pass covers all
    /// of them, with the structural error merged into the result. Only scopes
    /// that are *still* alive after their own forced window are reported.
    pub(crate) fn shutdown_all(&self) -> Result<()> {
        let sessions = {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            registry.accepting = false;
            registry
                .sessions
                .iter()
                .map(|(handle, session)| (handle.clone(), Arc::clone(session)))
                .collect::<Vec<_>>()
        };
        // Server exit is the last process owner: whichever sessions the bounded
        // group close could not verify closed (Timeout, Failed, or a structural
        // error that prevented per-session outcomes) must be force-terminated
        // here instead of letting the host drop with the scope still running.
        let (not_closed, close_error) = match self.run_close_group(
            sessions.clone(),
            default_group_deadline(),
            GROUP_CLOSE_WORKERS,
        ) {
            Ok(outcomes) => match self.apply_close_outcomes(outcomes) {
                Ok((_result, leftovers)) => (leftovers, None),
                Err(error) => {
                    // The registry lock is poisoned and no outcome was applied:
                    // every snapshot session still owns its scope.
                    (sessions, Some(error))
                }
            },
            Err(error) => {
                // The group close failed structurally without per-session
                // outcomes; the snapshot still owns every scope.
                (sessions, Some(error))
            }
        };
        // Each surviving session gets its own forced-termination window so a
        // slow first scope cannot consume a shared deadline and starve the
        // remaining ones of their Kill signal.
        let mut survivors = Vec::new();
        for (handle, session) in not_closed {
            let final_deadline =
                Instant::now() + Duration::from_millis(u64::from(PROCESS_TERMINATE_TIMEOUT_MS));
            match session.force_terminate_scope(final_deadline) {
                EscalationResult::Done => {
                    if session.finalize_now() != CloseOutcome::Closed {
                        survivors.push(format!(
                            "{handle}: close could not be finalized after final termination"
                        ));
                    }
                }
                EscalationResult::Timeout => survivors.push(format!(
                    "{handle}: process scope survived the final forced termination window"
                )),
                EscalationResult::Failed(message) => survivors.push(format!("{handle}: {message}")),
            }
        }
        self.notify_revision();
        if let Some(error) = close_error {
            survivors.push(format!("group close failed: {error:#}"));
        }
        if survivors.is_empty() {
            Ok(())
        } else {
            bail!(
                "failed to stop one or more sessions: {}",
                survivors.join("; ")
            )
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CloseError {
    NotFound(String),
    Timeout,
    Failed(String),
}

impl fmt::Display for CloseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(handle) => write!(formatter, "session not found: {handle}"),
            Self::Timeout => write!(
                formatter,
                "close timed out after {PROCESS_TERMINATE_TIMEOUT_MS} ms; \
                 the Grok process is still running; retry close to continue termination"
            ),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CloseError {}
