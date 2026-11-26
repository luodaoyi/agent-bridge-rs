mod process;
mod registry;
mod state;
mod web_events;

#[cfg(test)]
pub(crate) mod tests;

#[cfg(windows)]
pub(crate) use process::run_windows_job_child;
pub(crate) use registry::{CloseError, HostRevision, OrphanPolicy, SessionHost};
pub(crate) use web_events::WebEventsFramePlan;

use process::*;
use state::*;
use web_events::{pack_web_events_frames, web_events_session_view};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    ffi::OsString,
    fmt,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
#[cfg(windows)]
use portable_pty::CommandBuilder;
use portable_pty::{MasterPty, PtySize, native_pty_system};

use crate::{
    protocol::{
        ClientKind, ClientLeaseState, CloseGroupResult, CodexSandboxMode, HookActivity, HookEvent,
        HookEventKind, MAX_WRITE_BYTES, ReadResult, SessionPhase, SessionState,
        TerminalStreamEntry, WaitCondition, WaitResult, WebEventsMessage, WorkerKind,
        validate_client_session_id, validate_owner, validate_terminal_size,
    },
    worker::{
        self, HookEffect, blocked_reason, ensure_worker_state_dir_writable, hook_effect,
        phase_from_title, resolve_worker_bin,
    },
};

const INITIAL_COLS: u16 = 120;
const INITIAL_ROWS: u16 = 36;
const SCROLLBACK_ROWS: usize = 5_000;
const MAX_TRANSCRIPT_BYTES: usize = 512 * 1024;
const MAX_READ_BYTES: usize = 64 * 1024;
const WRITER_QUEUE_CAPACITY: usize = 64;
/// Total pending writer bytes budget (≈ four maximum-size writes). Admission is
/// bounded by both entry count and bytes so a full queue always rejects the
/// whole input instead of accepting a partial write.
const WRITER_QUEUE_MAX_BYTES: usize = 4 * MAX_WRITE_BYTES;
const QUIET_IDLE_MILLISECONDS: u64 = 3_000;
const PROCESS_TERMINATE_TIMEOUT_MS: u32 = 5_000;
/// One absolute deadline shared by every session in a group close. The WebUI
/// request timeout (8 s) exceeds this by a small response margin.
const GROUP_CLOSE_TIMEOUT_MS: u64 = 6_000;
/// How long the group close collector may wait for a worker past the shared
/// deadline before declaring the remaining sessions timed out.
const GROUP_CLOSE_RESPONSE_MARGIN_MS: u64 = 1_000;
/// Fixed concurrency bound for group close, independent of group size.
const GROUP_CLOSE_WORKERS: usize = 4;
/// Minimum grace between HUP, TERM, and KILL inside one close deadline so a
/// graceful child is not force-killed prematurely.
const TERMINATION_GRACE_MIN_MS: u64 = 500;
/// Lead time before the close deadline during which the Linux liveness probe
/// runs its stable two-scan verification. Before that window a successful
/// kill(-pgid, 0) answers `Alive` immediately, so escalation does not walk
/// /proc (twice) on every round.
const STABLE_SCOPE_SCAN_LEAD_MS: u64 = 500;
/// Bounded escalation budget used by reader/writer/waiter error edges that
/// cannot block the close path.
const ERROR_ESCALATION_TIMEOUT_MS: u64 = 1_500;
#[cfg(windows)]
const WINDOWS_LAUNCH_HANDSHAKE_TIMEOUT_MS: u32 = 10_000;
/// Bytes of PTY output included in a failed Windows PID handshake error.
#[cfg(windows)]
const WINDOWS_PRE_HANDSHAKE_OUTPUT_MAX: usize = 2 * 1024;
/// How long a closed session handle stays in the idempotent re-close tombstone.
const CLOSED_TOMBSTONE_TTL_MS: u64 = 10 * 60 * 1_000;
const CLOSED_TOMBSTONE_CAP: usize = 1_024;
const PROVIDER_SESSION_UUID_BYTES: usize = 16;
const DEFAULT_CODEX_LEASE_SECONDS: u64 = 120;
const DEFAULT_ORPHAN_GRACE_SECONDS: u64 = 600;
const MIN_CODEX_LEASE_SECONDS: u64 = 30;
const MAX_CODEX_LEASE_SECONDS: u64 = 24 * 60 * 60;
const MIN_ORPHAN_GRACE_SECONDS: u64 = 60;
const MAX_ORPHAN_GRACE_SECONDS: u64 = 7 * 24 * 60 * 60;

