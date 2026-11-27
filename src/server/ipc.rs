use std::{
    io::BufReader,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use anyhow::{Context, Result};
use interprocess::local_socket::{Stream, prelude::*};

#[cfg(windows)]
use crate::transport::RemapNowaitEmptyRead;
use crate::{
    protocol::{Request, ResponseEnvelope, ResponseResult, decode_request, decode_write_data},
    transport::{IPC_FRAME_READ_DEADLINE, read_frame, write_response},
};

use super::{RuntimeState, listener::wake_listener};

pub(super) fn handle_connection(stream: Stream, state: Arc<RuntimeState>) {
    // Non-blocking I/O lets the request read and the response write be bounded
    // by wall-clock deadlines on every platform (Windows named pipes have no
    // native socket timeouts).
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    #[cfg(windows)]
    let mut connection = BufReader::new(RemapNowaitEmptyRead(stream));
    #[cfg(not(windows))]
    let mut connection = BufReader::new(stream);
    let frame = match read_frame(
        &mut connection,
        Some(Instant::now() + IPC_FRAME_READ_DEADLINE),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            // The peer did not deliver a complete request (silent, closed, or
            // oversized). Record the bounded error and close; there is no
            // request to answer and writing would only wait another write
            // deadline on a peer that is not reading.
            eprintln!("agent-bridge server: IPC request read failed: {error:#}");
            return;
        }
    };
    let envelope = match decode_request(&frame) {
        Ok(envelope) => envelope,
        Err(error) => {
            let response = ResponseEnvelope::failure(
                "invalid-request",
                "invalid_request",
                format!("{error:#}"),
            );
            let _ = write_response(connection.get_mut(), &response);
            return;
        }
    };

    let request_id = envelope.id;
    let client_kind = envelope.client_kind;
    let client_session_id = envelope.client_session_id;
    let refresh_after_response = !matches!(envelope.request, Request::CloseClient);
    let stop_requested = matches!(envelope.request, Request::ServerStop);
    if let Some(client_session_id) = client_session_id.as_deref()
        && let Err(error) = state.host.touch_client(client_session_id)
    {
        let response =
            ResponseEnvelope::failure(request_id, "invalid_client_session", format!("{error:#}"));
        let _ = write_response(connection.get_mut(), &response);
        return;
    }
    let (response, stop_after_response) = match dispatch(
        &state,
        envelope.request,
        client_kind,
        client_session_id.as_deref(),
    ) {
        Ok((result, stop)) => (ResponseEnvelope::success(request_id, result), stop),
        Err(error) => (
            ResponseEnvelope::failure(request_id, "request_failed", format!("{error:#}")),
            // A ServerStop whose shutdown still failed must wake the accept
            // loop anyway so the process can exit; otherwise the loop would
            // wait for a client that never comes while the server holds the
            // session registry and its process ownership.
            stop_requested,
        ),
    };
    let wrote_response = write_response(connection.get_mut(), &response).is_ok();
    if wrote_response
        && response.ok
        && refresh_after_response
        && let Some(client_session_id) = client_session_id.as_deref()
    {
        let _ = state.host.touch_client(client_session_id);
    }
    if stop_after_response {
        wake_listener();
    }
}

fn dispatch(
    state: &RuntimeState,
    request: Request,
    client_kind: Option<crate::protocol::ClientKind>,
    client_session_id: Option<&str>,
) -> Result<(ResponseResult, bool)> {
    let result = match request {
        Request::ServerStatus => ResponseResult::ServerInfo(state.server_info()),
        Request::ServerStop => {
            state.stopping.store(true, Ordering::Release);
            state.host.shutdown_all()?;
            return Ok((ResponseResult::Accepted { accepted: true }, true));
        }
        Request::Heartbeat => {
            let client_session_id = client_session_id.context(
                "heartbeat requires CODEX_THREAD_ID, CODEX_SESSION_ID, or ANTIGRAVITY_CONVERSATION_ID in the client environment",
            )?;
            state.host.touch_client(client_session_id)?;
            ResponseResult::Accepted { accepted: true }
        }
        Request::CloseClient => {
            let client_session_id = client_session_id.context(
                "close_client requires CODEX_THREAD_ID, CODEX_SESSION_ID, or ANTIGRAVITY_CONVERSATION_ID in the client environment",
            )?;
            ResponseResult::CloseGroup(state.host.close_client(client_session_id)?)
        }
        Request::Create {
            worker,
            cwd,
            prompt,
            model,
            owner,
            always_approve,
            sandbox,
            dangerously_bypass,
        } => ResponseResult::Session(Box::new(state.host.create(
            worker,
            &cwd,
            prompt,
            model,
            owner,
            always_approve,
            sandbox,
            dangerously_bypass,
            client_kind,
            client_session_id.map(str::to_owned),
        )?)),
        Request::List => ResponseResult::Sessions {
            sessions: state.host.list()?,
        },
        Request::Show { session } => ResponseResult::Session(Box::new(state.host.show(&session)?)),
        Request::Read {
            session,
            cursor,
            limit,
            wait_ms,
        } => ResponseResult::Read(state.host.read(
            &session,
            cursor.unwrap_or(0),
            limit.unwrap_or(4096) as usize,
            wait_ms.unwrap_or(0),
        )?),
        Request::Send { session, input } => {
            ResponseResult::Session(Box::new(state.host.send(&session, input)?))
        }
        Request::Write {
            session,
            data_base64,
        } => {
            state
                .host
                .write_raw(&session, decode_write_data(&data_base64)?)?;
            ResponseResult::Accepted { accepted: true }
        }
        Request::Resize {
            session,
            cols,
            rows,
        } => {
            state.host.resize(&session, cols, rows)?;
            ResponseResult::Accepted { accepted: true }
        }
        Request::Wait {
            session,
            for_condition,
            timeout_ms,
        } => ResponseResult::Wait(state.host.wait(
            &session,
            for_condition,
            timeout_ms.unwrap_or(300_000),
        )?),
        Request::Close { session } => ResponseResult::Accepted {
            accepted: state.host.close(&session)?,
        },
        Request::HookEvent {
            worker,
            provider_session_id,
            event,
        } => ResponseResult::Accepted {
            accepted: state
                .host
                .apply_hook_event(worker, &provider_session_id, event)?,
        },
    };
    Ok((result, false))
}
