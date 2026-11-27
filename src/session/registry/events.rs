use super::*;

impl SessionHost {
    pub(crate) fn list_web(&self) -> Result<Vec<SessionState>> {
        self.list()
    }

    fn list_web_lightweight(&self) -> Result<Vec<(Arc<Session>, SessionState)>> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow::anyhow!("session registry lock was poisoned"))?;
        let mut items = registry
            .sessions
            .values()
            .map(|session| Ok((Arc::clone(session), session.state_without_screen()?)))
            .collect::<Result<Vec<_>>>()?;
        items.sort_by_key(|(_, state)| state.created_at_ms);
        Ok(items)
    }

    /// Plan ordered WebUI events frames without mutating the connection cursor map.
    ///
    /// - `force_reset`: one `reset=true` ANSI snapshot per session (upgrade / resync).
    /// - Otherwise drain raw PTY bytes only up to each session's **frozen**
    ///   `last_cursor` from this batch so a live producer cannot make the plan chase
    ///   forever.
    /// - Lightweight metadata queries skip heavy `screen` / `screen_ansi_base64`
    ///   formatting; full ANSI screen snapshots are fetched lazily only for sessions
    ///   requiring a reset.
    /// - Terminal entries may span multiple frames under `max_message_bytes`.
    /// - Cursor commits/drops are returned per frame and must be applied only after
    ///   that frame is successfully sent.
    pub(crate) fn plan_web_events(
        &self,
        cursors: &HashMap<String, u64>,
        force_reset: bool,
        max_message_bytes: usize,
    ) -> Result<Vec<WebEventsFramePlan>> {
        let mut items = self.list_web_lightweight()?;
        let active: HashSet<String> = items
            .iter()
            .map(|(_, state)| state.session.clone())
            .collect();
        let cursor_drops: Vec<String> = cursors
            .keys()
            .filter(|session| !active.contains(*session))
            .cloned()
            .collect();

        let mut terminal_entries: Vec<(TerminalStreamEntry, Option<(String, u64)>)> = Vec::new();
        for (session, state) in &mut items {
            if force_reset || !cursors.contains_key(&state.session) {
                let mut full = session.state()?;
                let entry = TerminalStreamEntry::reset_snapshot(&full);
                let commit = Some((full.session.clone(), full.last_cursor));
                full.screen = None;
                full.screen_ansi_base64 = String::new();
                *state = full;
                terminal_entries.push((entry, commit));
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
                let mut full = session.state()?;
                let entry = TerminalStreamEntry::reset_snapshot(&full);
                let commit = Some((full.session.clone(), full.last_cursor));
                full.screen = None;
                full.screen_ansi_base64 = String::new();
                *state = full;
                terminal_entries.push((entry, commit));
                continue;
            }
            if cursor == freeze_end {
                continue;
            }

            while cursor < freeze_end {
                let limit = usize::try_from(freeze_end - cursor)
                    .unwrap_or(MAX_READ_BYTES)
                    .clamp(1, MAX_READ_BYTES);
                let read = match session.read(cursor, limit, 0) {
                    Ok(read) => read,
                    Err(_) => {
                        let mut full = session.state()?;
                        let entry = TerminalStreamEntry::reset_snapshot(&full);
                        let commit = Some((full.session.clone(), full.last_cursor));
                        full.screen = None;
                        full.screen_ansi_base64 = String::new();
                        *state = full;
                        terminal_entries.push((entry, commit));
                        break;
                    }
                };
                if read.truncated {
                    let mut full = session.state()?;
                    let entry = TerminalStreamEntry::reset_snapshot(&full);
                    let commit = Some((full.session.clone(), full.last_cursor));
                    full.screen = None;
                    full.screen_ansi_base64 = String::new();
                    *state = full;
                    terminal_entries.push((entry, commit));
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

        let sessions_view: Vec<SessionState> = items.into_iter().map(|(_, state)| state).collect();
        pack_web_events_frames(
            sessions_view,
            terminal_entries,
            cursor_drops,
            max_message_bytes,
        )
    }
}