struct LaunchConfig {
    worker: WorkerKind,
    bin: OsString,
    cwd: PathBuf,
    prompt: Option<String>,
    model: Option<String>,
    owner: Option<String>,
    always_approve: bool,
    sandbox: Option<CodexSandboxMode>,
    dangerously_bypass: bool,
    client_kind: Option<ClientKind>,
    client_session_id: Option<String>,
    client_lease: Option<Arc<AtomicU64>>,
    orphan_policy: OrphanPolicy,
}

/// One item handed to the writer thread. `budget` is the number of admission
/// bytes this item holds: input enqueues carry their exact payload length,
/// while terminal response frames (never admitted, sent directly) carry zero.
/// The writer releases exactly `budget` bytes after writing, so the byte
/// counter can never go negative for a response that was never counted.
#[derive(Debug, PartialEq)]
struct WriterItem {
    data: Vec<u8>,
    budget: usize,
}

struct Session {
    inner: Mutex<SessionInner>,
    changed: Condvar,
    host_revision: Arc<HostRevision>,
    writer_tx: Mutex<Option<SyncSender<WriterItem>>>,
    /// Raw bytes currently queued to the writer channel (admission budget).
    pending_writer_bytes: AtomicUsize,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    /// Immutable identifier of the owned process scope, captured at spawn: on
    /// Unix the process-group id (the session leader pid from portable-pty's
    /// setsid), on Windows the root child pid. Every close liveness probe and
    /// termination signal addresses the whole scope through this id. It is
    /// deliberately independent of the externally visible
    /// `SessionState::process_id`, which is cleared as soon as the root exits,
    /// so a naturally exited root can never mask a surviving descendant.
    scope_id: u32,
    /// Monotonic escalation level already sent to the owned process scope.
    /// Persists across close attempts so a retry continues from observed state
    /// instead of restarting the HUP/TERM/KILL ladder from the beginning.
    /// This state is Unix-only; Windows targets the Job Object instead.
    #[cfg(unix)]
    termination: Mutex<TerminationLevel>,
    /// Serializes termination escalation across explicit close, orphan reaping,
    /// and I/O-error cleanup for one session.
    close_lock: Mutex<()>,
    /// Job Object containing the gated launcher and every Grok descendant.
    /// The launcher is assigned before Grok is allowed to start, so membership
    /// does not depend on recovering a complete parent chain from snapshots.
    #[cfg(windows)]
    scope_job: Option<WindowsJob>,
    shutdown: AtomicBool,
    cleanup_claimed: AtomicBool,
    cleanup_committed: AtomicBool,
}

/// Outcome of one close attempt. `Timeout` and `Failed` retain process and PTY
/// ownership so a later attempt can continue termination.
#[derive(Debug, Eq, PartialEq)]
enum CloseOutcome {
    /// The owned process scope is verified terminated and the session is
    /// finalized (or was already complete).
    Closed,
    /// The attempt reached its deadline with the process scope still live.
    Timeout,
    /// Termination failed with a non-timeout error.
    Failed(String),
}

type CloseGroupNotClosed = Vec<(String, Arc<Session>)>;

fn default_close_deadline() -> Instant {
    Instant::now() + Duration::from_millis(u64::from(PROCESS_TERMINATE_TIMEOUT_MS))
}

fn default_group_deadline() -> Instant {
    Instant::now() + Duration::from_millis(GROUP_CLOSE_TIMEOUT_MS)
}

struct SessionInner {
    session: String,
    worker: WorkerKind,
    client_kind: Option<ClientKind>,
    owner: Option<String>,
    client_session_id: Option<String>,
    client_lease: Option<Arc<AtomicU64>>,
    orphan_policy: OrphanPolicy,
    phase: SessionPhase,
    phase_changed_at_ms: u64,
    cwd: String,
    model: Option<String>,
    always_approve: bool,
    sandbox: Option<CodexSandboxMode>,
    dangerously_bypass: bool,
    process_id: Option<u32>,
    created_at_ms: u64,
    updated_at_ms: u64,
    exit_code: Option<u32>,
    error: Option<String>,
    title: Option<String>,
    parser: vt100::Parser<TitleCallbacks>,
    chunks: VecDeque<OutputChunk>,
    transcript_bytes: usize,
    next_cursor: u64,
    last_output_at_ms: Option<u64>,
    process_done: bool,
    reader_done: bool,
    hook: HookState,
}

struct OutputChunk {
    start: u64,
    data: Vec<u8>,
}

#[derive(Default)]
struct HookState {
    activity: HookActivity,
    last_event: Option<HookEventKind>,
    last_event_at_ms: Option<u64>,
    tool_name: Option<String>,
    waiting_reason: Option<String>,
    turn_done: bool,
}

#[derive(Default)]
struct TitleCallbacks {
    title: Option<String>,
    title_updated: bool,
    responses: Vec<Vec<u8>>,
}

