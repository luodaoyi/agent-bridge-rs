use std::{
    env, fmt,
    io::{self, ErrorKind},
    net::{SocketAddr, TcpListener},
    sync::{Arc, atomic::Ordering},
    thread,
};

use anyhow::{Context, Result};

use super::super::{
    ConnectionSlot, RuntimeState, WEB_HTTP_REJECT_DEADLINE, WEB_MAX_ACTIVE_CONNECTIONS,
    admit_connection,
};
use super::{response::write_http_bytes_with_deadline, routes::handle_web_connection};

/// Default WebUI listener. Kept loopback-only because the WebUI has no user
/// authentication; `AGENT_BRIDGE_WEB_ADDR` may only select a loopback address.
const DEFAULT_WEB_ADDR: &str = "127.0.0.1:47654";

/// Why a `AGENT_BRIDGE_WEB_ADDR` value cannot be used as the WebUI listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) enum WebAddrError {
    /// Not parseable as a literal `IpAddr:port`. Hostnames are never resolved.
    InvalidAddress(String),
    /// Parsed, but the IP is not a loopback address.
    NotLoopback(SocketAddr),
}

impl fmt::Display for WebAddrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebAddrError::InvalidAddress(value) => write!(
                formatter,
                "AGENT_BRIDGE_WEB_ADDR {value:?} is not a literal IP address with a port; \
                 the local-only WebUI never resolves hostnames, use a loopback address \
                 such as 127.0.0.1:47654 or [::1]:47654"
            ),
            WebAddrError::NotLoopback(address) => write!(
                formatter,
                "AGENT_BRIDGE_WEB_ADDR {address} is not a loopback address; the local-only \
                 WebUI may only bind to IPv4 127.0.0.0/8 or IPv6 ::1"
            ),
        }
    }
}

impl std::error::Error for WebAddrError {}

/// Parse a `AGENT_BRIDGE_WEB_ADDR` value into a bindable `SocketAddr` and
/// require it to be loopback (IPv4 127.0.0.0/8 or IPv6 ::1). Pure: never
/// resolves hostnames, never binds, never reads the environment.
pub(in crate::server) fn parse_web_addr(value: &str) -> Result<SocketAddr, WebAddrError> {
    match value.parse::<SocketAddr>() {
        Ok(address) if address.ip().is_loopback() => Ok(address),
        Ok(address) => Err(WebAddrError::NotLoopback(address)),
        Err(_) => Err(WebAddrError::InvalidAddress(value.to_owned())),
    }
}

/// Shared by `bind_web_ui` and tests; `value` is the raw `AGENT_BRIDGE_WEB_ADDR`.
pub(in crate::server) fn bind_web_ui_from(value: &str) -> Result<Option<TcpListener>> {
    // Parse and validate before binding so a wildcard, LAN, public, hostname,
    // or unresolvable value is a startup error and never reaches `bind`.
    let socket_addr = parse_web_addr(value)?;
    classify_web_ui_bind(value, TcpListener::bind(socket_addr))
}

pub(in crate::server) fn classify_web_ui_bind(
    value: &str,
    result: io::Result<TcpListener>,
) -> Result<Option<TcpListener>> {
    match result {
        Ok(listener) => Ok(Some(listener)),
        // A legal loopback address whose port is taken only disables the
        // WebUI; JSON CLI and PTY sessions keep running.
        Err(error) if error.kind() == ErrorKind::AddrInUse => {
            eprintln!("agent-bridge server: WebUI unavailable at {value}: {error}");
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| format!("failed to bind WebUI at {value}")),
    }
}

pub(in crate::server) fn bind_web_ui() -> Result<Option<TcpListener>> {
    let address = env::var("AGENT_BRIDGE_WEB_ADDR").unwrap_or_else(|_| DEFAULT_WEB_ADDR.to_owned());
    bind_web_ui_from(&address)
}

pub(in crate::server) fn run_web_ui(listener: TcpListener, state: Arc<RuntimeState>) {
    for connection in listener.incoming() {
        if state.stopping.load(Ordering::Acquire) {
            break;
        }
        match connection {
            Ok(mut stream) => {
                if !admit_connection(&state.web_connections, WEB_MAX_ACTIVE_CONNECTIONS) {
                    // At the Web connection cap: answer with a bounded 503
                    // under a short total deadline and close without spawning
                    // another handler thread, so a peer that never drains
                    // cannot pin the accept loop.
                    let _ = write_http_bytes_with_deadline(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        b"too many active WebUI connections; retry shortly",
                        WEB_HTTP_REJECT_DEADLINE,
                    );
                    continue;
                }
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    // Keep a dedicated Arc for the slot so the counter stays alive for
                    // the whole handler while `state` moves into the handler.
                    let slot_state = Arc::clone(&state);
                    let _slot = ConnectionSlot {
                        slots: &slot_state.web_connections,
                    };
                    handle_web_connection(stream, state);
                });
            }
            Err(error) => eprintln!("agent-bridge server: WebUI accept failed: {error}"),
        }
    }
}
