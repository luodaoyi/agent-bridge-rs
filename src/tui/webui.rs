use anyhow::{Context, Result};

use super::language::Language;

pub(super) fn open_webui(lang: &Language) -> Result<()> {
    use crate::protocol::Request;
    use crate::transport;
    #[cfg(any(windows, unix))]
    use std::process::Command;

    let s = lang.strings();
    let response = transport::call(Request::ServerStatus, true)?;
    if !response.ok {
        anyhow::bail!(s.err_server_not_running);
    }

    match response.result {
        Some(crate::protocol::ResponseResult::ServerInfo(info)) => match info.web_url {
            Some(url) => {
                #[cfg(windows)]
                Command::new("explorer.exe")
                    .arg(&url)
                    .spawn()
                    .context(s.err_spawn_explorer)?;
                #[cfg(target_os = "macos")]
                Command::new("open")
                    .arg(&url)
                    .spawn()
                    .context(s.err_spawn_open)?;
                #[cfg(all(unix, not(target_os = "macos")))]
                Command::new("xdg-open")
                    .arg(&url)
                    .spawn()
                    .context(s.err_spawn_xdg)?;
                Ok(())
            }
            None => anyhow::bail!(s.err_webui_unavailable),
        },
        _ => anyhow::bail!(s.err_unexpected_response),
    }
}
