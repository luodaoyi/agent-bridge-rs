mod commands;
mod connection;
mod handshake;

// Keep the existing server-facing paths, including helpers used only by tests.
#[allow(unused_imports)]
pub(super) use self::{
    commands::{
        WebEventsClientCommand, WebEventsCommandOutcome, WebEventsParseError,
        apply_web_events_client_command, build_web_events_command_result,
        parse_web_events_client_command, plan_web_events_command_outcome,
    },
    handshake::{
        handle_events_websocket, sec_websocket_key_valid, validate_events_websocket_request,
        web_socket_config,
    },
};
