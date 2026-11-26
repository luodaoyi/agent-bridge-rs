use super::*;

pub(super) fn set_phase(inner: &mut SessionInner, phase: SessionPhase, now: u64) {
    if inner.phase != phase {
        inner.phase = phase;
        inner.phase_changed_at_ms = now;
    }
}

pub(super) fn phase_is_safe_for_orphan_cleanup(phase: SessionPhase) -> bool {
    phase == SessionPhase::Idle || phase_is_terminal(phase)
}

pub(super) fn parse_duration_env(name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value
        .to_str()
        .with_context(|| format!("{name} must be valid Unicode"))?;
    let seconds = value
        .parse::<u64>()
        .with_context(|| format!("{name} must be an integer number of seconds"))?;
    if !(min..=max).contains(&seconds) {
        bail!("{name} must be between {min} and {max} seconds");
    }
    Ok(seconds)
}

pub(super) fn generate_provider_session_id() -> Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0_u8; PROVIDER_SESSION_UUID_BYTES];
    getrandom::fill(&mut bytes).context("failed to generate the Grok provider session ID")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut session_id = String::with_capacity(36);
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            session_id.push('-');
        }
        session_id.push(HEX[usize::from(byte >> 4)] as char);
        session_id.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    Ok(session_id)
}

pub(super) fn finalize_session(inner: &mut SessionInner, shutdown: bool) -> bool {
    let Some(phase) = completed_phase(
        inner.phase,
        inner.process_done,
        inner.reader_done,
        shutdown,
        inner.error.is_some(),
        inner.exit_code,
    ) else {
        return false;
    };
    let now = now_millis();
    set_phase(inner, phase, now);
    // The record is fully closed: clear the externally visible process id so a
    // naturally exited session never exposes a stale pid. The immutable
    // Session::scope_id keeps addressing the owned scope if a later close
    // still needs to verify or terminate it.
    inner.process_id = None;
    inner.updated_at_ms = now;
    true
}

