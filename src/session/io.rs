use super::*;

impl vt100::Callbacks for TitleCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
        self.title_updated = true;
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first_intermediate: Option<u8>,
        second_intermediate: Option<u8>,
        params: &[&[u16]],
        final_character: char,
    ) {
        if first_intermediate.is_some() || second_intermediate.is_some() {
            return;
        }
        let first_param = params.first().and_then(|value| value.first()).copied();
        match (final_character, first_param) {
            ('n', Some(5)) => self.responses.push(b"\x1b[0n".to_vec()),
            ('n', Some(6)) => {
                let (row, column) = screen.cursor_position();
                self.responses
                    .push(format!("\x1b[{};{}R", row + 1, column + 1).into_bytes());
            }
            ('c', None | Some(0)) => self.responses.push(b"\x1b[?1;2c".to_vec()),
            _ => {}
        }
    }
}

impl Session {
    pub(super) fn read(&self, cursor: u64, limit: usize, wait_ms: u64) -> Result<ReadResult> {
        let limit = limit.clamp(1, MAX_READ_BYTES);
        let deadline = Instant::now() + Duration::from_millis(wait_ms.min(300_000));
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if cursor > inner.next_cursor {
            bail!(
                "cursor {cursor} is beyond the latest cursor {}",
                inner.next_cursor
            );
        }
        while cursor == inner.next_cursor && phase_is_active(inner.phase) && wait_ms > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let waited = self
                .changed
                .wait_timeout(inner, remaining)
                .map_err(|_| anyhow::anyhow!("session wait lock was poisoned"))?;
            inner = waited.0;
            if waited.1.timed_out() {
                break;
            }
        }

