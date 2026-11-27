use std::fmt::{self, Display};
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

mod validation;

#[allow(unused_imports)]
pub(crate) use validation::validate_provider_session_id;
pub(crate) use validation::{
    decode_request, decode_response, decode_write_data, encode_frame, validate_client_session_id,
    validate_hook_request, validate_owner, validate_session_handle, validate_terminal_size,
};

#[cfg(test)]
include!("protocol/tests.rs");

pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_CLIENT_SESSION_ID_BYTES: usize = 128;
const MAX_OWNER_BYTES: usize = 128;
const PROVIDER_SESSION_ID_BYTES: usize = 36;
const MAX_HOOK_CWD_BYTES: usize = 4096;
const MAX_HOOK_SHORT_TEXT_BYTES: usize = 128;
const MAX_HOOK_MESSAGE_BYTES: usize = 1024;
const MAX_READ_LIMIT: u32 = 65_536;
const MIN_TERMINAL_COLS: u16 = 20;
const MAX_TERMINAL_COLS: u16 = 500;
const MIN_TERMINAL_ROWS: u16 = 5;
const MAX_TERMINAL_ROWS: u16 = 200;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkerKind {
    #[default]
    Grok,
    Codex,
}

impl WorkerKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::Codex => "codex",
        }
    }
}

impl Display for WorkerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WorkerKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "grok" => Ok(Self::Grok),
            "codex" => Ok(Self::Codex),
            other => bail!("unsupported worker: {other}; expected 'grok' or 'codex'"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClientKind {
    Codex,
    Agy,
    #[default]
    Unknown,
}

impl ClientKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Agy => "agy",
            Self::Unknown => "unknown",
        }
    }
}

impl Display for ClientKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ClientKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "agy" | "antigravity" => Ok(Self::Agy),
            "unknown" => Ok(Self::Unknown),
            other => {
                bail!("unsupported client kind: {other}; expected 'codex', 'agy', or 'unknown'")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CodexSandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl CodexSandboxMode {
    pub(crate) const fn as_cli_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

impl Display for CodexSandboxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_cli_arg())
    }
}

