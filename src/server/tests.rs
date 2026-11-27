use std::{
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
};

use super::http::{
    DeadlineWrite, ParsedHttpRequest, WebAddrError, bind_web_ui_from, classify_web_ui_bind,
    close_path_segment, handle_web_connection, parse_web_addr, percent_decode_path_segment,
    read_http_line, read_http_request_with_deadline, static_web_asset, web_origin_allowed,
    write_http_bytes, write_tcp_all_with_deadline,
};
use super::ws::{
    WebEventsClientCommand, apply_web_events_client_command, build_web_events_command_result,
    parse_web_events_client_command, plan_web_events_command_outcome, sec_websocket_key_valid,
    validate_events_websocket_request, web_socket_config,
};
use super::*;
use crate::protocol::SessionPhase;
use crate::session::tests::{
    test_host_with_poisoned_close_lock, with_test_host_holding_close_lock,
};
use std::io::Read as _;

#[test]
fn decodes_utf8_owner_path_segments_without_form_url_rules() {
    assert_eq!(
        percent_decode_path_segment("Codex-%E5%AF%B9%E8%AF%9D%2F100%25+ready").unwrap(),
        "Codex-对话/100%+ready"
    );
    assert_eq!(percent_decode_path_segment("A%2fb").unwrap(), "A/b");
}

#[test]
fn rejects_malformed_owner_path_segments() {
    for value in ["owner%", "owner%2", "owner%GG", "%FF"] {
        assert!(percent_decode_path_segment(value).is_err(), "{value}");
    }
}

#[test]
fn extracts_close_routes_without_overlapping_prefix_and_suffix() {
    assert_eq!(
        close_path_segment("/api/owners/Codex%20A/close", "/api/owners/"),
        Some("Codex%20A")
    );
    assert_eq!(
        close_path_segment("/api/owners//close", "/api/owners/"),
        Some("")
    );
    assert_eq!(
        close_path_segment("/api/owners/close", "/api/owners/"),
        None
    );
    assert_eq!(
        close_path_segment("/api/owners/a/b/close", "/api/owners/"),
        None
    );
    assert_eq!(
        close_path_segment("/api/sessions/close", "/api/sessions/"),
        None
    );
    assert_eq!(
        close_path_segment("/api/sessions/session-1/close", "/api/sessions/"),
        Some("session-1")
    );
}

#[test]
fn serves_only_bundled_webui_distribution_assets() {
    for (path, content_type, expected_body) in [
        ("/", "text/html; charset=utf-8", WEB_UI_HTML),
        (
            "/assets/app.js",
            "text/javascript; charset=utf-8",
            WEB_UI_JS,
        ),
        ("/assets/app.css", "text/css; charset=utf-8", WEB_UI_CSS),
    ] {
        let asset = static_web_asset(path).expect("static route must exist");
        assert_eq!(asset.content_type, content_type);
        assert_eq!(asset.body, expected_body);
        assert!(!asset.body.is_empty());

        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let response = serve_web_request(request.as_bytes());
        let (headers, body) = split_http_response(&response);
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains(&format!("Content-Type: {content_type}")));
        assert_eq!(body, expected_body);
    }

    let html = std::str::from_utf8(WEB_UI_HTML).expect("index.html must be UTF-8");
    assert!(html.contains("/assets/app.js"));
    assert!(html.contains("/assets/app.css"));
    assert!(static_web_asset("/api/sessions").is_none());
    assert!(static_web_asset("/assets/missing.js").is_none());
}

