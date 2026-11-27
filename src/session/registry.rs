mod close;
mod events;
mod hooks;

pub(crate) use close::CloseError;

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

#[derive(Default)]
pub(super) struct HookDiagnosticsTracker {
    accepted_count: AtomicU64,
    unmatched_count: AtomicU64,
    last_unmatched: Mutex<Option<(WorkerKind, HookEventKind, u64)>>,
}

pub(crate) struct SessionHost {
    pub(super) registry: Mutex<SessionRegistry>,
    pub(super) next_id: AtomicU64,
    pub(super) orphan_policy: OrphanPolicy,
    pub(super) revision: Arc<HostRevision>,
    pub(super) hook_diagnostics: HookDiagnosticsTracker,
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
            hook_diagnostics: HookDiagnosticsTracker::default(),
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

    /// Refresh every managed session's client lease in response to explicit
    /// interactive WebUI input (`terminal_input`). Passive WebUI subscriptions and
    /// automatic read-only `terminal_resize` viewport syncs must NOT refresh leases,
    /// so disconnected supervisors do not flash online/offline when viewing tabs.
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
