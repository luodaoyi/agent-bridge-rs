use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::protocol::{HookEvent, WorkerKind, validate_hook_request};

pub(crate) const HOOK_TOKEN_HEADER: &str = "X-Agent-Bridge-Hook-Token";
pub(crate) const HOOK_ENDPOINT_ENV: &str = "AGENT_BRIDGE_HOOK_ENDPOINT_FILE";
pub(crate) const HOOK_HTTP_ROUTE: &str = "/api/hooks";
pub(crate) const MAX_HOOK_HTTP_BODY_BYTES: usize = 64 * 1024;
const MAX_ENDPOINT_FILE_BYTES: u64 = 8 * 1024;
const HOOK_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const HOOK_HTTP_IO_TIMEOUT: Duration = Duration::from_millis(1_000);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookEndpointState {
    pub(crate) url: String,
    pub(crate) token: String,
    pub(crate) pid: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookHttpPayload {
    pub(crate) worker: WorkerKind,
    pub(crate) provider_session_id: String,
    pub(crate) event: HookEvent,
}

#[derive(Deserialize)]
struct HookHttpResponse {
    #[serde(default)]
    accepted: bool,
}

pub(crate) fn hook_endpoint_file_path() -> Result<PathBuf> {
    if let Some(override_path) = env::var_os(HOOK_ENDPOINT_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(override_path));
    }
    #[cfg(unix)]
    {
        Ok(crate::transport::runtime_dir()?.join("hook-endpoint.json"))
    }
    #[cfg(windows)]
    {
        let home = env::var_os("USERPROFILE")
            .filter(|value| !value.is_empty())
            .or_else(|| env::var_os("HOME").filter(|value| !value.is_empty()))
            .context("USERPROFILE and HOME are not set for hook endpoint state")?;
        Ok(PathBuf::from(home)
            .join(".agent-bridge")
            .join("hook-endpoint.json"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(env::temp_dir().join("agent-bridge-hook-endpoint.json"))
    }
}

pub(crate) fn parse_loopback_hook_url(url: &str) -> Result<(SocketAddr, String, String)> {
    let rest = url
        .strip_prefix("http://")
        .context("hook endpoint URL must use http://")?;
    let slash_index = rest
        .find('/')
        .context("hook endpoint URL must contain an explicit path")?;
    let authority = &rest[..slash_index];
    let path = &rest[slash_index..];
    if path != HOOK_HTTP_ROUTE {
        bail!("hook endpoint URL path must be {HOOK_HTTP_ROUTE}");
    }
    let addr = authority
        .parse::<SocketAddr>()
        .context("hook endpoint authority must be a literal IP:port")?;
    if !addr.ip().is_loopback() {
        bail!("hook endpoint address {addr} is not loopback");
    }
    Ok((addr, authority.to_owned(), path.to_owned()))
}

pub(crate) fn read_hook_endpoint_state(path: &Path) -> Result<HookEndpointState> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat hook endpoint file {}", path.display()))?;
    if metadata.len() > MAX_ENDPOINT_FILE_BYTES {
        bail!("hook endpoint file {} exceeds size limit", path.display());
    }
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read hook endpoint file {}", path.display()))?;
    let record: HookEndpointState = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse hook endpoint file {}", path.display()))?;
    validate_hook_token(&record.token)?;
    let _ = parse_loopback_hook_url(&record.url)?;
    Ok(record)
}

pub(crate) fn validate_hook_token(token: &str) -> Result<()> {
    if token.is_empty()
        || token.len() > 256
        || !token.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        bail!("hook token must be 1-256 ASCII alphanumeric bytes");
    }
    Ok(())
}

pub(crate) fn send_via_loopback_http_from(
    endpoint_path: &Path,
    worker: WorkerKind,
    provider_session_id: &str,
    event: &HookEvent,
) -> Result<bool> {
    validate_hook_request(provider_session_id, event)?;
    let record = read_hook_endpoint_state(endpoint_path)?;
    let (addr, authority, route) = parse_loopback_hook_url(&record.url)?;
    let payload = HookHttpPayload {
        worker,
        provider_session_id: provider_session_id.to_owned(),
        event: event.clone(),
    };
    let body = serde_json::to_vec(&payload).context("failed to encode hook HTTP payload")?;
    if body.len() > MAX_HOOK_HTTP_BODY_BYTES {
        bail!("hook HTTP payload exceeds {MAX_HOOK_HTTP_BODY_BYTES} bytes");
    }

    let mut stream = TcpStream::connect_timeout(&addr, HOOK_HTTP_CONNECT_TIMEOUT)
        .with_context(|| format!("failed to connect to loopback hook endpoint {addr}"))?;
    let _ = stream.set_read_timeout(Some(HOOK_HTTP_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HOOK_HTTP_IO_TIMEOUT));

    let headers = format!(
        "POST {route} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{HOOK_TOKEN_HEADER}: {}\r\nConnection: close\r\n\r\n",
        body.len(),
        record.token
    );
    let mut request_bytes = Vec::with_capacity(headers.len() + body.len());
    request_bytes.extend_from_slice(headers.as_bytes());
    request_bytes.extend_from_slice(&body);
    stream.write_all(&request_bytes)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
        bail!("loopback hook endpoint returned {}", status_line.trim());
    }
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    let mut response_bytes = Vec::new();
    reader
        .take(MAX_ENDPOINT_FILE_BYTES)
        .read_to_end(&mut response_bytes)?;
    if response_bytes.is_empty() {
        return Ok(true);
    }
    let parsed: HookHttpResponse =
        serde_json::from_slice(&response_bytes).context("invalid hook HTTP JSON response")?;
    Ok(parsed.accepted)
}
