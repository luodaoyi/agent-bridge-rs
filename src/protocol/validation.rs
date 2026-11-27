use std::fmt::Display;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Serialize, de::DeserializeOwned};

use super::{
    HookEvent, MAX_CLIENT_SESSION_ID_BYTES, MAX_FRAME_BYTES, MAX_HOOK_CWD_BYTES,
    MAX_HOOK_MESSAGE_BYTES, MAX_HOOK_SHORT_TEXT_BYTES, MAX_IDENTIFIER_BYTES, MAX_OWNER_BYTES,
    MAX_READ_LIMIT, MAX_TERMINAL_COLS, MAX_TERMINAL_ROWS, MAX_WRITE_BYTES, MIN_TERMINAL_COLS,
    MIN_TERMINAL_ROWS, PROVIDER_SESSION_ID_BYTES, Request, RequestEnvelope, ResponseEnvelope,
    ResponseResult, SessionState, WaitCondition, WorkerKind,
};

pub(crate) fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(value).context("failed to encode protocol frame")?;
    if frame.len() + 1 > MAX_FRAME_BYTES {
        bail!("protocol frame exceeds the 1 MiB limit");
    }
    frame.push(b'\n');
    Ok(frame)
}

pub(crate) fn decode_request(frame: &[u8]) -> Result<RequestEnvelope> {
    let envelope: RequestEnvelope = decode_frame(frame)?;
    validate_identifier(&envelope.id, "request id")?;
    if let Some(client_session_id) = envelope.client_session_id.as_deref() {
        validate_client_session_id(client_session_id)?;
    }
    validate_request(&envelope.request)?;
    Ok(envelope)
}

pub(crate) fn decode_response(frame: &[u8]) -> Result<ResponseEnvelope> {
    let envelope: ResponseEnvelope = decode_frame(frame)?;
    validate_identifier(&envelope.id, "response id")?;
    validate_response(&envelope)?;
    Ok(envelope)
}

pub(crate) fn decode_write_data(data_base64: &str) -> Result<Vec<u8>> {
    let data = BASE64
        .decode(data_base64)
        .context("terminal data_base64 is invalid")?;
    if data.is_empty() {
        bail!("terminal data must not be empty");
    }
    if data.len() > MAX_WRITE_BYTES {
        bail!("terminal data exceeds the 64 KiB limit");
    }
    Ok(data)
}

pub(crate) fn validate_terminal_size(cols: u16, rows: u16) -> Result<()> {
    if !(MIN_TERMINAL_COLS..=MAX_TERMINAL_COLS).contains(&cols) {
        bail!("terminal cols must be between {MIN_TERMINAL_COLS} and {MAX_TERMINAL_COLS}");
    }
    if !(MIN_TERMINAL_ROWS..=MAX_TERMINAL_ROWS).contains(&rows) {
        bail!("terminal rows must be between {MIN_TERMINAL_ROWS} and {MAX_TERMINAL_ROWS}");
    }
    Ok(())
}

fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T> {
    let payload = single_frame_payload(frame)?;
    serde_json::from_slice(payload).context("invalid JSON protocol frame")
}

fn single_frame_payload(frame: &[u8]) -> Result<&[u8]> {
    if frame.is_empty() {
        bail!("protocol frame is empty");
    }
    if frame.len() > MAX_FRAME_BYTES {
        bail!("protocol frame exceeds the 1 MiB limit");
    }

    let payload = if let Some(without_lf) = frame.strip_suffix(b"\n") {
        without_lf.strip_suffix(b"\r").unwrap_or(without_lf)
    } else {
        frame
    };
    if payload.is_empty() {
        bail!("protocol frame is empty");
    }
    if payload.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
        bail!("protocol input must contain exactly one NDJSON frame");
    }
    Ok(payload)
}

