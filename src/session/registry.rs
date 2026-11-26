use super::*;

#[derive(Clone, Copy)]
pub(crate) struct OrphanPolicy {
    pub(crate) lease_ms: u64,
    pub(crate) grace_ms: u64,
}

impl OrphanPolicy {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            lease_ms: parse_duration_env(
                "AGENT_BRIDGE_CLIENT_LEASE_SECONDS",
                DEFAULT_CODEX_LEASE_SECONDS,
                MIN_CODEX_LEASE_SECONDS,
                MAX_CODEX_LEASE_SECONDS,
            )?
            .saturating_mul(1_000),
            grace_ms: parse_duration_env(
                "AGENT_BRIDGE_ORPHAN_GRACE_SECONDS",
                DEFAULT_ORPHAN_GRACE_SECONDS,
                MIN_ORPHAN_GRACE_SECONDS,
                MAX_ORPHAN_GRACE_SECONDS,
            )?
            .saturating_mul(1_000),
        })
    }
}

/// Global host revision used by the read-only WebUI `/api/events` stream.
/// Every session metadata or terminal change bumps the revision and wakes waiters.
pub(crate) struct HostRevision {
    state: Mutex<u64>,
    changed: Condvar,
}

impl HostRevision {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(0),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn current(&self) -> u64 {
        self.state.lock().map(|guard| *guard).unwrap_or(0)
    }

    pub(crate) fn bump(&self) {
        let Ok(mut revision) = self.state.lock() else {
            return;
        };
        *revision = revision.wrapping_add(1);
        self.changed.notify_all();
    }

    pub(crate) fn wait_for_change(&self, seen: u64, timeout: Duration) -> u64 {
        let Ok(revision) = self.state.lock() else {
            return seen;
        };
        if *revision != seen {
            return *revision;
        }
        let Ok(result) = self.changed.wait_timeout(revision, timeout) else {
            return seen;
        };
        *result.0
    }
}

pub(crate) struct SessionHost {
    pub(super) registry: Mutex<SessionRegistry>,
    pub(super) next_id: AtomicU64,
    pub(super) orphan_policy: OrphanPolicy,
    pub(super) revision: Arc<HostRevision>,
}

pub(super) struct SessionRegistry {
    pub(super) accepting: bool,
    pub(super) sessions: HashMap<String, Arc<Session>>,
    pub(super) provider_sessions: HashMap<String, String>,
    /// Session handles that already received an external provider session binding
    /// (used for Codex hook dynamic binding fallback when custom env is stripped).
    pub(super) external_bound_handles: HashSet<String>,
    pub(super) clients: HashMap<String, Arc<AtomicU64>>,
    /// Recently closed session handles (handle -> closed_at_ms) kept so a
    /// repeated single close returns an idempotent already-closed result
    /// instead of "session not found".
    pub(super) closed: HashMap<String, u64>,
}

impl SessionRegistry {
    fn remove_session(&mut self, handle: &str, session: &Arc<Session>) -> bool {
        if !self
            .sessions
            .get(handle)
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            return false;
        }
        let client_session_id = session.client_session_id().ok().flatten();
        self.sessions.remove(handle);
        self.external_bound_handles.remove(handle);
        self.provider_sessions
            .retain(|_, mapped_handle| mapped_handle != handle);
        if let Some(client_session_id) = client_session_id
            && !self
                .sessions
                .values()
                .any(|remaining| remaining.has_client(&client_session_id).unwrap_or(false))
        {
            self.clients.remove(&client_session_id);
        }
        true
    }

    pub(super) fn remember_closed(&mut self, handle: &str) {
        let now = now_millis();
        if self.closed.len() >= CLOSED_TOMBSTONE_CAP {
            // Evict the OLDEST half when the tombstone grows past its cap so the
            // most recent (and most likely to be re-closed) handles survive.
            let mut entries = self
                .closed
                .drain()
                .map(|(handle, at)| (at, handle))
                .collect::<Vec<_>>();
            entries.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
            entries.truncate(CLOSED_TOMBSTONE_CAP / 2);
            self.closed = entries
                .into_iter()
                .map(|(at, handle)| (handle, at))
                .collect();
        }
        self.closed.insert(handle.to_owned(), now);
    }

    pub(super) fn was_closed(&self, handle: &str) -> bool {
        self.closed
            .get(handle)
            .is_some_and(|at| now_millis().saturating_sub(*at) <= CLOSED_TOMBSTONE_TTL_MS)
    }
}

