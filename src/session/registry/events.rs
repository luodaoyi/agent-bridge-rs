use super::*;

impl SessionHost {
    pub(crate) fn list_web(&self) -> Result<Vec<SessionState>> {
        self.list()
    }

    /// Plan ordered WebUI events frames without mutating the connection cursor map.
    ///
    /// - `force_reset`: one `reset=true` ANSI snapshot per session (upgrade / resync).
    /// - Otherwise drain raw PTY bytes only up to each session's **frozen**
    ///   `last_cursor` from this batch so a live producer cannot make the plan chase
    ///   forever.
    /// - Terminal entries may span multiple frames under `max_message_bytes`.
    /// - Cursor commits/drops are returned per frame and must be applied only after
    ///   that frame is successfully sent.
    /// - Session metadata in the JSON omits heavy `screen` / `screen_ansi_base64`;
    ///   reset terminal entries remain the authoritative ANSI snapshot.
    pub(crate) fn plan_web_events(
        &self,
        cursors: &HashMap<String, u64>,
        force_reset: bool,
        max_message_bytes: usize,
    ) -> Result<Vec<WebEventsFramePlan>> {
        let sessions = self.list_web()?;
        let active: HashSet<&str> = sessions
            .iter()
            .map(|state| state.session.as_str())
            .collect();
        let cursor_drops: Vec<String> = cursors
            .keys()
            .filter(|session| !active.contains(session.as_str()))
            .cloned()
            .collect();

        let mut terminal_entries: Vec<(TerminalStreamEntry, Option<(String, u64)>)> = Vec::new();
        for state in &sessions {
            if force_reset || !cursors.contains_key(&state.session) {
                terminal_entries.push((
                    TerminalStreamEntry::reset_snapshot(state),
                    Some((state.session.clone(), state.last_cursor)),
                ));
                continue;
            }

            let mut cursor = cursors
                .get(&state.session)
                .copied()
                .unwrap_or(state.last_cursor);
            // Freeze the exclusive end for this batch so continuous output cannot
            // unbounded-chase the live cursor inside one plan call.
            let freeze_end = state.last_cursor;
            if cursor > freeze_end {
                terminal_entries.push((
                    TerminalStreamEntry::reset_snapshot(state),
                    Some((state.session.clone(), freeze_end)),
                ));
                continue;
            }
            if cursor == freeze_end {
                continue;
            }

            while cursor < freeze_end {
                let limit = usize::try_from(freeze_end - cursor)
                    .unwrap_or(MAX_READ_BYTES)
                    .clamp(1, MAX_READ_BYTES);
                let read = match self.read(&state.session, cursor, limit, 0) {
                    Ok(read) => read,
                    Err(_) => {
                        terminal_entries.push((
                            TerminalStreamEntry::reset_snapshot(state),
                            Some((state.session.clone(), freeze_end)),
                        ));
                        break;
                    }
                };
                if read.truncated {
                    terminal_entries.push((
                        TerminalStreamEntry::reset_snapshot(state),
                        Some((state.session.clone(), freeze_end)),
                    ));
                    break;
                }
                if read.next_cursor == read.cursor {
                    break;
                }
                // Never emit past the freeze point even if the live stream advanced.
                let capped_next = read.next_cursor.min(freeze_end);
                if capped_next <= cursor {
                    break;
                }
                let mut entry = TerminalStreamEntry::delta(&read);
                if capped_next != read.next_cursor {
                    // Re-encode a prefix when the live read overshot the freeze.
                    let raw = BASE64.decode(&read.data_base64).unwrap_or_default();
                    let take = (capped_next - read.cursor) as usize;
                    let take = take.min(raw.len());
                    entry.data_base64 = BASE64.encode(&raw[..take]);
                    entry.next_cursor = read.cursor + take as u64;
                }
                cursor = entry.next_cursor;
                terminal_entries.push((entry, Some((state.session.clone(), cursor))));
            }
        }

        let sessions_view: Vec<SessionState> =
            sessions.into_iter().map(web_events_session_view).collect();
        pack_web_events_frames(
            sessions_view,
            terminal_entries,
            cursor_drops,
            max_message_bytes,
        )
    }
}