fn validate_request(request: &Request) -> Result<()> {
    match request {
        Request::ServerStatus
        | Request::ServerStop
        | Request::Heartbeat
        | Request::CloseClient
        | Request::List => Ok(()),
        Request::Create {
            worker,
            cwd,
            model,
            owner,
            sandbox,
            dangerously_bypass,
            ..
        } => {
            if cwd.trim().is_empty() {
                bail!("cwd must not be empty");
            }
            if model
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                bail!("model must not be empty when provided");
            }
            if let Some(owner) = owner.as_deref() {
                validate_owner(owner)?;
            }
            if *worker == WorkerKind::Grok && (sandbox.is_some() || *dangerously_bypass) {
                bail!("--sandbox and --dangerously-bypass are only supported when --worker codex");
            }
            Ok(())
        }
        Request::Show { session }
        | Request::Read { session, .. }
        | Request::Send { session, .. }
        | Request::Write { session, .. }
        | Request::Resize { session, .. }
        | Request::Wait { session, .. }
        | Request::Close { session } => {
            validate_identifier(session, "session handle")?;
            if let Request::Read {
                limit: Some(limit), ..
            } = request
                && (*limit == 0 || *limit > MAX_READ_LIMIT)
            {
                bail!("read limit must be between 1 and {MAX_READ_LIMIT}");
            }
            if let Request::Send { input, .. } = request
                && input.is_empty()
            {
                bail!("terminal input must not be empty");
            }
            if let Request::Write { data_base64, .. } = request {
                decode_write_data(data_base64)?;
            }
            if let Request::Resize { cols, rows, .. } = request {
                validate_terminal_size(*cols, *rows)?;
            }
            Ok(())
        }
        Request::HookEvent {
            provider_session_id,
            event,
            ..
        } => {
            validate_provider_session_id(provider_session_id)?;
            validate_hook_event(event)
        }
    }
}

fn validate_response(envelope: &ResponseEnvelope) -> Result<()> {
    match (envelope.ok, &envelope.result, &envelope.error) {
        (true, Some(result), None) => validate_response_result(result),
        (false, None, Some(error)) => {
            validate_identifier(&error.code, "error code")?;
            if error.message.trim().is_empty() {
                bail!("response error message must not be empty");
            }
            Ok(())
        }
        (true, _, _) => bail!("successful response must contain result and no error"),
        (false, _, _) => bail!("failed response must contain error and no result"),
    }
}

fn validate_response_result(result: &ResponseResult) -> Result<()> {
    match result {
        ResponseResult::ServerInfo(_)
        | ResponseResult::CloseGroup(_)
        | ResponseResult::Accepted { .. } => Ok(()),
        ResponseResult::Session(session) => validate_session_state(session),
        ResponseResult::Sessions { sessions } => {
            for session in sessions {
                validate_session_state(session)?;
            }
            Ok(())
        }
        ResponseResult::Read(read) => {
            validate_identifier(&read.session, "session handle")?;
            let bytes = BASE64
                .decode(&read.data_base64)
                .context("read data_base64 is invalid")?;
            let expected_next_cursor = read
                .cursor
                .checked_add(bytes.len() as u64)
                .context("read cursor overflowed")?;
            if read.next_cursor != expected_next_cursor {
                bail!("read next_cursor must equal cursor plus the returned byte count");
            }
            Ok(())
        }
        ResponseResult::Wait(wait) => {
            validate_identifier(&wait.session, "session handle")?;
            if wait.satisfied && wait.timed_out {
                bail!("wait result cannot be satisfied and timed out");
            }
            if let Some(reason) = wait.blocked_reason.as_deref() {
                validate_identifier(reason, "wait blocked reason")?;
                if wait.satisfied || wait.timed_out {
                    bail!("a blocked wait cannot be satisfied or timed out");
                }
                if wait.condition != WaitCondition::TuiIdle {
                    bail!("only tui_idle waits can report a blocked reason");
                }
            }
            Ok(())
        }
    }
}