impl SessionHost {
    pub(crate) fn new(orphan_policy: OrphanPolicy) -> Self {
        Self {
            registry: Mutex::new(SessionRegistry {
                accepting: true,
                sessions: HashMap::new(),
                provider_sessions: HashMap::new(),
                external_bound_handles: HashSet::new(),
                clients: HashMap::new(),
                closed: HashMap::new(),
            }),
            next_id: AtomicU64::new(1),
            orphan_policy,
            revision: Arc::new(HostRevision::new()),
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.current()
    }

    pub(crate) fn notify_revision(&self) {
        self.revision.bump();
    }

    pub(crate) fn wait_revision(&self, seen: u64, timeout: Duration) -> u64 {
        self.revision.wait_for_change(seen, timeout)
    }

    /// Earliest *future* wall-clock deadline at which any session's client lease
    /// state would change without another host revision (Connected →
    /// Disconnected/Orphaned). Returns only future deadlines so waiters do not
    /// spin after the transition is already reflected in live list/show state.
    pub(crate) fn next_client_lifecycle_deadline_ms(&self) -> Result<Option<u64>> {
        let now = now_millis();
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
        let mut next: Option<u64> = None;
        for session in registry.sessions.values() {
            if let Some(deadline) = session.next_lifecycle_deadline_ms(now)? {
                next = Some(match next {
                    Some(current) => current.min(deadline),
                    None => deadline,
                });
            }
        }
        Ok(next)
    }

    pub(crate) fn touch_client(&self, client_session_id: &str) -> Result<()> {
        self.touch_client_at(client_session_id, now_millis())
    }

    pub(super) fn touch_client_at(&self, client_session_id: &str, now: u64) -> Result<()> {
        validate_client_session_id(client_session_id)?;
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
        registry
            .clients
            .entry(client_session_id.to_owned())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .store(now, Ordering::Release);
        for session in registry.sessions.values() {
            session.cancel_uncommitted_cleanup_for_client(client_session_id)?;
        }
        drop(registry);
        self.notify_revision();
        Ok(())
    }

    /// Refresh every managed session's client lease in response to an inbound
    /// WebUI command. The WebUI's global event stream itself never refreshes
    /// leases — only inbound terminal commands (and Codex keepalives via
    /// touch_client) prove the client is attached right now, so each inbound
    /// command cancels any provisional orphan cleanup before the PTY is
    /// touched.
    pub(crate) fn touch_web_clients(&self) -> Result<usize> {
        self.touch_web_clients_at(now_millis())
    }

    pub(super) fn touch_web_clients_at(&self, now: u64) -> Result<usize> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
        let refreshed = registry.clients.len();
        for lease in registry.clients.values() {
            lease.store(now, Ordering::Release);
        }
        for session in registry.sessions.values() {
            session.cancel_uncommitted_orphan_cleanup()?;
        }
        drop(registry);
        if refreshed > 0 {
            self.notify_revision();
        }
        Ok(refreshed)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create(
        &self,
        worker: WorkerKind,
        cwd: &str,
        prompt: Option<String>,
        model: Option<String>,
        owner: Option<String>,
        always_approve: bool,
        sandbox: Option<CodexSandboxMode>,
        dangerously_bypass: bool,
        client_kind: Option<ClientKind>,
        client_session_id: Option<String>,
    ) -> Result<SessionState> {
        let cwd = canonical_directory(Path::new(cwd))?;
        ensure_allowed_root(&cwd)?;
        validate_prompt(prompt.as_deref())?;
        validate_model(model.as_deref())?;
        if worker == WorkerKind::Grok && (sandbox.is_some() || dangerously_bypass) {
            bail!("--sandbox and --dangerously-bypass are only supported when --worker codex");
        }

        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
        if !registry.accepting {
            bail!("runtime server is stopping and no longer accepts new sessions");
        }
        let client_lease = if let Some(client_session_id) = client_session_id.as_deref() {
            validate_client_session_id(client_session_id)?;
            let lease = registry
                .clients
                .entry(client_session_id.to_owned())
                .or_insert_with(|| Arc::new(AtomicU64::new(now_millis())))
                .clone();
            lease.store(now_millis(), Ordering::Release);
            Some(lease)
        } else {
            None
        };
        let handle = self.next_handle();
        let provider_session_id = generate_provider_session_id()?;
        if registry
            .provider_sessions
            .contains_key(&provider_session_id)
        {
            bail!("generated a duplicate provider session ID");
        }
        let session = Session::spawn(
            handle.clone(),
            &provider_session_id,
            LaunchConfig {
                worker,
                bin: resolve_worker_bin(worker),
                cwd,
                prompt,
                model,
                owner,
                always_approve,
                sandbox,
                dangerously_bypass,
                client_kind,
                client_session_id,
                client_lease,
                orphan_policy: self.orphan_policy,
            },
            Arc::clone(&self.revision),
        )?;
        let state = session.state()?;
        registry.sessions.insert(handle.clone(), session);
        registry
            .provider_sessions
            .insert(provider_session_id, handle);
        drop(registry);
        self.notify_revision();
        Ok(state)
    }

    pub(crate) fn list(&self) -> Result<Vec<SessionState>> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
        let mut states = registry
            .sessions
            .values()
            .map(|session| session.state())
            .collect::<Result<Vec<_>>>()?;
        states.sort_by_key(|state| state.created_at_ms);
        Ok(states)
    }