pub(super) fn completed_phase(
    current: SessionPhase,
    process_done: bool,
    reader_done: bool,
    shutdown: bool,
    failed: bool,
    exit_code: Option<u32>,
) -> Option<SessionPhase> {
    if phase_is_terminal(current) {
        return None;
    }
    if !process_done {
        return None;
    }
    if shutdown {
        // An explicit close marks the scope terminated once the process is
        // verified gone; the PTY master is released right after so the reader
        // edge unblocks. Missing reader EOF does not keep the record half-open.
        Some(SessionPhase::Stopped)
    } else if !reader_done {
        None
    } else if failed || exit_code != Some(0) {
        Some(SessionPhase::Failed)
    } else {
        Some(SessionPhase::Exited)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn phase_after_output(
    worker: WorkerKind,
    current: SessionPhase,
    title: Option<&str>,
    title_updated: bool,
    hook_activity: HookActivity,
    process_done: bool,
    failed: bool,
    shutdown: bool,
) -> SessionPhase {
    if phase_is_terminal(current) || process_done || failed || shutdown {
        current
    } else if title_updated && let Some(phase) = phase_from_title(worker, title) {
        phase
    } else if hook_activity == HookActivity::Done {
        SessionPhase::Idle
    } else if hook_activity == HookActivity::Waiting || current == SessionPhase::Starting {
        SessionPhase::Running
    } else {
        current
    }
}

pub(super) fn record_error(inner: &mut SessionInner, message: String) {
    match &mut inner.error {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(&message);
        }
        None => inner.error = Some(message),
    }
    inner.updated_at_ms = now_millis();
}

pub(super) fn wait_satisfied(inner: &mut SessionInner, condition: WaitCondition) -> bool {
    match condition {
        WaitCondition::Exit => phase_is_terminal(inner.phase),
        WaitCondition::TuiIdle => {
            if inner.error.is_some() {
                return false;
            }
            if inner.phase == SessionPhase::Idle {
                return true;
            }
            let quiet = now_millis().saturating_sub(
                inner
                    .last_output_at_ms
                    .unwrap_or(inner.updated_at_ms)
                    .max(inner.updated_at_ms),
            ) >= QUIET_IDLE_MILLISECONDS;
            if inner.phase == SessionPhase::Running
                && inner.title.is_none()
                && matches!(
                    inner.hook.activity,
                    HookActivity::Unknown | HookActivity::Working
                )
                && quiet
            {
                let now = now_millis();
                set_phase(inner, SessionPhase::Idle, now);
                inner.updated_at_ms = now;
                return true;
            }
            false
        }
    }
}

pub(super) fn phase_is_active(phase: SessionPhase) -> bool {
    matches!(
        phase,
        SessionPhase::Starting | SessionPhase::Running | SessionPhase::Idle
    )
}

pub(super) fn phase_is_terminal(phase: SessionPhase) -> bool {
    matches!(
        phase,
        SessionPhase::Exited | SessionPhase::Failed | SessionPhase::Stopped
    )
}

/// Map the raw hook activity to what a client should observe, resolving
/// phase/hook conflicts by fixed priority: a terminal phase overrides a stale
/// in-flight hook (the hook stream died with the process), and an idle TUI
/// resolves a stale working hook to done.
pub(super) fn projected_activity(phase: SessionPhase, hook_activity: HookActivity) -> HookActivity {
    if phase_is_terminal(phase) {
        if matches!(hook_activity, HookActivity::Working | HookActivity::Waiting) {
            HookActivity::Unknown
        } else {
            hook_activity
        }
    } else if phase == SessionPhase::Idle && hook_activity == HookActivity::Working {
        HookActivity::Done
    } else {
        hook_activity
    }
}

pub(super) fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let canonical = normalize_platform_path(
        path.canonicalize()
            .with_context(|| format!("failed to resolve working directory: {}", path.display()))?,
    );
    if !canonical.is_dir() {
        bail!(
            "working directory is not a directory: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

pub(super) fn ensure_allowed_root(cwd: &Path) -> Result<()> {
    let Some(value) = env::var_os("AGENT_BRIDGE_ALLOWED_ROOTS") else {
        return Ok(());
    };
    let mut roots = Vec::new();
    for root in env::split_paths(&value) {
        roots.push(normalize_platform_path(root.canonicalize().with_context(
            || format!("failed to resolve allowed root: {}", root.display()),
        )?));
    }
    if roots.iter().any(|root| cwd.starts_with(root)) {
        Ok(())
    } else {
        bail!(
            "working directory is outside AGENT_BRIDGE_ALLOWED_ROOTS: {}",
            cwd.display()
        )
    }
}

#[cfg(windows)]
pub(super) fn normalize_platform_path(path: PathBuf) -> PathBuf {
    let display = path.to_string_lossy();
    if let Some(rest) = display.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = display.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(windows))]
pub(super) fn normalize_platform_path(path: PathBuf) -> PathBuf {
    path
}

pub(super) fn validate_prompt(prompt: Option<&str>) -> Result<()> {
    if let Some(prompt) = prompt {
        if prompt.trim().is_empty() {
            bail!("prompt must not be empty");
        }
        if prompt.len() > 128 * 1024 {
            bail!("prompt exceeds the 128 KiB limit");
        }
    }
    Ok(())
}

pub(super) fn validate_model(model: Option<&str>) -> Result<()> {
    if let Some(model) = model {
        if model.is_empty() || model.len() > 256 {
            bail!("model must contain between 1 and 256 bytes");
        }
        if !model
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:/".contains(character))
        {
            bail!("model contains unsupported characters");
        }
    }
    Ok(())
}

pub(super) fn raw_input_starts_turn(data: &[u8]) -> bool {
    data.iter().any(|byte| matches!(*byte, b'\r' | b'\n'))
}

pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
