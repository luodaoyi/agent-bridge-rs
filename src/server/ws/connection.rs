use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    net::TcpStream,
    sync::atomic::Ordering,
    time::Duration,
};

use anyhow::Result;
use tungstenite::{Message, WebSocket};

use super::{
    super::{RuntimeState, WEB_EVENTS_CLIENT_POLL, WEB_EVENTS_MAX_MESSAGE_BYTES, now_millis},
    commands::{
        build_web_events_command_result, parse_web_events_client_command,
        plan_web_events_command_outcome,
    },
};

pub(super) fn run_events_websocket(websocket: &mut WebSocket<TcpStream>, state: &RuntimeState) {
    let mut cursors = HashMap::new();
    // Request ids must be unique within this connection; duplicates are
    // rejected before any PTY side effect.
    let mut seen_ids = HashSet::new();
    let mut seen_revision = state.host.revision();
    if !send_web_events(websocket, state, &mut cursors, true) {
        return;
    }

    while !state.stopping.load(Ordering::Acquire) {
        // Service inbound terminal_input / terminal_resize before sleeping so
        // interactive keystrokes never wait behind a multi-second idle timeout.
        match poll_websocket_client(websocket, state, &mut seen_ids) {
            WsClientAction::Continue => {}
            WsClientAction::Close => return,
        }

        let now = now_millis();
        let lease_deadline = state
            .host
            .next_client_lifecycle_deadline_ms()
            .ok()
            .flatten();
        // Sleep until the next host revision *or* the next pure-time lease
        // transition, but never longer than WEB_EVENTS_CLIENT_POLL so client
        // frames stay low-latency. Timeouts without a revision/lease signal
        // never push frames. The event stream itself never refreshes Codex
        // leases (only inbound terminal commands do), so real lease
        // transitions still arrive promptly for display.
        let wait = match lease_deadline {
            Some(deadline) if deadline > now => Duration::from_millis(deadline - now)
                .min(Duration::from_secs(30))
                .max(Duration::from_millis(1))
                .min(WEB_EVENTS_CLIENT_POLL),
            Some(_) => Duration::from_millis(1),
            None => WEB_EVENTS_CLIENT_POLL,
        };
        let current = state.host.wait_revision(seen_revision, wait);
        match poll_websocket_client(websocket, state, &mut seen_ids) {
            WsClientAction::Continue => {}
            WsClientAction::Close => return,
        }
        if state.stopping.load(Ordering::Acquire) {
            break;
        }
        let now_after = now_millis();
        let lease_due = lease_deadline.is_some_and(|deadline| now_after >= deadline);
        if current == seen_revision && !lease_due {
            continue;
        }
        if current != seen_revision {
            seen_revision = current;
        }
        if !send_web_events(websocket, state, &mut cursors, false) {
            return;
        }
    }

    let _ = websocket.close(None);
}

/// Plan frames from immutable cursors, send each frame, and commit cursor
/// advances only after that frame is successfully written. Oversize/encode
/// failures never advance cursors and never silently drop committed bytes.
fn send_web_events(
    websocket: &mut WebSocket<TcpStream>,
    state: &RuntimeState,
    cursors: &mut HashMap<String, u64>,
    force_reset: bool,
) -> bool {
    let frames =
        match state
            .host
            .plan_web_events(cursors, force_reset, WEB_EVENTS_MAX_MESSAGE_BYTES)
        {
            Ok(frames) => frames,
            Err(error) => {
                eprintln!("agent-bridge server: WebUI events plan failed: {error:#}");
                return true;
            }
        };

    for frame in frames {
        let payload = match serde_json::to_string(&frame.message) {
            Ok(payload) => payload,
            Err(error) => {
                eprintln!("agent-bridge server: WebUI events encode failed: {error}");
                // Do not commit any remaining planned cursors.
                return true;
            }
        };
        if payload.len() > WEB_EVENTS_MAX_MESSAGE_BYTES {
            eprintln!(
                "agent-bridge server: WebUI events frame exceeds {} bytes; leaving cursors uncommitted",
                WEB_EVENTS_MAX_MESSAGE_BYTES
            );
            return true;
        }
        if websocket
            .send(Message::text(payload))
            .and_then(|()| websocket.flush())
            .is_err()
        {
            return false;
        }
        for (session, cursor) in frame.cursor_commits {
            cursors.insert(session, cursor);
        }
        for session in frame.cursor_drops {
            cursors.remove(&session);
        }
    }
    true
}