#[test]
fn sessions_api_remains_json_instead_of_static_content() {
    let response = serve_web_request(b"GET /api/sessions HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, body) = split_http_response(&response);
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(headers.contains("Content-Type: application/json"));
    assert_eq!(body, b"[]");
}

#[test]
fn version_api_reports_current_package_version() {
    let response = serve_web_request(b"GET /api/version HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let (headers, body) = split_http_response(&response);
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(headers.contains("Content-Type: application/json"));
    let value: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(value["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["update_available"], false);
    assert!(
        value["release_url"]
            .as_str()
            .unwrap()
            .contains("github.com/luodaoyi/agent-bridge-rs/releases")
    );
}

#[test]
fn byte_http_writer_uses_raw_body_length() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    let body = [0, 0xff, b'\n', 0x80];

    write_http_bytes(&mut server, "200 OK", "application/octet-stream", &body).unwrap();
    drop(server);

    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = std::str::from_utf8(&response[..separator]).unwrap();
    assert!(headers.contains("Content-Length: 4"));
    assert_eq!(&response[separator + 4..], body);
}

#[test]
fn close_api_still_requires_webui_header() {
    let response =
        serve_web_request(b"POST /api/sessions/missing/close HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
    assert!(response.ends_with(b"missing WebUI request header"));
}

#[test]
fn close_api_distinguishes_missing_timeout_and_internal_failures() {
    // 404: a missing session through the real request path.
    let response = serve_web_request(
        b"POST /api/sessions/missing/close HTTP/1.1\r\nHost: localhost\r\nX-Agent-Bridge-WebUI: 1\r\n\r\n",
    );
    let (headers, body) = split_http_response(&response);
    assert!(
        headers.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{headers}"
    );
    assert!(
        std::str::from_utf8(body)
            .unwrap()
            .contains("session not found: missing"),
        "404 body: {body:?}"
    );

    // 504: the close lock is held past the deadline, so close really times
    // out and the route maps it to Gateway Timeout with the error body.
    let response = with_test_host_holding_close_lock(
        "provider-timeout",
        SessionPhase::Running,
        |host| {
            serve_web_request_with_host(
                b"POST /api/sessions/gbt-test/close HTTP/1.1\r\nHost: localhost\r\nX-Agent-Bridge-WebUI: 1\r\n\r\n",
                host,
            )
        },
    );
    let (headers, body) = split_http_response(&response);
    assert!(
        headers.starts_with("HTTP/1.1 504 Gateway Timeout\r\n"),
        "{headers}"
    );
    assert!(
        std::str::from_utf8(body)
            .unwrap()
            .contains("close timed out"),
        "504 body: {body:?}"
    );

    // 500: a poisoned close lock is an internal failure through the real
    // request path and surfaces as Internal Server Error.
    let host = test_host_with_poisoned_close_lock("provider-failed", SessionPhase::Running);
    let response = serve_web_request_with_host(
        b"POST /api/sessions/gbt-test/close HTTP/1.1\r\nHost: localhost\r\nX-Agent-Bridge-WebUI: 1\r\n\r\n",
        host,
    );
    let (headers, body) = split_http_response(&response);
    assert!(
        headers.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{headers}"
    );
    assert!(
        std::str::from_utf8(body)
            .unwrap()
            .contains("session close lock was poisoned"),
        "500 body: {body:?}"
    );
}

#[test]
fn web_origin_requires_matching_loopback_host() {
    assert!(web_origin_allowed(
        Some("http://127.0.0.1:47654"),
        Some("127.0.0.1:47654")
    ));
    assert!(web_origin_allowed(
        Some("http://localhost:47654"),
        Some("localhost:47654")
    ));
    assert!(!web_origin_allowed(
        Some("http://evil.example:47654"),
        Some("127.0.0.1:47654")
    ));
    assert!(!web_origin_allowed(
        Some("http://127.0.0.1:47654"),
        Some("127.0.0.1:9")
    ));
    assert!(!web_origin_allowed(None, Some("127.0.0.1:47654")));
    assert!(!web_origin_allowed(
        Some("https://127.0.0.1:47654"),
        Some("127.0.0.1:47654")
    ));
}

#[test]
fn parses_only_loopback_web_addr_as_socket_addr() {
    // Default and other IPv4 127.0.0.0/8 addresses, including port 0.
    for value in [
        "127.0.0.1:47654",
        "127.0.0.0:80",
        "127.255.255.254:1",
        "127.0.0.1:0",
    ] {
        assert_eq!(
            parse_web_addr(value).unwrap(),
            value.parse::<SocketAddr>().unwrap(),
            "{value}"
        );
    }
    // IPv6 ::1 is accepted; this only parses, it never needs the machine
    // to support binding an IPv6 socket.
    for value in ["[::1]:47654", "[::1]:0"] {
        assert_eq!(
            parse_web_addr(value).unwrap(),
            value.parse::<SocketAddr>().unwrap(),
            "{value}"
        );
    }
}

#[test]
fn rejects_wildcard_lan_and_public_web_addr_before_binding() {
    for value in [
        "0.0.0.0:47654",            // IPv4 wildcard
        "[::]:47654",               // IPv6 wildcard
        "192.168.1.10:47654",       // LAN (RFC 1918)
        "10.0.0.5:47654",           // LAN (RFC 1918)
        "172.16.0.1:47654",         // LAN (RFC 1918)
        "169.254.1.1:47654",        // link-local
        "8.8.8.8:47654",            // public IPv4
        "1.2.3.4:80",               // public IPv4
        "[2001:db8::1]:47654",      // public IPv6
        "[::ffff:127.0.0.1]:47654", // IPv4-mapped IPv6 is not loopback
    ] {
        let err = parse_web_addr(value).unwrap_err();
        assert!(
            matches!(err, WebAddrError::NotLoopback(_)),
            "expected NotLoopback for {value:?}, got {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("local-only"), "{value:?}: {message}");
        assert!(message.contains("127.0.0.0/8"), "{value:?}: {message}");
    }
}

#[test]
fn rejects_hostname_and_unresolvable_web_addr_without_dns() {
    for value in [
        "",
        "   ",
        "not-an-address",
        "127.0.0.1",          // missing port
        "127.0.0.1:notaport", // non-numeric port
        "127.0.0.1:70000",    // port out of range
        "localhost:47654",    // hostname must not be resolved
        "myhost.local:47654", // hostname must not be resolved
    ] {
        let err = parse_web_addr(value).unwrap_err();
        assert!(
            matches!(err, WebAddrError::InvalidAddress(_)),
            "expected InvalidAddress for {value:?}, got {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("local-only"), "{value:?}: {message}");
        assert!(message.contains("hostnames"), "{value:?}: {message}");
    }
}

#[test]
fn non_loopback_or_invalid_web_addr_is_a_startup_error() {
    // These must fail before any TcpListener::bind attempt.
    for value in [
        "0.0.0.0:47654",
        "8.8.8.8:47654",
        "localhost:47654",
        "garbage",
    ] {
        let err = bind_web_ui_from(value).unwrap_err();
        assert!(err.to_string().contains("local-only"), "{value:?}: {err:#}");
    }
}

#[test]
fn occupied_loopback_port_disables_webui_without_startup_error() {
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = occupied.local_addr().unwrap().to_string();
    assert!(
        parse_web_addr(&address).is_ok(),
        "{address} must be loopback"
    );
    let result = bind_web_ui_from(&address).unwrap();
    assert!(
        result.is_none(),
        "occupied port must keep WebUI unavailable"
    );
}

#[test]
fn non_addr_in_use_web_bind_errors_remain_startup_errors() {
    for kind in [ErrorKind::PermissionDenied, ErrorKind::AddrNotAvailable] {
        let error = classify_web_ui_bind(
            "127.0.0.1:47654",
            Err(io::Error::new(kind, "injected bind failure")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("failed to bind WebUI"));
        assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), kind);
    }
}

#[test]
fn http_line_reading_is_capped_and_strips_line_endings() {
    let mut reader = BufReader::new(&b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..]);
    let deadline = Instant::now() + Duration::from_secs(5);
    assert_eq!(
        read_http_line(&mut reader, 64, deadline).unwrap(),
        b"GET / HTTP/1.1"
    );
    assert_eq!(
        read_http_line(&mut reader, 64, deadline).unwrap(),
        b"Host: x"
    );
    assert!(
        read_http_line(&mut reader, 64, deadline)
            .unwrap()
            .is_empty()
    );

    let long = [b'a'; 16];
    let mut reader = BufReader::new(&long[..]);
    assert!(read_http_line(&mut reader, 8, deadline).is_err());
}

#[test]
fn oversized_request_line_is_rejected_with_400() {
    let path = format!("/{}", "x".repeat(WEB_HTTP_MAX_REQUEST_LINE_BYTES));
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let response = serve_web_request(request.as_bytes());
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    let (_, body) = split_http_response(&response);
    assert!(
        String::from_utf8_lossy(body).contains("size limit"),
        "{}",
        String::from_utf8_lossy(body)
    );
}

#[test]
fn too_many_headers_are_rejected_with_400() {
    let mut request = String::from("GET / HTTP/1.1\r\nHost: localhost\r\n");
    for _ in 0..(WEB_HTTP_MAX_HEADER_COUNT + 1) {
        request.push_str("X-Pad: 1\r\n");
    }
    request.push_str("\r\n");
    let response = serve_web_request(request.as_bytes());
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    let (_, body) = split_http_response(&response);
    assert!(String::from_utf8_lossy(body).contains("headers"));
}

#[test]
fn oversized_total_header_bytes_are_rejected_with_400() {
    // Five ~7 KiB headers stay under the per-line cap but exceed the
    // 32 KiB total header budget.
    let mut request = String::from("GET / HTTP/1.1\r\nHost: localhost\r\n");
    for index in 0..5 {
        request.push_str(&format!("X-Pad-{index}: {}\r\n", "x".repeat(7000)));
    }
    request.push_str("\r\n");
    let response = serve_web_request(request.as_bytes());
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    let (_, body) = split_http_response(&response);
    assert!(
        String::from_utf8_lossy(body).contains("size limit"),
        "{}",
        String::from_utf8_lossy(body)
    );
}

#[test]
fn http_request_read_stops_at_the_wall_clock_deadline_for_a_trickling_client() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    server.set_nonblocking(true).unwrap();
    client.write_all(b"GET / HT").unwrap(); // incomplete request line

    let deadline = Instant::now() + Duration::from_millis(300);
    let started = Instant::now();
    let err = read_http_request_with_deadline(&mut server, deadline).unwrap_err();
    assert!(err.contains("timed out"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn http_response_body_over_the_limit_is_replaced_with_a_bounded_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    let oversized = vec![0u8; WEB_HTTP_MAX_RESPONSE_BYTES + 1];

    write_http_bytes(
        &mut server,
        "200 OK",
        "application/octet-stream",
        &oversized,
    )
    .unwrap();
    drop(server);

    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    let (headers, body) = split_http_response(&response);
    assert!(headers.starts_with("HTTP/1.1 500 Internal Server Error\r\n"));
    assert_eq!(body, b"server response exceeds the size limit");
}

#[test]
fn http_write_stops_at_the_total_deadline_for_a_peer_that_stops_draining() {
    // Deterministic fake peer: accepts a bounded prefix, then reports
    // WouldBlock forever — the same failure mode as a real receive
    // buffer filling up, without depending on per-platform kernel socket
    // buffers (which std::net does not expose, and whose sizes differ
    // between Windows loopback and Unix).
    struct StalledWrite {
        accepted: usize,
    }

    impl Write for StalledWrite {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.accepted >= 4 * 1024 {
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "peer stopped draining",
                ));
            }
            let take = (4 * 1024 - self.accepted).min(buf.len());
            self.accepted += take;
            Ok(take)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl DeadlineWrite for StalledWrite {
        fn set_write_timeout(&mut self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    // The payload far exceeds what the peer accepts; the write must
    // stall until the total deadline, with the bounded backoff keeping
    // the loop from running past it by much.
    let payload = vec![0x55u8; 8 * 1024];
    let started = Instant::now();
    let error = write_tcp_all_with_deadline(
        &mut StalledWrite { accepted: 0 },
        &payload,
        Duration::from_millis(200),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
}

#[test]
fn connection_admission_is_bounded_and_releases_slots() {
    let slots = AtomicUsize::new(0);
    for _ in 0..3 {
        assert!(admit_connection(&slots, 3));
    }
    assert!(!admit_connection(&slots, 3));
    assert_eq!(slots.load(Ordering::Relaxed), 3);

    drop(ConnectionSlot { slots: &slots });
    assert_eq!(slots.load(Ordering::Relaxed), 2);
    assert!(admit_connection(&slots, 3));
}

#[test]
fn websocket_config_has_explicit_frame_message_and_buffer_limits() {
    let config = web_socket_config();
    assert_eq!(config.max_message_size, Some(WEB_EVENTS_MAX_MESSAGE_BYTES));
    assert_eq!(config.max_frame_size, Some(WEB_EVENTS_MAX_MESSAGE_BYTES));
    assert_eq!(config.write_buffer_size, 0);
    assert_eq!(config.max_write_buffer_size, WEB_EVENTS_MAX_MESSAGE_BYTES);
    assert_eq!(config.read_buffer_size, 8 * 1024);
}

#[test]
fn connection_caps_and_http_limits_are_bounded() {
    assert!((1..=256).contains(&WEB_MAX_ACTIVE_CONNECTIONS));
    assert!((1..=256).contains(&IPC_MAX_ACTIVE_CONNECTIONS));
    // Compile-time invariants: header lines nest inside the header budget
    // and the request line/response budgets stay large enough to be useful.
    const {
        assert!(WEB_HTTP_MAX_HEADER_LINE_BYTES <= WEB_HTTP_MAX_HEADER_BYTES);
        assert!(WEB_HTTP_MAX_REQUEST_LINE_BYTES >= 256);
        assert!(WEB_HTTP_MAX_RESPONSE_BYTES >= 1024 * 1024);
    }
}

#[test]
fn events_websocket_handshake_rejects_bad_origin_and_path() {
    let missing_upgrade = ParsedHttpRequest {
        method: "GET".to_owned(),
        path: "/api/events".to_owned(),
        bridge_header: false,
        host: Some("127.0.0.1:47654".to_owned()),
        origin: Some("http://127.0.0.1:47654".to_owned()),
        upgrade_websocket: false,
        connection_upgrade: false,
        sec_websocket_key: Some("dGhlIHNhbXBsZSBub25jZQ==".to_owned()),
        sec_websocket_version: Some("13".to_owned()),
    };
    assert!(validate_events_websocket_request(&missing_upgrade).is_err());

    let bad_origin = ParsedHttpRequest {
        upgrade_websocket: true,
        connection_upgrade: true,
        origin: Some("http://evil.example".to_owned()),
        ..missing_upgrade.clone()
    };
    assert_eq!(
        validate_events_websocket_request(&bad_origin).unwrap_err(),
        "origin not allowed"
    );

    let bad_path = ParsedHttpRequest {
        path: "/api/sessions".to_owned(),
        upgrade_websocket: true,
        connection_upgrade: true,
        origin: Some("http://127.0.0.1:47653".to_owned()),
        ..missing_upgrade
    };
    assert!(validate_events_websocket_request(&bad_path).is_err());
}

#[test]
fn events_websocket_handshake_accepts_same_origin_loopback() {
    let request = ParsedHttpRequest {
        method: "GET".to_owned(),
        path: "/api/events".to_owned(),
        bridge_header: false,
        host: Some("127.0.0.1:47654".to_owned()),
        origin: Some("http://127.0.0.1:47654".to_owned()),
        upgrade_websocket: true,
        connection_upgrade: true,
        sec_websocket_key: Some("dGhlIHNhbXBsZSBub25jZQ==".to_owned()),
        sec_websocket_version: Some("13".to_owned()),
    };
    assert!(validate_events_websocket_request(&request).is_ok());
}

#[test]
fn sec_websocket_key_must_be_rfc6455_16_byte_base64() {
    // RFC 6455 example key decodes to 16 bytes.
    assert!(sec_websocket_key_valid("dGhlIHNhbXBsZSBub25jZQ=="));
    assert!(!sec_websocket_key_valid(""));
    assert!(!sec_websocket_key_valid("   "));
    assert!(!sec_websocket_key_valid("not-base64!!!"));
    // Valid base64 but wrong decoded length (3 bytes).
    assert!(!sec_websocket_key_valid("YWJj"));
    // Valid base64 of 15 bytes.
    assert!(!sec_websocket_key_valid("AAAAAAAAAAAAAAAAAAAA"));
    // Valid base64 of 17 bytes.
    assert!(!sec_websocket_key_valid("AQIDBAUGBwgJCgsMDQ4PEBE="));

    let mut request = ParsedHttpRequest {
        method: "GET".to_owned(),
        path: "/api/events".to_owned(),
        bridge_header: false,
        host: Some("127.0.0.1:47654".to_owned()),
        origin: Some("http://127.0.0.1:47654".to_owned()),
        upgrade_websocket: true,
        connection_upgrade: true,
        sec_websocket_key: Some("YWJj".to_owned()),
        sec_websocket_version: Some("13".to_owned()),
    };
    assert_eq!(
        validate_events_websocket_request(&request).unwrap_err(),
        "invalid Sec-WebSocket-Key"
    );
    request.sec_websocket_key = Some("dGhlIHNhbXBsZSBub25jZQ==".to_owned());
    assert!(validate_events_websocket_request(&request).is_ok());
}

#[test]
fn events_api_without_upgrade_stays_http_error() {
    let response = serve_web_request(
        b"GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:47653\r\nOrigin: http://127.0.0.1:47653\r\n\r\n",
    );
    assert!(response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
}

#[test]
fn parse_terminal_input_and_resize_commands() {
    let input = parse_web_events_client_command(
        r#"{"type":"terminal_input","id":"r1","session":"gbt-1","data_base64":"YQ=="}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        input,
        WebEventsClientCommand::TerminalInput {
            id: "r1".to_owned(),
            session: "gbt-1".to_owned(),
            data_base64: "YQ==".to_owned(),
        }
    );

    let resize = parse_web_events_client_command(
        r#"{"type":"terminal_resize","id":"r2","session":"gbt-1","cols":120,"rows":40}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        resize,
        WebEventsClientCommand::TerminalResize {
            id: "r2".to_owned(),
            session: "gbt-1".to_owned(),
            cols: 120,
            rows: 40,
        }
    );

    // Unknown types are ignored (push-only / future).
    assert_eq!(
        parse_web_events_client_command(r#"{"type":"sessions","sessions":[]}"#).unwrap(),
        None
    );
}

#[test]
fn parse_rejects_missing_empty_or_oversized_request_ids() {
    let missing = r#"{"type":"terminal_input","session":"gbt-1","data_base64":"YQ=="}"#;
    assert!(parse_web_events_client_command(missing).is_err());
    let empty = r#"{"type":"terminal_resize","id":"","session":"gbt-1","cols":80,"rows":24}"#;
    assert!(parse_web_events_client_command(empty).is_err());
    let numeric = r#"{"type":"terminal_input","id":7,"session":"gbt-1","data_base64":"YQ=="}"#;
    assert!(parse_web_events_client_command(numeric).is_err());
    let long_id = format!(
        r#"{{"type":"terminal_input","id":{id},"session":"gbt-1","data_base64":"YQ=="}}"#,
        id = serde_json::to_string(&"x".repeat(WEB_EVENTS_MAX_REQUEST_ID_BYTES + 1)).unwrap()
    );
    assert!(parse_web_events_client_command(&long_id).is_err());
}

#[test]
fn parse_rejects_malformed_and_oversized_input_without_command() {
    assert!(parse_web_events_client_command("not-json").is_err());
    assert!(
        parse_web_events_client_command(
            r#"{"type":"terminal_input","id":"r1","session":"","data_base64":"YQ=="}"#
        )
        .is_err()
    );
    assert!(
        parse_web_events_client_command(r#"{"type":"terminal_input","id":"r1","session":"gbt-1"}"#)
            .is_err()
    );
    assert!(
        parse_web_events_client_command(
            r#"{"type":"terminal_resize","id":"r1","session":"gbt-1","cols":1,"rows":40}"#
        )
        .is_ok()
    );
    // cols=1 is parsed; apply-time validate_terminal_size rejects it.
    let cmd = parse_web_events_client_command(
        r#"{"type":"terminal_resize","id":"r1","session":"gbt-1","cols":1,"rows":40}"#,
    )
    .unwrap()
    .unwrap();
    let host = SessionHost::new(OrphanPolicy {
        lease_ms: 120_000,
        grace_ms: 600_000,
    });
    assert!(apply_web_events_client_command(&host, &cmd).is_err());
}

#[test]
fn apply_terminal_input_uses_decode_write_data_and_rejects_unknown_session() {
    let host = SessionHost::new(OrphanPolicy {
        lease_ms: 120_000,
        grace_ms: 600_000,
    });
    // Exact raw bytes for "A" (0x41) via standard base64.
    let cmd = WebEventsClientCommand::TerminalInput {
        id: "n1".to_owned(),
        session: "missing-session".to_owned(),
        data_base64: "QQ==".to_owned(),
    };
    let data = decode_write_data("QQ==").unwrap();
    assert_eq!(data, vec![0x41]);
    // Unknown session: error, never panics, never writes.
    let err = apply_web_events_client_command(&host, &cmd).unwrap_err();
    assert!(!err.is_empty());

    // Empty / oversized rejected by decode_write_data before host write.
    let empty = WebEventsClientCommand::TerminalInput {
        id: "e1".to_owned(),
        session: "missing-session".to_owned(),
        data_base64: "".to_owned(),
    };
    // Empty base64 fails at parse (required).
    assert!(
        parse_web_events_client_command(
            r#"{"type":"terminal_input","id":"r1","session":"s","data_base64":""}"#
        )
        .is_err()
    );
    let _ = empty;

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    let oversize = BASE64.encode(vec![0x5a; crate::protocol::MAX_WRITE_BYTES + 1]);
    let over = WebEventsClientCommand::TerminalInput {
        id: "o1".to_owned(),
        session: "missing-session".to_owned(),
        data_base64: oversize,
    };
    assert!(apply_web_events_client_command(&host, &over).is_err());
}

#[test]
fn command_result_json_is_not_sessions_type() {
    let payload = build_web_events_command_result(
        "input_result",
        Some("r1"),
        Some("gbt-1"),
        false,
        Some("nope"),
        false,
    );
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(value["type"], "input_result");
    assert_eq!(value["ok"], false);
    assert_eq!(value["id"], "r1");
    assert_eq!(value["session"], "gbt-1");
    assert_eq!(value["error"], "nope");
    assert!(value.get("reconnect").is_none());
    assert!(value.get("sessions").is_none());
    assert!(value.get("terminals").is_none());

    let rotate = build_web_events_command_result(
        "input_result",
        Some("r2"),
        Some("gbt-1"),
        false,
        Some("request id window exhausted; reconnect required"),
        true,
    );
    let rotate: serde_json::Value = serde_json::from_str(&rotate).unwrap();
    assert_eq!(rotate["reconnect"], true);
}

#[test]
fn client_poll_interval_is_bounded_for_interactive_latency() {
    // Guard against regressions that reintroduce multi-second waits before
    // reading client frames.
    assert!(WEB_EVENTS_CLIENT_POLL <= Duration::from_millis(100));
    assert!(WEB_EVENTS_CLIENT_POLL >= Duration::from_millis(1));
}

#[test]
fn web_events_command_plan_rejects_duplicate_ids_and_acks_input_success() {
    use std::collections::HashSet as StdSet;

    let host = SessionHost::new(OrphanPolicy {
        lease_ms: 120_000,
        grace_ms: 600_000,
    });
    let command = WebEventsClientCommand::TerminalInput {
        id: "k1".to_owned(),
        session: "missing-session".to_owned(),
        data_base64: "YQ==".to_owned(),
    };
    let mut seen = StdSet::new();
    // Unknown session fails apply, but still resolves to exactly one result.
    let outcome = plan_web_events_command_outcome(&host, &command, &mut seen);
    assert_eq!(outcome.result_type, "input_result");
    assert_eq!(outcome.id.as_deref(), Some("k1"));
    assert!(!outcome.ok);
    assert!(outcome.error.is_some());

    // Same id again on the same connection is a strict duplicate rejection,
    // even though the first attempt never touched a PTY.
    let duplicate = plan_web_events_command_outcome(&host, &command, &mut seen);
    assert_eq!(duplicate.id.as_deref(), Some("k1"));
    assert!(!duplicate.ok);
    assert!(
        duplicate
            .error
            .as_deref()
            .is_some_and(|error| error.contains("duplicate request id"))
    );

    // A fresh id resolves independently.
    let other = WebEventsClientCommand::TerminalInput {
        id: "k2".to_owned(),
        session: "missing-session".to_owned(),
        data_base64: "YQ==".to_owned(),
    };
    let second = plan_web_events_command_outcome(&host, &other, &mut seen);
    assert_eq!(second.id.as_deref(), Some("k2"));
    assert!(!second.ok);

    // A missing session fails apply for resize too, but still resolves to
    // exactly one resize_result carrying the id.
    let resize = WebEventsClientCommand::TerminalResize {
        id: "k3".to_owned(),
        session: "missing-session".to_owned(),
        cols: 80,
        rows: 24,
    };
    let resize_outcome = plan_web_events_command_outcome(&host, &resize, &mut seen);
    assert_eq!(resize_outcome.result_type, "resize_result");
    assert_eq!(resize_outcome.id.as_deref(), Some("k3"));
    assert!(!resize_outcome.ok);
}

#[test]
fn request_id_window_rotates_connection_when_full_without_evicting_old_ids() {
    use std::collections::HashSet as StdSet;

    let host = SessionHost::new(OrphanPolicy {
        lease_ms: 120_000,
        grace_ms: 600_000,
    });
    let mut seen = StdSet::new();
    let first_id = "bulk-0".to_owned();
    for index in 0..WEB_EVENTS_MAX_TRACKED_REQUEST_IDS {
        let command = WebEventsClientCommand::TerminalInput {
            id: format!("bulk-{index}"),
            session: "missing-session".to_owned(),
            data_base64: "YQ==".to_owned(),
        };
        let outcome = plan_web_events_command_outcome(&host, &command, &mut seen);
        assert!(
            !outcome
                .error
                .as_deref()
                .is_some_and(|error| error.contains("duplicate request id"))
        );
    }
    assert_eq!(seen.len(), WEB_EVENTS_MAX_TRACKED_REQUEST_IDS);

    // Window is full: the new command is rejected before apply and the
    // result instructs the browser to rotate this exhausted connection.
    let extra = WebEventsClientCommand::TerminalInput {
        id: "bulk-overflow".to_owned(),
        session: "missing-session".to_owned(),
        data_base64: "YQ==".to_owned(),
    };
    let overflow = plan_web_events_command_outcome(&host, &extra, &mut seen);
    assert!(!overflow.ok);
    assert_eq!(
        overflow.error.as_deref(),
        Some("request id window exhausted; reconnect required")
    );
    assert!(overflow.reconnect);
    assert_eq!(seen.len(), WEB_EVENTS_MAX_TRACKED_REQUEST_IDS);

    // An OLD applied-or-rejected id is never re-admitted for replay: it is
    // explicitly rejected as a duplicate, never re-applied.
    let replay = WebEventsClientCommand::TerminalInput {
        id: first_id,
        session: "missing-session".to_owned(),
        data_base64: "YQ==".to_owned(),
    };
    let replay_outcome = plan_web_events_command_outcome(&host, &replay, &mut seen);
    assert!(!replay_outcome.ok);
    assert_eq!(
        replay_outcome.error.as_deref(),
        Some("duplicate request id")
    );
}

#[test]
fn parse_rejects_invalid_session_handles_before_host_touch() {
    let long = "x".repeat(200);
    let bad_cases = [
        "",
        "has space",
        "bad/id",
        long.as_str(),
        "ctrl\n",
        "unicode-\u{4e2d}",
    ];
    for bad in bad_cases {
        let payload = format!(
            r#"{{"type":"terminal_input","id":"r1","session":{session},"data_base64":"YQ=="}}"#,
            session = serde_json::to_string(bad).unwrap()
        );
        let err = parse_web_events_client_command(&payload).unwrap_err();
        assert!(
            err.message.contains("session handle") || err.message.contains("session"),
            "bad={bad:?} err={:?}",
            err.message
        );
        let resize = format!(
            r#"{{"type":"terminal_resize","id":"r1","session":{session},"cols":80,"rows":24}}"#,
            session = serde_json::to_string(bad).unwrap()
        );
        assert!(parse_web_events_client_command(&resize).is_err());
    }
    // Valid handle shape is accepted by the parser (host still enforces existence).
    assert!(
        parse_web_events_client_command(
            r#"{"type":"terminal_input","id":"r1","session":"gbt-1","data_base64":"YQ=="}"#
        )
        .unwrap()
        .is_some()
    );
}

#[test]
fn parse_errors_keep_recognized_id_session_and_result_type() {
    // Malformed known-shape command: the error still carries the request
    // id, session, and result type so the browser can settle its pending
    // entry and release the admission bytes it holds.
    let err =
        parse_web_events_client_command(r#"{"type":"terminal_input","id":"r1","session":"gbt-1"}"#)
            .unwrap_err();
    assert_eq!(err.id.as_deref(), Some("r1"));
    assert_eq!(err.session.as_deref(), Some("gbt-1"));
    assert_eq!(err.result_type, "input_result");
    assert!(err.message.contains("data_base64"));

    // Invalid session handle: id and result type survive.
    let err = parse_web_events_client_command(
        r#"{"type":"terminal_input","id":"r2","session":"bad id","data_base64":"YQ=="}"#,
    )
    .unwrap_err();
    assert_eq!(err.id.as_deref(), Some("r2"));
    assert_eq!(err.result_type, "input_result");
    assert_eq!(err.session.as_deref(), Some("bad id"));

    // resize errors keep resize_result.
    let err = parse_web_events_client_command(
        r#"{"type":"terminal_resize","id":"r3","session":"gbt-1"}"#,
    )
    .unwrap_err();
    assert_eq!(err.id.as_deref(), Some("r3"));
    assert_eq!(err.result_type, "resize_result");
    assert!(err.message.contains("cols"));

    // Missing id: nothing to correlate, generic fallback type.
    let err = parse_web_events_client_command(
        r#"{"type":"terminal_input","session":"gbt-1","data_base64":"YQ=="}"#,
    )
    .unwrap_err();
    assert_eq!(err.id, None);
    assert_eq!(err.session.as_deref(), Some("gbt-1"));
    assert_eq!(err.result_type, "input_result");
}

#[test]
fn generic_parse_errors_stay_id_less_with_input_result_type() {
    // Junk that carries no recognizable id keeps the generic fallback.
    for payload in ["not-json", r#"[]"#, r#""plain string""#] {
        let err = parse_web_events_client_command(payload).unwrap_err();
        assert_eq!(err.id, None);
        assert_eq!(err.result_type, "input_result");
    }
    // A non-object with an unknown type is ignored, not an error.
    assert_eq!(
        parse_web_events_client_command(r#"{"type":7}"#).unwrap(),
        None
    );
    // Oversized / non-string id is also not correlatable.
    let err = parse_web_events_client_command(
        r#"{"type":"terminal_input","id":7,"session":"gbt-1","data_base64":"YQ=="}"#,
    )
    .unwrap_err();
    assert_eq!(err.id, None);
    assert_eq!(err.result_type, "input_result");
}

#[test]
fn client_close_api_reports_an_exact_empty_group() {
    let response = serve_web_request(
        b"POST /api/clients/codex-thread-42/close HTTP/1.1\r\nHost: localhost\r\nX-Agent-Bridge-WebUI: 1\r\n\r\n",
    );
    let (headers, body) = split_http_response(&response);
    assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(body).unwrap(),
        serde_json::json!({ "matched": 0, "closed": 0, "timed_out": [], "failures": [] })
    );
}

fn serve_web_request(request: &[u8]) -> Vec<u8> {
    serve_web_request_with_host(
        request,
        SessionHost::new(OrphanPolicy {
            lease_ms: 120_000,
            grace_ms: 600_000,
        }),
    )
}

fn serve_web_request_with_host(request: &[u8], host: SessionHost) -> Vec<u8> {
    let timeout = Duration::from_secs(10);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    client.set_read_timeout(Some(timeout)).unwrap();
    client.set_write_timeout(Some(timeout)).unwrap();
    server.set_read_timeout(Some(timeout)).unwrap();
    server.set_write_timeout(Some(timeout)).unwrap();
    client.write_all(request).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();

    let handler = std::thread::spawn(move || {
        handle_web_connection(
            server,
            Arc::new(RuntimeState {
                host,
                started_at_ms: 0,
                stopping: AtomicBool::new(false),
                web_url: None,
                version_checker: Arc::new(VersionChecker::new()),
                web_connections: AtomicUsize::new(0),
                ipc_connections: AtomicUsize::new(0),
            }),
        );
    });

    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    handler.join().unwrap();
    response
}

fn split_http_response(response: &[u8]) -> (&str, &[u8]) {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    (
        std::str::from_utf8(&response[..separator + 2]).unwrap(),
        &response[separator + 4..],
    )
}

/// The Unix liveness probe must accept a live listener and refuse a stale
/// socket file, on a throwaway path that never touches the real runtime.
/// Covered: no socket file (ENOENT), a live listener, and a socket file
/// with no listener behind it (ECONNREFUSED). The throwaway directory is
/// [`crate::transport::ShortTempDir`], a canonical-`/tmp` short path whose
/// Drop-based cleanup runs even when an assertion fails.
#[cfg(unix)]
#[test]
fn unix_liveness_probe_distinguishes_live_and_stale_sockets() {
    use std::os::unix::io::IntoRawFd as _;

    let temp = crate::transport::ShortTempDir::new("probe");

    // No socket file at the path: definitively not alive.
    let missing = temp.path().join("missing.sock");
    let missing_name = missing
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    assert!(!runtime_server_is_alive_for(&missing_name));

    // A live listener behind the path: must probe as alive.
    let socket = temp.path().join("probe.sock");
    let name = socket
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    let listener = interprocess::local_socket::ListenerOptions::new()
        .name(name.clone())
        .create_sync()
        .unwrap();
    assert!(runtime_server_is_alive_for(&name));
    drop(listener);

    // A socket file with no listener behind it (a crashed Runtime's
    // leftover): refuses the probe and must not count as alive.
    let stale = temp.path().join("stale.sock");
    let stale_listener = std::os::unix::net::UnixListener::bind(&stale).unwrap();
    let fd = stale_listener.into_raw_fd();
    unsafe {
        libc::close(fd);
    }
    assert!(stale.exists(), "stale socket file must remain on disk");
    let stale_name = stale
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    assert!(!runtime_server_is_alive_for(&stale_name));
}

/// An inconclusive probe (any connect error other than missing/refused)
/// must be surfaced as `Uncertain` with the original error retained, and
/// the conservative [`runtime_server_is_alive_for`] bool must still read
/// it as alive so a live Runtime's socket is never treated as stale.
#[cfg(unix)]
#[test]
fn uncertain_probe_error_is_retained_and_counts_as_alive() {
    let temp = crate::transport::ShortTempDir::new("probe-uncertain");

    // A path whose parent is a regular file makes connect fail with
    // ENOTDIR — neither "no socket file" nor "connection refused", so the
    // probe must stay Uncertain instead of collapsing into a boolean.
    let blocker = temp.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let name = blocker
        .join("runtime.sock")
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    match probe_runtime_name(&name) {
        ProbeOutcome::Uncertain(error) => {
            assert_eq!(error.kind(), ErrorKind::NotADirectory);
        }
        outcome => panic!("expected Uncertain, got {outcome:?}"),
    }
    assert!(
        runtime_server_is_alive_for(&name),
        "an uncertain probe must still count as alive for the current endpoint"
    );

    // Missing and refused stay definitively dead, so startup and stale
    // cleanup treat them as absent.
    let missing = temp.path().join("missing.sock");
    let missing_name = missing
        .to_fs_name::<interprocess::local_socket::GenericFilePath>()
        .unwrap();
    assert!(matches!(
        probe_runtime_name(&missing_name),
        ProbeOutcome::Dead
    ));
    assert!(!runtime_server_is_alive_for(&missing_name));
}
