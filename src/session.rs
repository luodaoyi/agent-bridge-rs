mod io;
mod process;
mod registry;
mod snapshot;
mod state;
mod termination;
mod web_events;

#[cfg(test)]
pub(crate) mod tests;

#[cfg(windows)]
pub(crate) use process::run_windows_job_child;
pub(crate) use registry::{CloseError, HostRevision, OrphanPolicy, SessionHost};
pub(crate) use web_events::WebEventsFramePlan;

use io::{spawn_reader, spawn_waiter, spawn_writer};
use process::*;
use state::*;
use web_events::pack_web_events_frames;

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