        let oldest_cursor = inner
            .chunks
            .front()
            .map(|chunk| chunk.start)
            .unwrap_or(inner.next_cursor);
        let actual_cursor = cursor.max(oldest_cursor);
        let mut output = Vec::with_capacity(limit);
        for chunk in &inner.chunks {
            let end = chunk.start + chunk.data.len() as u64;
            if end <= actual_cursor {
                continue;
            }
            let offset = actual_cursor.saturating_sub(chunk.start) as usize;
            let available = &chunk.data[offset.min(chunk.data.len())..];
            let take = available.len().min(limit - output.len());
            output.extend_from_slice(&available[..take]);
            if output.len() == limit {
                break;
            }
        }
        let next_cursor = actual_cursor + output.len() as u64;
        Ok(ReadResult {
            session: inner.session.clone(),
            cursor: actual_cursor,
            next_cursor,
            data_base64: BASE64.encode(&output),
            plain_text: None,
            screen: Some(inner.parser.screen().contents()),
            truncated: cursor < oldest_cursor,
            eof: phase_is_terminal(inner.phase),
        })
    }

    pub(super) fn send(&self, input: String) -> Result<()> {
        if input.is_empty() {
            bail!("input must not be empty");
        }
        // A lone Ctrl-C interrupts the current turn; enqueue it without
        // starting a new one, keeping the bytes sent to the PTY unchanged.
        let starts_turn = !(input.len() == 1 && input.as_bytes()[0] == 0x03);
        let (data, followup) = if input.len() == 1 && input.as_bytes()[0].is_ascii_control() {
            (input.into_bytes(), None)
        } else {
            let worker = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?
                .worker;
            let mut data = Vec::with_capacity(input.len() + 13);
            data.extend_from_slice(b"\x1b[200~");
            data.extend_from_slice(input.as_bytes());
            data.extend_from_slice(b"\x1b[201~");
            match worker {
                WorkerKind::Codex => (data, Some((CODEX_PASTE_SUBMIT_DELAY, b"\r".to_vec()))),
                WorkerKind::Grok => {
                    data.push(b'\r');
                    (data, None)
                }
            }
        };
        self.enqueue_input(data, followup, starts_turn)
    }

    pub(super) fn write_raw(&self, data: Vec<u8>) -> Result<()> {
        if data.is_empty() {
            bail!("terminal data must not be empty");
        }
        if data.len() > MAX_WRITE_BYTES {
            bail!("terminal data exceeds the 64 KiB limit");
        }
        let starts_turn = raw_input_starts_turn(&data);
        self.enqueue_input(data, None, starts_turn)
    }

    pub(super) fn enqueue_input(
        &self,
        data: Vec<u8>,
        followup: Option<(Duration, Vec<u8>)>,
        starts_turn: bool,
    ) -> Result<()> {
        if self.shutdown.load(Ordering::Acquire) {
            bail!("session has already stopped");
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if self.cleanup_claimed.load(Ordering::Acquire)
            || self.cleanup_committed.load(Ordering::Acquire)
        {
            bail!("session cleanup has started");
        }
        if inner.process_done || phase_is_terminal(inner.phase) || inner.error.is_some() {
            bail!("session is not writable");
        }
        let writer_guard = self
            .writer_tx
            .lock()
            .map_err(|_| anyhow::anyhow!("session input lock was poisoned"))?;
        let Some(writer) = writer_guard.as_ref() else {
            bail!("session input channel is closed");
        };
        // Byte budget is checked before any byte is queued; queue-full rejection
        // is always whole-input (never a partial write).
        let data_len = data.len() + followup.as_ref().map_or(0, |(_, data)| data.len());
        if self
            .pending_writer_bytes
            .load(Ordering::Acquire)
            .saturating_add(data_len)
            > WRITER_QUEUE_MAX_BYTES
        {
            bail!("session input queue is full");
        }
        // Count the bytes BEFORE the item becomes visible to the writer thread.
        // The writer only ever subtracts for items it received, and an item only
        // becomes receivable after a successful try_send that follows this
        // increment, so the counter can never underflow and the writer can never
        // subtract before the matching add. On Full/Disconnected the increment
        // is rolled back under the same lock. Ordering: Release on the add
        // pairs with the writer's Acquire release of the same item, but the
        // counter's ordering is not what makes admission correct — the channel
        // itself synchronizes the item — so the plain Acquire on the release
        // path is the minimal sufficient read side (Relaxed would also be
        // sound); no AcqRel is required for count correctness.
        self.pending_writer_bytes
            .fetch_add(data_len, Ordering::Release);
        match writer.try_send(WriterItem {
            data,
            followup,
            budget: data_len,
        }) {
            Ok(()) => {
                let now = now_millis();
                if starts_turn {
                    set_phase(&mut inner, SessionPhase::Running, now);
                    inner.hook.activity = HookActivity::Working;
                    inner.hook.tool_name = None;
                    inner.hook.waiting_reason = None;
                    inner.hook.turn_done = false;
                }
                inner.updated_at_ms = now;
                drop(writer_guard);
                drop(inner);
                self.signal_changed();
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.pending_writer_bytes
                    .fetch_sub(data_len, Ordering::Acquire);
                bail!("session input queue is full");
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending_writer_bytes
                    .fetch_sub(data_len, Ordering::Acquire);
                bail!("session input channel is closed");
            }
        }
    }

    pub(super) fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        validate_terminal_size(cols, rows)?;
        if self.shutdown.load(Ordering::Acquire) {
            bail!("session has already stopped");
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?;
        if self.cleanup_claimed.load(Ordering::Acquire)
            || self.cleanup_committed.load(Ordering::Acquire)
        {
            bail!("session cleanup has started");
        }
        if inner.process_done || phase_is_terminal(inner.phase) || inner.error.is_some() {
            bail!("session is not resizable");
        }
        let master_guard = self
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY master lock was poisoned"))?;
        let Some(master) = master_guard.as_ref() else {
            bail!("PTY master is closed");
        };
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize PTY")?;
        inner.parser.screen_mut().set_size(rows, cols);
        inner.updated_at_ms = now_millis();
        drop(master_guard);
        drop(inner);
        self.signal_changed();
        Ok(())
    }

    pub(super) fn append_output(&self, data: Vec<u8>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let now = now_millis();
        let start = inner.next_cursor;
        inner.next_cursor = inner.next_cursor.saturating_add(data.len() as u64);
        inner.transcript_bytes = inner.transcript_bytes.saturating_add(data.len());
        inner.parser.process(&data);
        inner.title = inner.parser.callbacks().title.clone();
        let callbacks = inner.parser.callbacks_mut();
        let title_updated = std::mem::take(&mut callbacks.title_updated);
        let responses = std::mem::take(&mut callbacks.responses);
        let phase = phase_after_output(
            inner.worker,
            inner.phase,
            inner.title.as_deref(),
            title_updated,
            inner.hook.activity,
            inner.process_done,
            inner.error.is_some(),
            self.shutdown.load(Ordering::Acquire),
        );
        set_phase(&mut inner, phase, now);
        inner.last_output_at_ms = Some(now);
        inner.updated_at_ms = now;
        inner.chunks.push_back(OutputChunk { start, data });
        while inner.transcript_bytes > MAX_TRANSCRIPT_BYTES {
            let Some(removed) = inner.chunks.pop_front() else {
                break;
            };
            inner.transcript_bytes = inner.transcript_bytes.saturating_sub(removed.data.len());
        }
        drop(inner);
        for response in responses {
            self.queue_terminal_response(response);
        }
        self.signal_changed();
    }

    pub(super) fn queue_terminal_response(&self, response: Vec<u8>) {
        // Terminal response frames bypass admission: they carry budget 0 so the
        // writer's release never underflows the byte counter for bytes that
        // were never counted.
        let result = self.writer_tx.lock().ok().and_then(|writer| {
            writer.as_ref().map(|writer| {
                writer.try_send(WriterItem {
                    data: response,
                    followup: None,
                    budget: 0,
                })
            })
        });
        match result {
            Some(Ok(())) => {}
            Some(Err(TrySendError::Full(_))) => {
                // Queue full is transient backpressure, not a fatal writer
                // failure: record the error for observers but never escalate
                // the owned process scope, matching enqueue_input semantics.
                self.mark_writer_error_with_escalation(
                    "terminal response queue is full".to_owned(),
                    false,
                );
            }
            Some(Err(TrySendError::Disconnected(_))) | None => {
                if !self.shutdown.load(Ordering::Acquire) {
                    self.mark_writer_error("terminal response channel is closed".to_owned());
                }
            }
        }
    }

    pub(super) fn mark_reader_done(&self) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            inner.reader_done = true;
            inner.updated_at_ms = now_millis();
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        self.finish_transition(finalized);
    }

    pub(super) fn mark_reader_error(&self, message: String) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            inner.reader_done = true;
            record_error(&mut inner, message);
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        self.finish_transition(finalized);
        // The stream is gone; terminate the owned process scope within a bounded
        // escalation so the session converges instead of waiting forever.
        if !self.process_is_done_or_terminal() {
            self.run_error_edge_escalation();
        }
    }

    pub(super) fn mark_writer_error(&self, message: String) {
        self.mark_writer_error_with_escalation(message, true);
    }

    /// Record a writer failure and optionally escalate the owned process scope.
    /// `escalate` must be false for recoverable backpressure (a full queue):
    /// the stream is still usable and the scope must survive; escalation is
    /// reserved for unrecoverable failures like a closed channel.
    pub(super) fn mark_writer_error_with_escalation(&self, message: String, escalate: bool) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            record_error(&mut inner, message);
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        self.finish_transition(finalized);
        if escalate && !self.process_is_done_or_terminal() {
            self.run_error_edge_escalation();
        }
    }

    pub(super) fn mark_wait_error(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            record_error(&mut inner, message);
        }
        self.signal_changed();
        // The waiter can no longer observe the root process, so verify the
        // whole owned scope directly (Unix process-group probe or Windows Job
        // Object). Never set process_done
        // without verification: an unverifiable failure keeps the real state
        // and ownership so a later close can retry termination.
        self.run_error_edge_escalation();
    }

    pub(super) fn mark_exit(&self, exit_code: u32) {
        let finalized = if let Ok(mut inner) = self.inner.lock() {
            if !inner.process_done {
                inner.process_done = true;
                inner.exit_code = Some(exit_code);
                // The root exited: the externally visible process id is stale
                // now (baseline semantics; a natural exit must never expose a
                // dead pid). The immutable Session::scope_id still identifies
                // the whole owned scope for close liveness and termination.
                inner.process_id = None;
            }
            inner.updated_at_ms = now_millis();
            finalize_session(&mut inner, self.shutdown.load(Ordering::Acquire))
        } else {
            false
        };
        #[cfg(windows)]
        if !finalized {
            // On Windows ConPTY, once the child exits naturally, release the
            // master handle so ClosePseudoConsole lets the reader observe EOF
            // after draining any buffered output.
            self.close_writer();
            self.release_master();
        }
        self.finish_transition(finalized);
    }

    #[cfg(windows)]
    pub(super) fn set_process_id(&self, process_id: u32) -> Result<()> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("session state lock was poisoned"))?
            .process_id = Some(process_id);
        Ok(())
    }

    #[cfg(windows)]
    pub(super) fn bounded_pre_handshake_output(&self) -> String {
        let Ok(inner) = self.inner.lock() else {
            return String::new();
        };
        let mut bytes = Vec::new();
        for chunk in &inner.chunks {
            bytes.extend_from_slice(&chunk.data);
            if bytes.len() >= WINDOWS_PRE_HANDSHAKE_OUTPUT_MAX {
                break;
            }
        }
        let end = bytes.len().min(WINDOWS_PRE_HANDSHAKE_OUTPUT_MAX);
        bytes[..end].escape_ascii().to_string()
    }
}