    pub(crate) fn list_web(&self) -> Result<Vec<SessionState>> {
        self.list()
    }

    /// Plan ordered WebUI events frames without mutating the connection cursor map.
    ///
    /// - `force_reset`: one `reset=true` ANSI snapshot per session (upgrade / resync).
    /// - Otherwise drain raw PTY bytes only up to each session's **frozen**
    ///   `last_cursor` from this batch so a live producer cannot make the plan chase
    ///   forever.
    /// - Terminal entries may span multiple frames under `max_message_bytes`.
    /// - Cursor commits/drops are returned per frame and must be applied only after
    ///   that frame is successfully sent.
    /// - Session metadata in the JSON omits heavy `screen` / `screen_ansi_base64`;
    ///   reset terminal entries remain the authoritative ANSI snapshot.
    pub(crate) fn plan_web_events(
        &self,
        cursors: &HashMap<String, u64>,
        force_reset: bool,
        max_message_bytes: usize,
    ) -> Result<Vec<WebEventsFramePlan>> {
        let sessions = self.list_web()?;
        let active: HashSet<&str> = sessions
            .iter()
            .map(|state| state.session.as_str())
            .collect();
        let cursor_drops: Vec<String> = cursors
            .keys()
            .filter(|session| !active.contains(session.as_str()))
            .cloned()
            .collect();

        let mut terminal_entries: Vec<(TerminalStreamEntry, Option<(String, u64)>)> = Vec::new();
        for state in &sessions {
            if force_reset || !cursors.contains_key(&state.session) {
                terminal_entries.push((
                    TerminalStreamEntry::reset_snapshot(state),
                    Some((state.session.clone(), state.last_cursor)),
                ));
                continue;
            }

            let mut cursor = cursors
                .get(&state.session)
                .copied()
                .unwrap_or(state.last_cursor);
            // Freeze the exclusive end for this batch so continuous output cannot
            // unbounded-chase the live cursor inside one plan call.
            let freeze_end = state.last_cursor;
            if cursor > freeze_end {
                terminal_entries.push((
                    TerminalStreamEntry::reset_snapshot(state),
                    Some((state.session.clone(), freeze_end)),
                ));
                continue;
            }
            if cursor == freeze_end {
                continue;
            }

            while cursor < freeze_end {
                let limit = usize::try_from(freeze_end - cursor)
                    .unwrap_or(MAX_READ_BYTES)
                    .clamp(1, MAX_READ_BYTES);
                let read = match self.read(&state.session, cursor, limit, 0) {
                    Ok(read) => read,
                    Err(_) => {
                        terminal_entries.push((
                            TerminalStreamEntry::reset_snapshot(state),
                            Some((state.session.clone(), freeze_end)),
                        ));
                        break;
                    }
                };
                if read.truncated {
                    terminal_entries.push((
                        TerminalStreamEntry::reset_snapshot(state),
                        Some((state.session.clone(), freeze_end)),
                    ));
                    break;
                }
                if read.next_cursor == read.cursor {
                    break;
                }
                // Never emit past the freeze point even if the live stream advanced.
                let capped_next = read.next_cursor.min(freeze_end);
                if capped_next <= cursor {
                    break;
                }
                let mut entry = TerminalStreamEntry::delta(&read);
                if capped_next != read.next_cursor {
                    // Re-encode a prefix when the live read overshot the freeze.
                    let raw = BASE64.decode(&read.data_base64).unwrap_or_default();
                    let take = (capped_next - read.cursor) as usize;
                    let take = take.min(raw.len());
                    entry.data_base64 = BASE64.encode(&raw[..take]);
                    entry.next_cursor = read.cursor + take as u64;
                }
                cursor = entry.next_cursor;
                terminal_entries.push((entry, Some((state.session.clone(), cursor))));
            }
        }

        let sessions_view: Vec<SessionState> =
            sessions.into_iter().map(web_events_session_view).collect();
        pack_web_events_frames(
            sessions_view,
            terminal_entries,
            cursor_drops,
            max_message_bytes,
        )
    }