impl FromStr for CodexSandboxMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "danger-full-access" => Ok(Self::DangerFullAccess),
            other => bail!(
                "unsupported sandbox mode: {other}; expected 'read-only', 'workspace-write', or 'danger-full-access'"
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestEnvelope {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_kind: Option<ClientKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_session_id: Option<String>,
    pub(crate) request: Request,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "method",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Request {
    ServerStatus,
    ServerStop,
    Heartbeat,
    #[serde(alias = "close_codex")]
    CloseClient,
    Create {
        #[serde(default)]
        worker: WorkerKind,
        cwd: String,
        prompt: Option<String>,
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        #[serde(default)]
        always_approve: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sandbox: Option<CodexSandboxMode>,
        #[serde(default)]
        dangerously_bypass: bool,
    },
    List,
    Show {
        session: String,
    },
    Read {
        session: String,
        cursor: Option<u64>,
        limit: Option<u32>,
        wait_ms: Option<u64>,
    },
    Send {
        session: String,
        input: String,
    },
    Write {
        session: String,
        data_base64: String,
    },
    Resize {
        session: String,
        cols: u16,
        rows: u16,
    },
    Wait {
        session: String,
        for_condition: WaitCondition,
        timeout_ms: Option<u64>,
    },
    Close {
        session: String,
    },
    HookEvent {
        #[serde(default)]
        worker: WorkerKind,
        provider_session_id: String,
        event: HookEvent,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HookEventKind {
    SessionStart,
    UserPromptSubmit,
    Stop,
    StopFailure,
    SessionEnd,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,
    PermissionRequest,
    Interrupt,
    Notification,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HookActivity {
    #[default]
    Unknown,
    Working,
    Waiting,
    Done,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookEvent {
    pub(crate) kind: HookEventKind,
    pub(crate) cwd: Option<String>,
    pub(crate) tool_name: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) notification_type: Option<String>,
    pub(crate) level: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitCondition {
    TuiIdle,
    Exit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponseEnvelope {
    pub(crate) id: String,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<ResponseResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<ResponseError>,
}

impl ResponseEnvelope {
    pub(crate) fn success(id: impl Into<String>, result: ResponseResult) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn failure(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(ResponseError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ResponseResult {
    ServerInfo(ServerInfo),
    Session(Box<SessionState>),
    Sessions { sessions: Vec<SessionState> },
    Read(ReadResult),
    Wait(WaitResult),
    CloseGroup(CloseGroupResult),
    Accepted { accepted: bool },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CloseGroupResult {
    pub(crate) matched: usize,
    pub(crate) closed: usize,
    /// Session handles that reached the group close deadline while their owned
    /// process scope was still live. Each entry is `"<handle>: <reason>"`.
    pub(crate) timed_out: Vec<String>,
    /// Session handles that failed with a non-timeout error. Each entry is
    /// `"<handle>: <reason>"`.
    pub(crate) failures: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponseError {
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookDiagnostics {
    pub(crate) accepted_count: u64,
    pub(crate) unmatched_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_unmatched_worker: Option<WorkerKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_unmatched_event: Option<HookEventKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_unmatched_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerInfo {
    pub(crate) version: String,
    pub(crate) process_id: u32,
    pub(crate) started_at_ms: u64,
    pub(crate) active_sessions: u32,
    pub(crate) web_url: Option<String>,
    pub(crate) stopping: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hook_diagnostics: Option<HookDiagnostics>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionPhase {
    Starting,
    Running,
    Idle,
    Exited,
    Failed,
    Stopped,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClientLeaseState {
    #[default]
    Unmanaged,
    Connected,
    Disconnected,
    Orphaned,
    Closing,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionState {
    pub(crate) session: String,
    #[serde(default)]
    pub(crate) worker: WorkerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_kind: Option<ClientKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_session_id: Option<String>,
    #[serde(default)]
    pub(crate) client_state: ClientLeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_lease_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) orphan_grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_last_seen_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) orphaned_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_close_at_ms: Option<u64>,
    pub(crate) phase: SessionPhase,
    pub(crate) title: Option<String>,
    pub(crate) cwd: String,
    pub(crate) model: Option<String>,
    pub(crate) always_approve: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox: Option<CodexSandboxMode>,
    #[serde(default)]
    pub(crate) dangerously_bypass: bool,
    pub(crate) process_id: Option<u32>,
    pub(crate) screen: Option<String>,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) screen_ansi_base64: String,
    pub(crate) last_cursor: u64,
    pub(crate) last_output_at_ms: Option<u64>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) exit_code: Option<u32>,
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) activity: HookActivity,
    pub(crate) hook_event: Option<HookEventKind>,
    pub(crate) hook_at_ms: Option<u64>,
    pub(crate) tool_name: Option<String>,
    pub(crate) waiting_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadResult {
    pub(crate) session: String,
    /// Inclusive byte offset of the first byte returned in `data_base64`.
    pub(crate) cursor: u64,
    /// Exclusive byte offset after the returned bytes.
    pub(crate) next_cursor: u64,
    pub(crate) data_base64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plain_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) screen: Option<String>,
    pub(crate) truncated: bool,
    pub(crate) eof: bool,
}

/// Read-only WebUI `/api/events` stream frame.
/// Wire contract: `{ type: "sessions", sessions: [...], terminals: [...] }`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebEventsMessage {
    #[serde(rename = "type")]
    pub(crate) message_type: String,
    pub(crate) sessions: Vec<SessionState>,
    pub(crate) terminals: Vec<TerminalStreamEntry>,
}

impl WebEventsMessage {
    pub(crate) fn sessions(
        sessions: Vec<SessionState>,
        terminals: Vec<TerminalStreamEntry>,
    ) -> Self {
        Self {
            message_type: "sessions".to_owned(),
            sessions,
            terminals,
        }
    }
}

/// One terminal payload inside a `WebEventsMessage`.
/// `reset=true` carries a full ANSI snapshot; `reset=false` is raw PTY delta.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalStreamEntry {
    pub(crate) session: String,
    pub(crate) reset: bool,
    pub(crate) cursor: u64,
    pub(crate) next_cursor: u64,
    pub(crate) data_base64: String,
}

impl TerminalStreamEntry {
    pub(crate) fn reset_snapshot(state: &SessionState) -> Self {
        Self {
            session: state.session.clone(),
            reset: true,
            cursor: 0,
            next_cursor: state.last_cursor,
            data_base64: state.screen_ansi_base64.clone(),
        }
    }

    pub(crate) fn delta(read: &ReadResult) -> Self {
        Self {
            session: read.session.clone(),
            reset: false,
            cursor: read.cursor,
            next_cursor: read.next_cursor,
            data_base64: read.data_base64.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitResult {
    pub(crate) session: String,
    pub(crate) condition: WaitCondition,
    pub(crate) satisfied: bool,
    pub(crate) timed_out: bool,
    pub(crate) phase: SessionPhase,
    pub(crate) exit_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blocked_reason: Option<String>,
}