impl vt100::Callbacks for TitleCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
        self.title_updated = true;
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first_intermediate: Option<u8>,
        second_intermediate: Option<u8>,
        params: &[&[u16]],
        final_character: char,
    ) {
        if first_intermediate.is_some() || second_intermediate.is_some() {
            return;
        }
        let first_param = params.first().and_then(|value| value.first()).copied();
        match (final_character, first_param) {
            ('n', Some(5)) => self.responses.push(b"\x1b[0n".to_vec()),
            ('n', Some(6)) => {
                let (row, column) = screen.cursor_position();
                self.responses
                    .push(format!("\x1b[{};{}R", row + 1, column + 1).into_bytes());
            }
            ('c', None | Some(0)) => self.responses.push(b"\x1b[?1;2c".to_vec()),
            _ => {}
        }
    }
}

impl Session {
    fn worker(&self) -> WorkerKind {
        self.inner
            .lock()
            .map(|inner| inner.worker)
            .unwrap_or(WorkerKind::Grok)
    }

    fn created_at_ms(&self) -> Result<u64> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?
            .created_at_ms)
    }

    fn is_active_codex_matching_cwd(&self, expected_cwd: &Path) -> bool {
        let Ok(inner) = self.inner.lock() else {
            return false;
        };
        inner.worker == WorkerKind::Codex
            && phase_is_active(inner.phase)
            && normalize_platform_path(PathBuf::from(&inner.cwd)) == expected_cwd
    }

    fn has_owner(&self, owner: &str) -> Result<bool> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?
            .owner
            .as_deref()
            == Some(owner))
    }

    fn has_client(&self, client_session_id: &str) -> Result<bool> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?
            .client_session_id
            .as_deref()
            == Some(client_session_id))
    }

    fn client_session_id(&self) -> Result<Option<String>> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?
            .client_session_id
            .clone())
    }

    fn spawn(
        handle: String,
        provider_session_id: &str,
        config: LaunchConfig,
        host_revision: Arc<HostRevision>,
    ) -> Result<Arc<Self>> {
        let state_dir =
            ensure_worker_state_dir_writable(config.worker, &config.cwd, provider_session_id)?;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                cols: INITIAL_COLS,
                rows: INITIAL_ROWS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone the PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take the PTY writer")?;
        #[cfg(not(windows))]
        let command = worker::build_worker_command(
            config.worker,
            &config.bin,
            &config.cwd,
            config.prompt.as_deref(),
            config.model.as_deref(),
            config.always_approve,
            config.sandbox,
            config.dangerously_bypass,
            provider_session_id,
            state_dir.as_deref(),
        );
        #[cfg(windows)]
        let (command, launch_gate, scope_job) =
            build_windows_launcher_command(&config, provider_session_id, state_dir.as_deref())?;
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("failed to start interactive {} worker", config.worker))?;
        drop(pair.slave);
        let launcher_process_id = child
            .process_id()
            .context("the PTY child did not report a process ID")?;
        #[cfg(windows)]
        let initial_process_id = None;
        #[cfg(not(windows))]
        let initial_process_id = Some(launcher_process_id);
        let (writer_tx, writer_rx) = sync_channel(WRITER_QUEUE_CAPACITY);
        let now = now_millis();
        let session = Arc::new(Self {
            inner: Mutex::new(SessionInner {
                session: handle,
                worker: config.worker,
                client_kind: config.client_kind,
                owner: config.owner,
                client_session_id: config.client_session_id,
                client_lease: config.client_lease,
                orphan_policy: config.orphan_policy,
                phase: SessionPhase::Starting,
                phase_changed_at_ms: now,
                cwd: config.cwd.to_string_lossy().into_owned(),
                model: config.model,
                always_approve: config.always_approve,
                sandbox: config.sandbox,
                dangerously_bypass: config.dangerously_bypass,
                process_id: initial_process_id,
                created_at_ms: now,
                updated_at_ms: now,
                exit_code: None,
                error: None,
                title: None,
                parser: vt100::Parser::new_with_callbacks(
                    INITIAL_ROWS,
                    INITIAL_COLS,
                    SCROLLBACK_ROWS,
                    TitleCallbacks::default(),
                ),
                chunks: VecDeque::new(),
                transcript_bytes: 0,
                next_cursor: 0,
                last_output_at_ms: None,
                process_done: false,
                reader_done: false,
                hook: HookState::default(),
            }),
            changed: Condvar::new(),
            host_revision,
            writer_tx: Mutex::new(Some(writer_tx)),
            pending_writer_bytes: AtomicUsize::new(0),
            master: Mutex::new(Some(pair.master)),
            scope_id: launcher_process_id,
            #[cfg(unix)]
            termination: Mutex::new(TerminationLevel::default()),
            close_lock: Mutex::new(()),
            #[cfg(windows)]
            scope_job: Some(scope_job),
            shutdown: AtomicBool::new(false),
            cleanup_claimed: AtomicBool::new(false),
            cleanup_committed: AtomicBool::new(false),
        });

        // Writer first so ConPTY inherit-cursor CSI 6n replies are not lost if
        // the reader observes the query before the writer thread exists.
        spawn_writer(Arc::clone(&session), writer, writer_rx);
        spawn_reader(Arc::clone(&session), reader);
        #[cfg(windows)]
        let child = {
            let mut child = child;
            let launch = child
                .as_raw_handle()
                .context("portable-pty did not expose the Windows launcher handle")
                .and_then(|handle| {
                    session
                        .scope_job
                        .as_ref()
                        .context("Windows sessions own a Job Object")?
                        .assign_process(handle)
                        .context("failed to assign the Windows launcher to its Job Object")?;
                    launch_gate
                        .signal()
                        .context("failed to release the Windows launch gate")?;
                    launch_gate
                        .wait_for_worker_pid(WINDOWS_LAUNCH_HANDSHAKE_TIMEOUT_MS, Some(handle))
                        .context("the Windows launcher did not report its process ID")
                });
            match launch {
                Ok(process_id) => {
                    session.set_process_id(process_id)?;
                    child
                }
                Err(error) => {
                    let output = session.bounded_pre_handshake_output();
                    if let Some(job) = session.scope_job.as_ref() {
                        let _ = job.terminate(TerminationLevel::Kill);
                    }
                    let _ = child.kill();
                    session.close_writer();
                    session.release_master();
                    return Err(if output.is_empty() {
                        error
                    } else {
                        error.context(format!("pre-handshake PTY output: {output}"))
                    });
                }
            }
        };
        spawn_waiter(Arc::clone(&session), child);
        Ok(session)
    }

    fn signal_changed(&self) {
        self.changed.notify_all();
        self.host_revision.bump();
    }

    fn state(&self) -> Result<SessionState> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        Ok(inner.to_state(now_millis(), self.cleanup_claimed.load(Ordering::Acquire)))
    }

    /// Next pure time-based client lease transition for this session, if any.
    /// Returns only a future deadline so waiters do not spin after the transition
    /// has already been observed via a subsequent list/show.
    fn next_lifecycle_deadline_ms(&self, now: u64) -> Result<Option<u64>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        Ok(inner.next_lifecycle_deadline_ms(now, self.cleanup_claimed.load(Ordering::Acquire)))
    }

    fn claim_orphan_cleanup(&self, now: u64) -> Result<bool> {
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
    fn commit_orphan_cleanup(&self, now: u64) -> Result<bool> {
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

    fn cancel_uncommitted_cleanup_for_client(&self, client_session_id: &str) -> Result<bool> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if inner.client_session_id.as_deref() != Some(client_session_id) {
            return Ok(false);
        }
        Ok(self.cancel_uncommitted_cleanup_locked())
    }

    fn cancel_uncommitted_orphan_cleanup(&self) -> Result<bool> {
        let _inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        Ok(self.cancel_uncommitted_cleanup_locked())
    }

    fn cancel_uncommitted_cleanup_locked(&self) -> bool {
        !self.cleanup_committed.load(Ordering::Acquire)
            && self.cleanup_claimed.swap(false, Ordering::AcqRel)
    }

    fn reset_orphan_cleanup(&self) {
        self.cleanup_committed.store(false, Ordering::Release);
        self.cleanup_claimed.store(false, Ordering::Release);
    }

    fn apply_hook_event(&self, event: HookEvent) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if phase_is_terminal(inner.phase) {
            return Ok(());
        }

        if let Some(cwd) = event.cwd.as_deref() {
            let hook_cwd = canonical_directory(Path::new(cwd))?;
            let session_cwd = normalize_platform_path(PathBuf::from(&inner.cwd));
            if hook_cwd != session_cwd {
                bail!("hook working directory does not match the session");
            }
        }

        let now = now_millis();
        let effect = if inner.hook.turn_done
            && !matches!(
                event.kind,
                HookEventKind::SessionStart
                    | HookEventKind::UserPromptSubmit
                    | HookEventKind::Stop
                    | HookEventKind::StopFailure
                    | HookEventKind::SessionEnd
            ) {
            HookEffect::RecordOnly
        } else {
            hook_effect(&event)
        };
        match effect {
            HookEffect::Reset => {
                inner.hook.activity = HookActivity::Unknown;
                inner.hook.tool_name = None;
                inner.hook.waiting_reason = None;
                inner.hook.turn_done = false;
            }
            HookEffect::Working { tool_name } => {
                inner.hook.activity = HookActivity::Working;
                inner.hook.tool_name = tool_name;
                inner.hook.waiting_reason = None;
                if event.kind == HookEventKind::UserPromptSubmit {
                    inner.hook.turn_done = false;
                }
                set_phase(&mut inner, SessionPhase::Running, now);
            }
            HookEffect::Waiting { tool_name, reason } => {
                inner.hook.activity = HookActivity::Waiting;
                inner.hook.tool_name = tool_name;
                inner.hook.waiting_reason = Some(reason);
                set_phase(&mut inner, SessionPhase::Running, now);
            }
            HookEffect::Done => {
                inner.hook.activity = HookActivity::Done;
                inner.hook.tool_name = None;
                inner.hook.waiting_reason = None;
                inner.hook.turn_done = true;
                set_phase(&mut inner, SessionPhase::Idle, now);
            }
            HookEffect::RecordOnly => {}
        }

        inner.hook.last_event = Some(event.kind);
        inner.hook.last_event_at_ms = Some(now);
        inner.updated_at_ms = now;
        drop(inner);
        self.signal_changed();
        Ok(())
    }

    fn read(&self, cursor: u64, limit: usize, wait_ms: u64) -> Result<ReadResult> {
        let limit = limit.clamp(1, MAX_READ_BYTES);
        let deadline = Instant::now() + Duration::from_millis(wait_ms.min(300_000));
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if cursor > inner.next_cursor {
            bail!(
                "cursor {cursor} is beyond the latest cursor {}",
                inner.next_cursor
            );
        }
        while cursor == inner.next_cursor && phase_is_active(inner.phase) && wait_ms > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let waited = self
                .changed
                .wait_timeout(inner, remaining)
                .map_err(|_| anyhow::anyhow!("session wait lock was poisoned"))?;
            inner = waited.0;
            if waited.1.timed_out() {
                break;
            }
        }

        let oldest_cursor = inner
            .chunks
            .front()
            .map(|chunk| chunk.start)
            .unwrap_or(inner.next_cursor);
        let actual_cursor = cursor.max(oldest_cursor);
        let mut output = Vec::with_capacity(limit);
        for chunk in &inner.chunks {
            let end = chunk.start + chunk.data.len() as u64;
            if end <= actual_cursor {
                continue;
            }
            let offset = actual_cursor.saturating_sub(chunk.start) as usize;
            let available = &chunk.data[offset.min(chunk.data.len())..];
            let take = available.len().min(limit - output.len());
            output.extend_from_slice(&available[..take]);
            if output.len() == limit {
                break;
            }
        }
        let next_cursor = actual_cursor + output.len() as u64;
        Ok(ReadResult {
            session: inner.session.clone(),
            cursor: actual_cursor,
            next_cursor,
            data_base64: BASE64.encode(&output),
            plain_text: None,
            screen: Some(inner.parser.screen().contents()),
            truncated: cursor < oldest_cursor,
            eof: phase_is_terminal(inner.phase),
        })
    }

    fn send(&self, input: String) -> Result<()> {
        if input.is_empty() {
            bail!("input must not be empty");
        }
        // A lone Ctrl-C interrupts the current turn; enqueue it without
        // starting a new one, keeping the bytes sent to the PTY unchanged.
        let starts_turn = !(input.len() == 1 && input.as_bytes()[0] == 0x03);
        let data = if input.len() == 1 && input.as_bytes()[0].is_ascii_control() {
            input.into_bytes()
        } else {
            let mut data = Vec::with_capacity(input.len() + 13);
            data.extend_from_slice(b"\x1b[200~");
            data.extend_from_slice(input.as_bytes());
            data.extend_from_slice(b"\x1b[201~\r");
            data
        };
        self.enqueue_input(data, starts_turn)
    }

    fn write_raw(&self, data: Vec<u8>) -> Result<()> {
        if data.is_empty() {
            bail!("terminal data must not be empty");
        }
        if data.len() > MAX_WRITE_BYTES {
            bail!("terminal data exceeds the 64 KiB limit");
        }
        let starts_turn = raw_input_starts_turn(&data);
        self.enqueue_input(data, starts_turn)
    }

    fn enqueue_input(&self, data: Vec<u8>, starts_turn: bool) -> Result<()> {
        if self.shutdown.load(Ordering::Acquire) {
            bail!("session has already stopped");
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if self.cleanup_claimed.load(Ordering::Acquire)
            || self.cleanup_committed.load(Ordering::Acquire)
        {
            bail!("session cleanup has started");
        }
        if inner.process_done || phase_is_terminal(inner.phase) || inner.error.is_some() {
            bail!("session is not writable");
        }
        let writer_guard = self
            .writer_tx
            .lock()
            .map_err(|_| anyhow::anyhow!("session input lock was poisoned"))?;
        let Some(writer) = writer_guard.as_ref() else {
            bail!("session input channel is closed");
        };
        // Byte budget is checked before any byte is queued; queue-full rejection
        // is always whole-input (never a partial write).
        let data_len = data.len();
        if self
            .pending_writer_bytes
            .load(Ordering::Acquire)
            .saturating_add(data_len)
            > WRITER_QUEUE_MAX_BYTES
        {
            bail!("session input queue is full");
        }
        // Count the bytes BEFORE the item becomes visible to the writer thread.
        // The writer only ever subtracts for items it received, and an item only
        // becomes receivable after a successful try_send that follows this
        // increment, so the counter can never underflow and the writer can never
        // subtract before the matching add. On Full/Disconnected the increment
        // is rolled back under the same lock. Ordering: Release on the add
        // pairs with the writer's Acquire release of the same item, but the
        // counter's ordering is not what makes admission correct — the channel
        // itself synchronizes the item — so the plain Acquire on the release
        // path is the minimal sufficient read side (Relaxed would also be
        // sound); no AcqRel is required for count correctness.
        self.pending_writer_bytes
            .fetch_add(data_len, Ordering::Release);
        match writer.try_send(WriterItem {
            data,
            budget: data_len,
        }) {
            Ok(()) => {
                let now = now_millis();
                if starts_turn {
                    set_phase(&mut inner, SessionPhase::Running, now);
                    inner.hook.activity = HookActivity::Working;
                    inner.hook.tool_name = None;
                    inner.hook.waiting_reason = None;
                    inner.hook.turn_done = false;
                }
                inner.updated_at_ms = now;
                drop(writer_guard);
                drop(inner);
                self.signal_changed();
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.pending_writer_bytes
                    .fetch_sub(data_len, Ordering::Acquire);
                bail!("session input queue is full");
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending_writer_bytes
                    .fetch_sub(data_len, Ordering::Acquire);
                bail!("session input channel is closed");
            }
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        validate_terminal_size(cols, rows)?;
        if self.shutdown.load(Ordering::Acquire) {
            bail!("session has already stopped");
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if self.cleanup_claimed.load(Ordering::Acquire)
            || self.cleanup_committed.load(Ordering::Acquire)
        {
            bail!("session cleanup has started");
        }
        if inner.process_done || phase_is_terminal(inner.phase) || inner.error.is_some() {
            bail!("session is not resizable");
        }
        let master_guard = self
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY master lock was poisoned"))?;
        let Some(master) = master_guard.as_ref() else {
            bail!("PTY master is closed");
        };
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")?;
        inner.parser.screen_mut().set_size(rows, cols);
        inner.updated_at_ms = now_millis();
        drop(master_guard);
        drop(inner);
        self.signal_changed();
        Ok(())
    }

    fn wait(&self, condition: WaitCondition, timeout_ms: u64) -> Result<WaitResult> {
        let timeout_ms = timeout_ms.clamp(1, 7_200_000);
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        loop {
            if condition == WaitCondition::TuiIdle {
                let screen = inner.parser.screen().contents();
                if let Some(reason) = blocked_reason(inner.worker, &screen) {
                    return Ok(inner.wait_result(condition, false, false, Some(reason)));
                }
                if inner.hook.activity == HookActivity::Waiting {
                    let reason = inner
                        .hook
                        .waiting_reason
                        .as_deref()
                        .unwrap_or("agent-hook-waiting");
                    return Ok(inner.wait_result(condition, false, false, Some(reason)));
                }
            }
            if wait_satisfied(&mut inner, condition) {
                return Ok(inner.wait_result(condition, true, false, None));
            }
            if condition == WaitCondition::TuiIdle
                && (phase_is_terminal(inner.phase) || inner.process_done)
            {
                return Ok(inner.wait_result(condition, false, false, None));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(inner.wait_result(condition, false, true, None));
            }
            let poll = remaining.min(Duration::from_millis(250));
            let waited = self
                .changed
                .wait_timeout(inner, poll)
                .map_err(|_| anyhow::anyhow!("session wait lock was poisoned"))?;
            inner = waited.0;
        }
    }

    /// One bounded close attempt. Marks the session as shutdown-requested,
    /// escalates termination of the owned process scope within `deadline`, and
    /// finalizes once termination is verified. On `Timeout`/`Failed` the
    /// process and PTY ownership is retained so a later attempt can continue
    /// from observed state (see `TerminationLevel`).
    fn close_attempt(&self, deadline: Instant) -> CloseOutcome {
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

    fn try_acquire_close_lock(
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
    fn process_is_done_or_terminal(&self) -> bool {
        matches!(self.probe_scope(true), ScopeAlive::Gone)
    }

    /// Probe the owned process scope. Unix uses the immutable process-group id;
    /// Windows queries the Job Object that contains the gated launcher and all
    /// of its descendants. `stable_verification` is passed through to the Unix
    /// probe so the escalation loop can skip the expensive all-zombie scan
    /// until its final confirmation window.
    fn probe_scope(&self, stable_verification: bool) -> ScopeAlive {
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
    fn scope_state(&self, stable_verification: bool) -> ScopeState {
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
    fn record_verified_process_done(&self) {
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
    fn escalate_until_done(&self, deadline: Instant) -> EscalationResult {
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
    fn force_terminate_scope(&self, deadline: Instant) -> EscalationResult {
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
    fn escalate_to(&self, target: TerminationLevel) -> std::io::Result<()> {
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
    fn finalize_now(&self) -> CloseOutcome {
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

    fn append_output(&self, data: Vec<u8>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = now_millis();
        let start = inner.next_cursor;
        inner.next_cursor = inner.next_cursor.saturating_add(data.len() as u64);
        inner.transcript_bytes = inner.transcript_bytes.saturating_add(data.len());
        inner.parser.process(&data);
        inner.title = inner.parser.callbacks().title.clone();
        let callbacks = inner.parser.callbacks_mut();
        let title_updated = std::mem::take(&mut callbacks.title_updated);
        let responses = std::mem::take(&mut callbacks.responses);
        let phase = phase_after_output(
            inner.worker,
            inner.phase,
            inner.title.as_deref(),
            title_updated,
            inner.hook.activity,
            inner.process_done,
            inner.error.is_some(),
            self.shutdown.load(Ordering::Acquire),
        );
        set_phase(&mut inner, phase, now);
        inner.last_output_at_ms = Some(now);
        inner.updated_at_ms = now;
        inner.chunks.push_back(OutputChunk { start, data });
        while inner.transcript_bytes > MAX_TRANSCRIPT_BYTES {
            let Some(removed) = inner.chunks.pop_front() else {
                break;
            };
            inner.transcript_bytes = inner.transcript_bytes.saturating_sub(removed.data.len());
        }
        drop(inner);
        for response in responses {
            self.queue_terminal_response(response);
        }
        self.signal_changed();
    }

    fn queue_terminal_response(&self, response: Vec<u8>) {
        // Terminal response frames bypass admission: they carry budget 0 so the
        // writer's release never underflows the byte counter for bytes that
        // were never counted.
        let result = self.writer_tx.lock().ok().and_then(|writer| {
            writer.as_ref().map(|writer| {
                writer.try_send(WriterItem {
                    data: response,
                    budget: 0,
                })
            })
        });
        match result {
            Some(Ok(())) => {}
            Some(Err(TrySendError::Full(_))) => {
                // Queue full is transient backpressure, not a fatal writer
                // failure: record the error for observers but never escalate
                // the owned process scope, matching enqueue_input semantics.
                self.mark_writer_error_with_escalation(
                    "terminal response queue is full".to_owned(),
                    false,
                );
            }
            Some(Err(TrySendError::Disconnected(_))) | None => {
                if !self.shutdown.load(Ordering::Acquire) {
                    self.mark_writer_error("terminal response channel is closed".to_owned());
                }
            }
        }
    }

    fn mark_reader_done(&self) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            inner.reader_done = true;
            inner.updated_at_ms = now_millis();
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        self.finish_transition(finalized);
    }

    fn mark_reader_error(&self, message: String) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            inner.reader_done = true;
            record_error(&mut inner, message);
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        self.finish_transition(finalized);
        // The stream is gone; terminate the owned process scope within a bounded
        // escalation so the session converges instead of waiting forever.
        if !self.process_is_done_or_terminal() {
            self.run_error_edge_escalation();
        }
    }

    fn mark_writer_error(&self, message: String) {
        self.mark_writer_error_with_escalation(message, true);
    }

    /// Record a writer failure and optionally escalate the owned process scope.
    /// `escalate` must be false for recoverable backpressure (a full queue):
    /// the stream is still usable and the scope must survive; escalation is
    /// reserved for unrecoverable failures like a closed channel.
    fn mark_writer_error_with_escalation(&self, message: String, escalate: bool) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            record_error(&mut inner, message);
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        self.finish_transition(finalized);
        if escalate && !self.process_is_done_or_terminal() {
            self.run_error_edge_escalation();
        }
    }

    fn mark_wait_error(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            record_error(&mut inner, message);
        }
        self.signal_changed();
        // The waiter can no longer observe the root process, so verify the
        // whole owned scope directly (Unix process-group probe or Windows Job
        // Object). Never set process_done
        // without verification: an unverifiable failure keeps the real state
        // and ownership so a later close can retry termination.
        self.run_error_edge_escalation();
    }

    /// Bounded best-effort escalation used by reader/writer/waiter error edges.
    /// Converges finalization only when the scope is verified gone; timeout and
    /// failure keep ownership and state so a later close can retry.
    fn run_error_edge_escalation(&self) {
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

    fn mark_exit(&self, exit_code: u32) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            if !inner.process_done {
                inner.process_done = true;
                inner.exit_code = Some(exit_code);
                // The root exited: the externally visible process id is stale
                // now (baseline semantics; a natural exit must never expose a
                // dead pid). The immutable Session::scope_id still identifies
                // the whole owned scope for close liveness and termination.
                inner.process_id = None;
            }
            inner.updated_at_ms = now_millis();
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        #[cfg(windows)]
        if !finalized {
            // On Windows ConPTY, once the child exits naturally, release the
            // master handle so ClosePseudoConsole lets the reader observe EOF
            // after draining any buffered output.
            self.close_writer();
            self.release_master();
        }
        self.finish_transition(finalized);
    }

    fn record_secondary_error(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            record_error(&mut inner, message);
        }
        self.signal_changed();
    }

    fn close_writer(&self) {
        if let Ok(mut writer) = self.writer_tx.lock() {
            writer.take();
        }
    }

    #[cfg(windows)]
    fn set_process_id(&self, process_id: u32) -> Result<()> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?
            .process_id = Some(process_id);
        Ok(())
    }

    #[cfg(windows)]
    fn bounded_pre_handshake_output(&self) -> String {
        let Ok(inner) = self.inner.lock() else {
            return String::new();
        };
        let mut bytes = Vec::new();
        for chunk in &inner.chunks {
            bytes.extend_from_slice(&chunk.data);
            if bytes.len() >= WINDOWS_PRE_HANDSHAKE_OUTPUT_MAX {
                break;
            }
        }
        let end = bytes.len().min(WINDOWS_PRE_HANDSHAKE_OUTPUT_MAX);
        bytes[..end].escape_ascii().to_string()
    }

    fn release_master(&self) {
        if let Ok(mut master) = self.master.lock() {
            master.take();
        }
    }

    fn finish_transition(&self, finalized: bool) {
        if finalized {
            self.close_writer();
            self.release_master();
        }
        self.signal_changed();
    }

    #[cfg(test)]
    fn test_hold_close_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.close_lock.lock().unwrap()
    }

    #[cfg(test)]
    fn test_poison_close_lock(&self) {
        let _guard = self.close_lock.lock().unwrap();
        panic!("test poisons the session close lock");
    }
}

impl SessionInner {
    fn to_state(&self, now: u64, cleanup_claimed: bool) -> SessionState {
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

    fn next_lifecycle_deadline_ms(&self, now: u64, cleanup_claimed: bool) -> Option<u64> {
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

    fn client_lifecycle(
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

    fn orphan_cleanup_due(&self, now: u64) -> bool {
        let (_, _, _, auto_close_at) = self.client_lifecycle(now, false);
        auto_close_at.is_some_and(|deadline| now >= deadline)
    }

    fn wait_result(
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

fn spawn_reader(session: Arc<Session>, mut reader: Box<dyn Read + Send>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    session.mark_reader_done();
                    return;
                }
                Ok(read) => session.append_output(buffer[..read].to_vec()),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
                    ) =>
                {
                    session.mark_reader_done();
                    return;
                }
                Err(error) => {
                    session.mark_reader_error(format!("failed to read Grok output: {error}"));
                    return;
                }
            }
        }
    });
}

fn spawn_writer(
    session: Arc<Session>,
    mut writer: Box<dyn Write + Send>,
    writer_rx: std::sync::mpsc::Receiver<WriterItem>,
) {
    thread::spawn(move || {
        while let Ok(item) = writer_rx.recv() {
            // Only the item's admitted budget is released: inputs release their
            // payload bytes, terminal responses release nothing, so the counter
            // tracks exactly the queued input bytes and can never underflow.
            if let Err(error) = writer.write_all(&item.data).and_then(|()| writer.flush()) {
                // The item left the queue, so release its budget too. Any items
                // still queued when the channel later closes are dropped without
                // release, but the session is terminating at that point and no
                // further admission happens.
                session
                    .pending_writer_bytes
                    .fetch_sub(item.budget, Ordering::Acquire);
                session.mark_writer_error(format!("failed to write Grok input: {error}"));
                return;
            }
            session
                .pending_writer_bytes
                .fetch_sub(item.budget, Ordering::Acquire);
        }
    });
}

fn spawn_waiter(session: Arc<Session>, mut child: Box<dyn portable_pty::Child + Send + Sync>) {
    thread::spawn(move || match child.wait() {
        Ok(status) => session.mark_exit(status.exit_code()),
        Err(error) => session.mark_wait_error(format!("failed while waiting for Grok: {error}")),
    });
}
