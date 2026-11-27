use std::collections::HashSet;

use anyhow::Result;

use crate::{
    protocol::{decode_write_data, validate_session_handle, validate_terminal_size},
    session::SessionHost,
};

use super::super::{
    WEB_EVENTS_MAX_MESSAGE_BYTES, WEB_EVENTS_MAX_REQUEST_ID_BYTES,
    WEB_EVENTS_MAX_TRACKED_REQUEST_IDS,
};

/// Client → server command on `/api/events` (JSON text only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) enum WebEventsClientCommand {
    TerminalInput {
        id: String,
        session: String,
        data_base64: String,
    },
    TerminalResize {
        id: String,
        session: String,
        cols: u16,
        rows: u16,
    },
}

/// A parse failure that may still carry enough structure — a legal request id,
/// a recognized command type, and a cleanly extractable session — to correlate
/// the error result with the browser's pending command. Only messages where
/// nothing usable can be identified (invalid JSON, non-object, oversized, or a
/// malformed id) stay id-less with the generic `input_result` type.
#[derive(Debug)]
pub(in crate::server) struct WebEventsParseError {
    pub(in crate::server) message: String,
    pub(in crate::server) id: Option<String>,
    pub(in crate::server) session: Option<String>,
    pub(in crate::server) result_type: &'static str,
}

impl WebEventsParseError {
    fn generic(message: impl Into<String>) -> Self {
        WebEventsParseError {
            message: message.into(),
            id: None,
            session: None,
            result_type: "input_result",
        }
    }

    fn with_context(
        message: impl Into<String>,
        id: Option<String>,
        session: Option<String>,
        result_type: &'static str,
    ) -> Self {
        WebEventsParseError {
            message: message.into(),
            id,
            session,
            result_type,
        }
    }
}

/// Parse a single WebUI client command without panicking on junk.
/// Unknown types yield `Ok(None)` (ignored). Malformed known types yield `Err`
/// carrying the recognized request id and result type when available.
pub(in crate::server) fn parse_web_events_client_command(
    text: &str,
) -> Result<Option<WebEventsClientCommand>, WebEventsParseError> {
    if text.len() > WEB_EVENTS_MAX_MESSAGE_BYTES {
        return Err(WebEventsParseError::generic("message exceeds size limit"));
    }
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| WebEventsParseError::generic(format!("invalid JSON: {error}")))?;
    let Some(object) = value.as_object() else {
        return Err(WebEventsParseError::generic(
            "message must be a JSON object",
        ));
    };
    let message_type = object
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    // Best-effort extraction before the field checks below, so a malformed
    // known-shape command can still be settled by its request id and type.
    let id = match object.get("id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => {
            if value.len() > WEB_EVENTS_MAX_REQUEST_ID_BYTES {
                return Err(WebEventsParseError::generic("request id is too long"));
            }
            Some(value.clone())
        }
        Some(_) => return Err(WebEventsParseError::generic("request id must be a string")),
    };
    let session = object
        .get("session")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let result_type = match message_type {
        "terminal_input" => "input_result",
        "terminal_resize" => "resize_result",
        // Unknown / push-only types (e.g. future clients): ignore safely.
        _ => return Ok(None),
    };
    // Errors past this point keep the recognized id/session/type so the
    // browser can release its pending entry and see a correlated failure.
    // The closure owns its own clones so `id` below can still be moved into
    // the command variants.
    let context_error = {
        let id = id.clone();
        let session = session.clone();
        move |message: String| {
            WebEventsParseError::with_context(message, id.clone(), session.clone(), result_type)
        }
    };

    match message_type {
        "terminal_input" => {
            let Some(id) = id else {
                return Err(context_error("request id is required".to_owned()));
            };
            if id.is_empty() {
                return Err(context_error("request id is required".to_owned()));
            }
            let session = object
                .get("session")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            validate_session_handle(&session).map_err(|err| context_error(format!("{err:#}")))?;
            let data_base64 = object
                .get("data_base64")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            if data_base64.is_empty() {
                return Err(context_error("data_base64 is required".to_owned()));
            }
            Ok(Some(WebEventsClientCommand::TerminalInput {
                id,
                session,
                data_base64,
            }))
        }
        "terminal_resize" => {
            let Some(id) = id else {
                return Err(context_error("request id is required".to_owned()));
            };
            if id.is_empty() {
                return Err(context_error("request id is required".to_owned()));
            }
            let session = object
                .get("session")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            validate_session_handle(&session).map_err(|err| context_error(format!("{err:#}")))?;
            let cols = object
                .get("cols")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| context_error("cols is required".to_owned()))?;
            let rows = object
                .get("rows")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| context_error("rows is required".to_owned()))?;
            let cols =
                u16::try_from(cols).map_err(|_| context_error("cols out of range".to_owned()))?;
            let rows =
                u16::try_from(rows).map_err(|_| context_error("rows out of range".to_owned()))?;
            Ok(Some(WebEventsClientCommand::TerminalResize {
                id,
                session,
                cols,
                rows,
            }))
        }
        _ => Ok(None),
    }
}