    pub(crate) fn show(&self, handle: &str) -> Result<SessionState> {
        self.get(handle)?.state()
    }

    pub(crate) fn read(
        &self,
        handle: &str,
        cursor: u64,
        limit: usize,
        wait_ms: u64,
    ) -> Result<ReadResult> {
        self.get(handle)?.read(cursor, limit, wait_ms)
    }

    pub(crate) fn send(&self, handle: &str, input: String) -> Result<SessionState> {
        let session = self.get(handle)?;
        session.send(input)?;
        session.state()
    }

    pub(crate) fn write_raw(&self, handle: &str, data: Vec<u8>) -> Result<SessionState> {
        let session = self.get(handle)?;
        session.write_raw(data)?;
        session.state()
    }

    pub(crate) fn resize(&self, handle: &str, cols: u16, rows: u16) -> Result<SessionState> {
        let session = self.get(handle)?;
        session.resize(cols, rows)?;
        session.state()
    }

    pub(crate) fn wait(
        &self,
        handle: &str,
        condition: WaitCondition,
        timeout_ms: u64,
    ) -> Result<WaitResult> {
        self.get(handle)?.wait(condition, timeout_ms)
    }

    pub(crate) fn apply_hook_event(
        &self,
        worker: WorkerKind,
        provider_session_id: &str,
        event: HookEvent,
    ) -> Result<bool> {
        let session = {
            let mut registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            if let Some(handle) = registry.provider_sessions.get(provider_session_id) {
                let Some(session) = registry.sessions.get(handle) else {
                    return Ok(false);
                };
                if session.worker() != worker {
                    return Ok(false);
                }
                Arc::clone(session)
            } else if worker == WorkerKind::Codex && event.kind == HookEventKind::SessionStart {
                let Some(hook_cwd) = event
                    .cwd
                    .as_deref()
                    .and_then(|cwd| canonical_directory(Path::new(cwd)).ok())
                else {
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
                    return Ok(false);
                };
                registry
                    .provider_sessions
                    .insert(provider_session_id.to_owned(), matched_handle.clone());
                registry.external_bound_handles.insert(matched_handle);
                matched_session
            } else {
                return Ok(false);
            }
        };
        session.apply_hook_event(event)?;
        Ok(true)
    }

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
            registry.clients.remove(client_session_id);
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
        let candidates = {
            let registry = self
                .registry
                .lock()
                .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
            let mut sessions = Vec::new();
            for (handle, session) in &registry.sessions {
                if session.claim_orphan_cleanup(now)? {
                    sessions.push((handle.clone(), Arc::clone(session)));
                }
            }
            sessions
        };

        let mut sessions = Vec::new();
        for (handle, session) in candidates {
            let committed = {
                let registry = self
                    .registry
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
                let still_registered = registry
                    .sessions
                    .get(&handle)
                    .is_some_and(|current| Arc::ptr_eq(current, &session));
                still_registered && session.commit_orphan_cleanup(now_millis())?
            };
            if committed {
                sessions.push((handle, session));
            }
        }
        if !sessions.is_empty() {
            // Surface ClientLeaseState::Closing only after the final lease and
            // phase recheck commits cleanup.
            self.notify_revision();
        }
        let outcomes =
            self.run_close_group(sessions, default_group_deadline(), GROUP_CLOSE_WORKERS)?;
        let (result, not_closed) = self.apply_close_outcomes(outcomes)?;
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
    pub(super) fn apply_close_outcomes(
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

    pub(crate) fn active_count(&self) -> u32 {
        self.list()
            .map(|states| {
                states
                    .iter()
                    .filter(|state| phase_is_active(state.phase))
                    .count() as u32
            })
            .unwrap_or(0)
    }

    pub(super) fn get(&self, handle: &str) -> Result<Arc<Session>> {
        self.registry
            .lock()
            .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?
            .sessions
            .get(handle)
            .cloned()
            .with_context(|| format!("session not found: {handle}"))
    }

    fn next_handle(&self) -> String {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("gbt-{:x}-{:x}-{id:x}", std::process::id(), now_millis())
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