enum WsClientAction {
    Continue,
    Close,
}

fn poll_websocket_client(
    websocket: &mut WebSocket<TcpStream>,
    state: &RuntimeState,
    seen_ids: &mut HashSet<String>,
) -> WsClientAction {
    // Drain a bounded number of control/application frames so a noisy client
    // cannot pin this connection thread forever.
    for _ in 0..32 {
        match websocket.read() {
            Ok(Message::Ping(payload)) => {
                if websocket.send(Message::Pong(payload)).is_err() {
                    return WsClientAction::Close;
                }
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => {
                let _ = websocket.close(None);
                return WsClientAction::Close;
            }
            Ok(Message::Text(text)) => {
                if matches!(
                    handle_web_events_client_text(websocket, state, seen_ids, text.as_str()),
                    WsClientAction::Close
                ) {
                    return WsClientAction::Close;
                }
            }
            // Binary frames are not part of the JSON command protocol.
            Ok(Message::Binary(_)) | Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::Io(error))
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                return WsClientAction::Continue;
            }
            Err(tungstenite::Error::ConnectionClosed)
            | Err(tungstenite::Error::AlreadyClosed)
            | Err(tungstenite::Error::Protocol(_)) => {
                return WsClientAction::Close;
            }
            Err(_) => return WsClientAction::Close,
        }
    }
    WsClientAction::Continue
}

fn send_web_events_command_result(
    websocket: &mut WebSocket<TcpStream>,
    result_type: &str,
    id: Option<&str>,
    session: Option<&str>,
    ok: bool,
    error: Option<&str>,
    reconnect: bool,
) -> Result<(), ()> {
    let payload = build_web_events_command_result(result_type, id, session, ok, error, reconnect);
    if payload.len() > WEB_EVENTS_MAX_MESSAGE_BYTES {
        return Err(());
    }
    websocket
        .send(Message::text(payload))
        .and_then(|()| websocket.flush())
        .map_err(|_| ())
}

fn handle_web_events_client_text(
    websocket: &mut WebSocket<TcpStream>,
    state: &RuntimeState,
    seen_ids: &mut HashSet<String>,
    text: &str,
) -> WsClientAction {
    match parse_web_events_client_command(text) {
        Ok(None) => WsClientAction::Continue,
        Ok(Some(command)) => {
            // terminal_input and terminal_resize always resolve to exactly one
            // result frame (input acks included) keyed by the connection-unique
            // request id, so the browser can settle its pending command.
            let outcome = plan_web_events_command_outcome(&state.host, &command, seen_ids);
            let sent = send_web_events_command_result(
                websocket,
                outcome.result_type,
                outcome.id.as_deref(),
                outcome.session.as_deref(),
                outcome.ok,
                outcome.error.as_deref(),
                outcome.reconnect,
            );
            if sent.is_err() {
                return WsClientAction::Close;
            }
            if outcome.reconnect {
                let _ = websocket.close(None);
                WsClientAction::Close
            } else {
                WsClientAction::Continue
            }
        }
        Err(error) => {
            // Malformed known-shape or generic junk that failed parse: never
            // touch PTY. If the parse preserved a recognized id/type/session,
            // send a correlated failure so the browser can settle (and release
            // the admission bytes of) its pending command; fully-unidentifiable
            // junk falls back to a generic input_result the browser ignores.
            let sent = send_web_events_command_result(
                websocket,
                error.result_type,
                error.id.as_deref(),
                error.session.as_deref(),
                false,
                Some(&error.message),
                false,
            );
            if sent.is_err() {
                WsClientAction::Close
            } else {
                WsClientAction::Continue
            }
        }
    }
}