/// Apply a parsed command through SessionHost, reusing write_raw / resize.
pub(in crate::server) fn apply_web_events_client_command(
    host: &SessionHost,
    command: &WebEventsClientCommand,
) -> Result<(), String> {
    match command {
        WebEventsClientCommand::TerminalInput {
            session,
            data_base64,
            ..
        } => {
            let data = decode_write_data(data_base64).map_err(|error| format!("{error:#}"))?;
            // Explicit keyboard input proves interactive human presence: refresh
            // client leases before touching the PTY so a provisional orphan claim
            // is canceled right before input delivery.
            host.touch_web_clients()
                .map_err(|error| format!("{error:#}"))?;
            host.write_raw(session, data)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        }
        WebEventsClientCommand::TerminalResize {
            session,
            cols,
            rows,
            ..
        } => {
            // Read-only viewport sync must never refresh client leases: connecting
            // or switching tabs in WebUI automatically emits `terminal_resize` and
            // would otherwise falsely mark expired supervisors as online.
            validate_terminal_size(*cols, *rows).map_err(|error| format!("{error:#}"))?;
            host.resize(session, *cols, *rows)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        }
    }
}

fn web_events_result_type(command: &WebEventsClientCommand) -> &'static str {
    match command {
        WebEventsClientCommand::TerminalInput { .. } => "input_result",
        WebEventsClientCommand::TerminalResize { .. } => "resize_result",
    }
}

fn web_events_command_session(command: &WebEventsClientCommand) -> &str {
    match command {
        WebEventsClientCommand::TerminalInput { session, .. }
        | WebEventsClientCommand::TerminalResize { session, .. } => session.as_str(),
    }
}

fn web_events_command_id(command: &WebEventsClientCommand) -> &str {
    match command {
        WebEventsClientCommand::TerminalInput { id, .. }
        | WebEventsClientCommand::TerminalResize { id, .. } => id.as_str(),
    }
}

/// The single result a client command resolves to: applied, rejected by apply
/// validation, or rejected as a duplicate request id. Exactly one result is
/// always sent back so the caller can match it to its pending command.
pub(in crate::server) struct WebEventsCommandOutcome {
    pub(in crate::server) result_type: &'static str,
    pub(in crate::server) id: Option<String>,
    pub(super) session: Option<String>,
    pub(in crate::server) ok: bool,
    pub(in crate::server) error: Option<String>,
    pub(in crate::server) reconnect: bool,
}

pub(in crate::server) fn plan_web_events_command_outcome(
    host: &SessionHost,
    command: &WebEventsClientCommand,
    seen_ids: &mut HashSet<String>,
) -> WebEventsCommandOutcome {
    let result_type = web_events_result_type(command);
    let id = web_events_command_id(command).to_owned();
    let session = Some(web_events_command_session(command).to_owned());
    // Strict per-connection uniqueness with a hard memory bound. Eviction
    // would let a previously-applied non-idempotent input replay, so a full
    // window rejects one new command and explicitly rotates the connection.
    if seen_ids.contains(&id) {
        return WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: false,
            error: Some("duplicate request id".to_owned()),
            reconnect: false,
        };
    }
    if seen_ids.len() >= WEB_EVENTS_MAX_TRACKED_REQUEST_IDS {
        return WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: false,
            error: Some("request id window exhausted; reconnect required".to_owned()),
            reconnect: true,
        };
    }
    seen_ids.insert(id.clone());
    match apply_web_events_client_command(host, command) {
        Ok(()) => WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: true,
            error: None,
            reconnect: false,
        },
        Err(error) => WebEventsCommandOutcome {
            result_type,
            id: Some(id),
            session,
            ok: false,
            error: Some(error),
            reconnect: false,
        },
    }
}

/// Build a result envelope that never collides with `{ type: "sessions" }` push frames.
pub(in crate::server) fn build_web_events_command_result(
    result_type: &str,
    id: Option<&str>,
    session: Option<&str>,
    ok: bool,
    error: Option<&str>,
    reconnect: bool,
) -> String {
    let mut map = serde_json::Map::new();
    map.insert(
        "type".to_owned(),
        serde_json::Value::String(result_type.to_owned()),
    );
    map.insert("ok".to_owned(), serde_json::Value::Bool(ok));
    if let Some(id) = id {
        map.insert("id".to_owned(), serde_json::Value::String(id.to_owned()));
    }
    if let Some(session) = session {
        map.insert(
            "session".to_owned(),
            serde_json::Value::String(session.to_owned()),
        );
    }
    if let Some(error) = error {
        map.insert(
            "error".to_owned(),
            serde_json::Value::String(error.to_owned()),
        );
    }
    if reconnect {
        map.insert("reconnect".to_owned(), serde_json::Value::Bool(true));
    }
    serde_json::Value::Object(map).to_string()
}
