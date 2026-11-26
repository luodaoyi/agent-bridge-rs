use super::*;

/// One encoded WebUI events frame plus cursor commits that become durable only
/// after the frame is successfully sent.
#[derive(Debug)]
pub(crate) struct WebEventsFramePlan {
    pub(crate) message: WebEventsMessage,
    /// Exclusive byte cursors to store after this frame is sent.
    pub(crate) cursor_commits: HashMap<String, u64>,
    /// Cursor map keys to drop after this frame is sent (closed sessions).
    pub(crate) cursor_drops: Vec<String>,
}

/// Strip heavy terminal snapshots from session metadata for `/api/events`.
/// Reset terminal entries carry the authoritative ANSI snapshot.
pub(super) fn web_events_session_view(mut state: SessionState) -> SessionState {
    state.screen = None;
    state.screen_ansi_base64 = String::new();
    state
}

fn message_json_len(message: &WebEventsMessage) -> usize {
    serde_json::to_vec(message)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

/// One planned terminal payload plus an optional durable cursor commit for its
/// final piece (`session` → exclusive PTY cursor).
type PlannedTerminal = (TerminalStreamEntry, Option<(String, u64)>);

pub(super) fn pack_web_events_frames(
    sessions_view: Vec<SessionState>,
    terminal_entries: Vec<PlannedTerminal>,
    cursor_drops: Vec<String>,
    max_message_bytes: usize,
) -> Result<Vec<WebEventsFramePlan>> {
    let max_message_bytes = max_message_bytes.max(1);
    let sessions_only = WebEventsMessage::sessions(sessions_view.clone(), Vec::new());
    let sessions_only_len = message_json_len(&sessions_only);
    if sessions_only_len > max_message_bytes {
        bail!(
            "web events sessions metadata exceeds max_message_bytes ({sessions_only_len} > {max_message_bytes})"
        );
    }

    // Expand every terminal entry into pieces that each serialize under the bound
    // when paired alone with sessions metadata.
    let mut expanded: Vec<PlannedTerminal> = Vec::new();
    for (entry, commit) in terminal_entries {
        expanded.extend(split_terminal_entry_to_fit(
            entry,
            commit,
            &sessions_view,
            max_message_bytes,
        )?);
    }

    let mut frames: Vec<WebEventsFramePlan> = Vec::new();
    let mut terminals: Vec<TerminalStreamEntry> = Vec::new();
    let mut commits: HashMap<String, u64> = HashMap::new();
    let mut drops_for_first = cursor_drops;

    let flush = |terminals: &mut Vec<TerminalStreamEntry>,
                 commits: &mut HashMap<String, u64>,
                 drops: &mut Vec<String>,
                 sessions_view: &Vec<SessionState>,
                 frames: &mut Vec<WebEventsFramePlan>| {
        if terminals.is_empty() && commits.is_empty() && drops.is_empty() && !frames.is_empty() {
            return;
        }
        let message = WebEventsMessage::sessions(sessions_view.clone(), std::mem::take(terminals));
        debug_assert!(message_json_len(&message) <= max_message_bytes);
        frames.push(WebEventsFramePlan {
            message,
            cursor_commits: std::mem::take(commits),
            cursor_drops: std::mem::take(drops),
        });
    };

    if expanded.is_empty() {
        frames.push(WebEventsFramePlan {
            message: sessions_only,
            cursor_commits: HashMap::new(),
            cursor_drops: drops_for_first,
        });
        return Ok(frames);
    }

    for (entry, commit) in expanded {
        let mut probe_terminals = terminals.clone();
        probe_terminals.push(entry.clone());
        let probe = WebEventsMessage::sessions(sessions_view.clone(), probe_terminals);
        if !terminals.is_empty() && message_json_len(&probe) > max_message_bytes {
            flush(
                &mut terminals,
                &mut commits,
                &mut drops_for_first,
                &sessions_view,
                &mut frames,
            );
        }
        // After split_terminal_entry_to_fit, a single entry must fit alone.
        let alone = WebEventsMessage::sessions(sessions_view.clone(), vec![entry.clone()]);
        if message_json_len(&alone) > max_message_bytes {
            bail!("web events terminal chunk still exceeds max_message_bytes after split");
        }
        terminals.push(entry);
        if let Some((session, cursor)) = commit {
            commits.insert(session, cursor);
        }
    }

    if !terminals.is_empty()
        || !commits.is_empty()
        || !drops_for_first.is_empty()
        || frames.is_empty()
    {
        flush(
            &mut terminals,
            &mut commits,
            &mut drops_for_first,
            &sessions_view,
            &mut frames,
        );
    }

    if frames.is_empty() {
        frames.push(WebEventsFramePlan {
            message: WebEventsMessage::sessions(sessions_view, Vec::new()),
            cursor_commits: HashMap::new(),
            cursor_drops: Vec::new(),
        });
    }
    for frame in &frames {
        let len = message_json_len(&frame.message);
        if len > max_message_bytes {
            bail!("web events frame exceeds max_message_bytes ({len} > {max_message_bytes})");
        }
    }
    Ok(frames)
}

fn terminal_entry_message_len(
    sessions_view: &[SessionState],
    entry: &TerminalStreamEntry,
) -> usize {
    message_json_len(&WebEventsMessage::sessions(
        sessions_view.to_vec(),
        vec![entry.clone()],
    ))
}

/// Split one terminal entry into ordered pieces that each serialize to
/// `<= max_message_bytes` with the sessions metadata. Reset snapshots: first
/// piece `reset=true`, continuations `reset=false`; PTY cursor commit only on
/// the final piece. Raw deltas preserve byte cursor progression.
fn split_terminal_entry_to_fit(
    entry: TerminalStreamEntry,
    commit: Option<(String, u64)>,
    sessions_view: &[SessionState],
    max_message_bytes: usize,
) -> Result<Vec<PlannedTerminal>> {
    if terminal_entry_message_len(sessions_view, &entry) <= max_message_bytes {
        return Ok(vec![(entry, commit)]);
    }

    let raw = BASE64
        .decode(&entry.data_base64)
        .context("terminal data_base64 is invalid")?;
    if raw.is_empty() {
        bail!("web events terminal entry exceeds max_message_bytes with empty payload");
    }

    let mut pieces: Vec<PlannedTerminal> = Vec::new();
    let mut offset = 0_usize;
    let mut stream_cursor = entry.cursor;
    let original_reset = entry.reset;
    let pty_commit_cursor = entry.next_cursor;

    while offset < raw.len() {
        let remaining = raw.len() - offset;
        // Binary-search the largest raw prefix that still fits in one frame.
        let mut lo = 1_usize;
        let mut hi = remaining;
        let mut best = 0_usize;
        while lo <= hi {
            let mid = (lo + hi) / 2;
            let candidate = TerminalStreamEntry {
                session: entry.session.clone(),
                reset: original_reset && offset == 0,
                cursor: stream_cursor,
                next_cursor: stream_cursor.saturating_add(mid as u64),
                data_base64: BASE64.encode(&raw[offset..offset + mid]),
            };
            if terminal_entry_message_len(sessions_view, &candidate) <= max_message_bytes {
                best = mid;
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        if best == 0 {
            bail!(
                "web events cannot fit any terminal payload bytes within max_message_bytes ({max_message_bytes})"
            );
        }

        let is_last = offset + best >= raw.len();
        let next_cursor = if is_last {
            // Final piece reports the original exclusive end (PTY or snapshot end).
            pty_commit_cursor
        } else {
            stream_cursor.saturating_add(best as u64)
        };
        let piece = TerminalStreamEntry {
            session: entry.session.clone(),
            reset: original_reset && offset == 0,
            cursor: stream_cursor,
            next_cursor,
            data_base64: BASE64.encode(&raw[offset..offset + best]),
        };
        // Durable PTY cursor advances only after the final chunk is sent.
        let piece_commit = if is_last { commit.clone() } else { None };
        pieces.push((piece, piece_commit));
        stream_cursor = next_cursor;
        offset += best;
    }

    Ok(pieces)
}