fn validate_session_state(session: &SessionState) -> Result<()> {
    validate_identifier(&session.session, "session handle")?;
    if let Some(owner) = session.owner.as_deref() {
        validate_owner(owner)?;
    }
    if let Some(client_session_id) = session.client_session_id.as_deref() {
        validate_client_session_id(client_session_id)?;
    }
    if session.cwd.trim().is_empty() {
        bail!("session cwd must not be empty");
    }
    if session.updated_at_ms < session.created_at_ms {
        bail!("session updated_at_ms predates created_at_ms");
    }
    validate_terminal_size(session.cols, session.rows)?;
    BASE64
        .decode(&session.screen_ansi_base64)
        .context("session screen_ansi_base64 is invalid")?;
    validate_optional_hook_text(
        session.tool_name.as_deref(),
        "session tool_name",
        MAX_HOOK_SHORT_TEXT_BYTES,
    )?;
    validate_optional_hook_text(
        session.waiting_reason.as_deref(),
        "session waiting_reason",
        MAX_HOOK_MESSAGE_BYTES,
    )?;
    Ok(())
}

pub(crate) fn validate_owner(owner: &str) -> Result<()> {
    if owner.trim().is_empty()
        || owner.len() > MAX_OWNER_BYTES
        || owner.chars().any(char::is_control)
    {
        bail!("owner must contain between 1 and {MAX_OWNER_BYTES} bytes and no control characters");
    }
    Ok(())
}

pub(crate) fn validate_client_session_id(client_session_id: &str) -> Result<()> {
    if client_session_id.trim().is_empty()
        || client_session_id.len() > MAX_CLIENT_SESSION_ID_BYTES
        || client_session_id.chars().any(char::is_control)
    {
        bail!(
            "client_session_id must contain between 1 and {MAX_CLIENT_SESSION_ID_BYTES} bytes and no control characters"
        );
    }
    Ok(())
}

pub(crate) fn validate_hook_request(provider_session_id: &str, event: &HookEvent) -> Result<()> {
    validate_provider_session_id(provider_session_id)?;
    validate_hook_event(event)
}

pub(crate) fn validate_provider_session_id(provider_session_id: &str) -> Result<()> {
    let valid = provider_session_id.len() == PROVIDER_SESSION_ID_BYTES
        && provider_session_id
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            });
    if !valid {
        bail!("provider_session_id must be a canonical UUID");
    }
    Ok(())
}

fn validate_hook_event(event: &HookEvent) -> Result<()> {
    if let Some(cwd) = event.cwd.as_deref()
        && (cwd.len() > MAX_HOOK_CWD_BYTES || cwd.contains('\0'))
    {
        bail!("hook cwd must contain at most {MAX_HOOK_CWD_BYTES} bytes and no NUL characters");
    }
    validate_optional_hook_text(
        event.tool_name.as_deref(),
        "hook tool_name",
        MAX_HOOK_SHORT_TEXT_BYTES,
    )?;
    validate_optional_hook_text(
        event.message.as_deref(),
        "hook message",
        MAX_HOOK_MESSAGE_BYTES,
    )?;
    validate_optional_hook_text(
        event.notification_type.as_deref(),
        "hook notification_type",
        MAX_HOOK_SHORT_TEXT_BYTES,
    )?;
    validate_optional_hook_text(
        event.level.as_deref(),
        "hook level",
        MAX_HOOK_SHORT_TEXT_BYTES,
    )
}

fn validate_optional_hook_text(value: Option<&str>, field: &str, max_bytes: usize) -> Result<()> {
    if let Some(value) = value
        && (value.len() > max_bytes || value.chars().any(char::is_control))
    {
        bail!("{field} must contain at most {max_bytes} bytes and no control characters");
    }
    Ok(())
}

fn validate_identifier(value: &str, field: impl Display) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        bail!(
            "{field} must contain only ASCII letters, digits, '-', '_' or '.', up to {MAX_IDENTIFIER_BYTES} bytes"
        );
    }
    Ok(())
}

/// WebUI/WebSocket session handles reuse the same identifier rules as CLI frames.
pub(crate) fn validate_session_handle(session: &str) -> Result<()> {
    validate_identifier(session, "session handle")
}
