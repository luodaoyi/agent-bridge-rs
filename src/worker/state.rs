use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::protocol::WorkerKind;

pub(crate) fn ensure_worker_state_dir_writable(
    worker: WorkerKind,
    cwd: &Path,
    provider_session_id: &str,
) -> Result<Option<PathBuf>> {
    let Some(state_dir) = worker_state_dir(worker, cwd) else {
        return Ok(None);
    };
    ensure_worker_state_dir_writable_at(worker, &state_dir, provider_session_id)?;
    Ok(Some(state_dir))
}

pub(crate) fn worker_state_dir(worker: WorkerKind, cwd: &Path) -> Option<PathBuf> {
    let (env_name, default_dir) = match worker {
        WorkerKind::Grok => ("GROK_HOME", ".grok"),
        WorkerKind::Codex => ("CODEX_HOME", ".codex"),
    };
    let state_dir = worker_state_dir_from(
        env::var_os(env_name),
        env::var_os("HOME"),
        env::var_os("USERPROFILE"),
        cfg!(windows),
        default_dir,
    )?;
    Some(resolve_state_dir_from_cwd(cwd, state_dir))
}

fn resolve_state_dir_from_cwd(cwd: &Path, state_dir: PathBuf) -> PathBuf {
    if state_dir.is_absolute() {
        state_dir
    } else {
        cwd.join(state_dir)
    }
}

pub(crate) fn worker_state_dir_from(
    worker_home: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
    windows: bool,
    default_dir_name: &str,
) -> Option<PathBuf> {
    non_empty_path(worker_home).or_else(|| {
        let home = if windows {
            non_empty_path(user_profile).or_else(|| non_empty_path(home))
        } else {
            non_empty_path(home).or_else(|| non_empty_path(user_profile))
        }?;
        Some(home.join(default_dir_name))
    })
}

fn non_empty_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

pub(crate) fn ensure_worker_state_dir_writable_at(
    worker: WorkerKind,
    state_dir: &Path,
    provider_session_id: &str,
) -> Result<()> {
    let label = match worker {
        WorkerKind::Grok => "Grok",
        WorkerKind::Codex => "Codex",
    };
    let context = format!(
        "{label} state directory is not writable: {}. The Runtime may have inherited a filesystem sandbox; start agent-bridge server outside that sandbox and retry",
        state_dir.display()
    );
    fs::create_dir_all(state_dir).with_context(|| context.clone())?;
    let probe_path = state_dir.join(format!(".agent-bridge-write-probe-{provider_session_id}"));
    let mut created = false;
    let probe_result = (|| -> std::io::Result<()> {
        let mut probe = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)?;
        created = true;
        probe.write_all(b"agent-bridge")?;
        probe.flush()
    })();
    let cleanup_result = if created {
        match fs::remove_file(&probe_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    probe_result.with_context(|| context.clone())?;
    cleanup_result.with_context(|| {
        format!(
            "failed to remove {label} state probe: {}",
            probe_path.display()
        )
    })?;
    Ok(())
}