pub(super) fn spawn_reader(session: Arc<Session>, mut reader: Box<dyn Read + Send>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    session.mark_reader_done();
                    return;
                }
                Ok(read) => session.append_output(buffer[..read].to_vec()),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
                    ) =>
                {
                    session.mark_reader_done();
                    return;
                }
                Err(error) => {
                    session.mark_reader_error(format!("failed to read Grok output: {error}"));
                    return;
                }
            }
        }
    });
}

pub(super) fn spawn_writer(
    session: Arc<Session>,
    mut writer: Box<dyn Write + Send>,
    writer_rx: std::sync::mpsc::Receiver<WriterItem>,
) {
    thread::spawn(move || {
        while let Ok(item) = writer_rx.recv() {
            let result = (|| {
                if session.shutdown.load(Ordering::Acquire) {
                    return Ok(());
                }
                writer.write_all(&item.data)?;
                writer.flush()?;
                if let Some((delay, followup_data)) = item.followup {
                    let deadline = Instant::now() + delay;
                    loop {
                        if session.shutdown.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        thread::sleep(remaining.min(Duration::from_millis(20)));
                    }
                    writer.write_all(&followup_data)?;
                    writer.flush()?;
                }
                Ok::<(), std::io::Error>(())
            })();
            // Release both writes together, including on failure or cancellation.
            session
                .pending_writer_bytes
                .fetch_sub(item.budget, Ordering::Acquire);
            if let Err(error) = result {
                session.mark_writer_error(format!("failed to write Grok input: {error}"));
                break;
            }
            if session.shutdown.load(Ordering::Acquire) {
                break;
            }
        }
        // Stop admission before releasing items abandoned after an error or shutdown.
        session.close_writer();
        for item in writer_rx.try_iter() {
            session
                .pending_writer_bytes
                .fetch_sub(item.budget, Ordering::Acquire);
        }
    });
}

pub(super) fn spawn_waiter(
    session: Arc<Session>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
) {
    thread::spawn(move || match child.wait() {
        Ok(status) => session.mark_exit(status.exit_code()),
        Err(error) => session.mark_wait_error(format!("failed while waiting for Grok: {error}")),
    });
}
