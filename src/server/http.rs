mod listener;
mod origin;
mod request;
mod response;
mod routes;

// Keep the existing server-facing paths, including helpers used only by tests.
#[allow(unused_imports)]
pub(super) use self::{
    listener::{
        WebAddrError, bind_web_ui, bind_web_ui_from, classify_web_ui_bind, parse_web_addr,
        run_web_ui,
    },
    origin::web_origin_allowed,
    request::{ParsedHttpRequest, read_http_line, read_http_request_with_deadline},
    response::{DeadlineWrite, write_http, write_http_bytes, write_tcp_all_with_deadline},
    routes::{
        StaticWebAsset, close_path_segment, handle_web_connection, percent_decode_path_segment,
        static_web_asset,
    },
};
