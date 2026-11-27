use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};

use crate::{
    hooks::http_fallback::{
        HookEndpointState, HookHttpPayload, MAX_HOOK_HTTP_BODY_BYTES, hook_endpoint_file_path,
        read_hook_endpoint_state,
    },
    protocol::validate_hook_request,
};

use super::{
    RuntimeState,
    http::{ParsedHttpRequest, write_http},
};

pub(super) struct HookEndpointGuard {
    path: PathBuf,
    pid: u32,
}

impl HookEndpointGuard {
    pub(super) fn publish(addr: SocketAddr, token: &str) -> Result<Self> {
        let path = hook_endpoint_file_path()?;
        let pid = std::process::id();
        let record = HookEndpointState {
            url: format!("http://{addr}/api/hooks"),
            token: token.to_owned(),
            pid,
        };
        let parent = path
            .parent()
            .context("hook endpoint state path must have a parent directory")?;
        create_private_dir(parent)?;

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_path = parent.join(format!(".hook-endpoint.{pid}.{nanos}.tmp"));
        let bytes =
            serde_json::to_vec(&record).context("failed to serialize hook endpoint record")?;

        let write_result = (|| -> std::io::Result<()> {
            let mut file = open_private_temp_file(&temp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            if path.exists() {
                let _ = fs::remove_file(&path);
            }
            fs::rename(&temp_path, &path)
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error).with_context(|| {
                format!("failed to write hook endpoint state {}", path.display())
            });
        }

        Ok(Self { path, pid })
    }
}

impl Drop for HookEndpointGuard {
    fn drop(&mut self) {
        if let Ok(record) = read_hook_endpoint_state(&self.path)
            && record.pid == self.pid
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to secure {}", dir.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))
}

#[cfg(unix)]
fn open_private_temp_file(path: &std::path::Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private_temp_file(path: &std::path::Path) -> std::io::Result<fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

pub(super) fn generate_hook_token() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("failed to generate runtime hook token")?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        token.push(HEX[usize::from(byte >> 4)] as char);
        token.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    Ok(token)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

pub(super) fn handle_hook_http_post(
    stream: &mut TcpStream,
    state: &RuntimeState,
    request: ParsedHttpRequest,
) {
    if request.origin.is_some() {
        let _ = write_http(
            stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            "browser origin is not allowed on hook endpoint",
        );
        return;
    }
    let token_valid = !state.hook_token.is_empty()
        && request.hook_token.as_deref().is_some_and(|provided| {
            constant_time_eq(provided.as_bytes(), state.hook_token.as_bytes())
        });
    if !token_valid {
        let _ = write_http(
            stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            "missing or invalid hook token",
        );
        return;
    }
    if let Some(error) = request.body_error {
        let status = if error.contains("exceeds") || error.contains("limit") {
            "413 Payload Too Large"
        } else {
            "400 Bad Request"
        };
        let _ = write_http(stream, status, "text/plain; charset=utf-8", &error);
        return;
    }
    if request.body.is_empty() || request.body.len() > MAX_HOOK_HTTP_BODY_BYTES {
        let _ = write_http(
            stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            "missing or oversized hook JSON body",
        );
        return;
    }
    let payload: HookHttpPayload = match serde_json::from_slice(&request.body) {
        Ok(payload) => payload,
        Err(error) => {
            let _ = write_http(
                stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                &format!("invalid hook JSON payload: {error}"),
            );
            return;
        }
    };
    if let Err(error) = validate_hook_request(&payload.provider_session_id, &payload.event) {
        let _ = write_http(
            stream,
            "400 Bad Request",
            "text/plain; charset=utf-8",
            &format!("{error:#}"),
        );
        return;
    }
    match state
        .host
        .apply_hook_event(payload.worker, &payload.provider_session_id, payload.event)
    {
        Ok(accepted) => {
            let body = format!(r#"{{"accepted":{accepted}}}"#);
            let _ = write_http(stream, "200 OK", "application/json", &body);
        }
        Err(error) => {
            let _ = write_http(
                stream,
                "500 Internal Server Error",
                "text/plain; charset=utf-8",
                &format!("{error:#}"),
            );
        }
    }
}
