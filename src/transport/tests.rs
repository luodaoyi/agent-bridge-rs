use super::platform::*;
use super::*;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};
#[cfg(unix)]
use std::{
    ffi::OsString,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
/// A unique throwaway temp directory, removed on drop. Tests must never leave
/// files behind on failure, and must never collide with the real runtime path.
#[cfg(all(unix, test))]
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(all(unix, test))]
struct TempDir(PathBuf);

#[cfg(all(unix, test))]
impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "grok-bridge-ipc-test-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(all(unix, test))]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A unique throwaway temp directory under the canonical short `/tmp`, removed
/// on drop (including on assertion failure). Unix socket paths created inside
/// it stay short enough for `sun_path` on every platform — `TempDir` lives
/// under a deep macOS `TMPDIR`, which would overflow the socket address.
#[cfg(all(unix, test))]
static NEXT_SHORT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(all(unix, test))]
pub(crate) struct ShortTempDir(PathBuf);

#[cfg(all(unix, test))]
impl ShortTempDir {
    pub(crate) fn new(label: &str) -> Self {
        let path = std::fs::canonicalize("/tmp")
            .expect("canonical /tmp must resolve")
            .join(format!(
                "gbt-{label}-{}-{}",
                std::process::id(),
                NEXT_SHORT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(all(unix, test))]
impl Drop for ShortTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Preserve the existing test grouping inside the extracted test module.
#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::*;
    use crate::protocol::WaitCondition;
    use std::io::{Cursor, Read};
    use std::sync::mpsc;

    #[test]
    fn reads_exactly_one_frame() {
        let mut reader = BufReader::new(Cursor::new(b"one\ntwo\n"));
        assert_eq!(read_frame(&mut reader, None).unwrap(), b"one\n");
        assert_eq!(read_frame(&mut reader, None).unwrap(), b"two\n");
    }

    #[test]
    fn rejects_oversized_frames_before_unbounded_growth() {
        let input = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut reader = BufReader::new(Cursor::new(input));
        assert!(read_frame(&mut reader, None).is_err());
    }

    #[test]
    fn read_frame_with_a_fresh_deadline_accepts_prompt_frames() {
        let mut reader = BufReader::new(Cursor::new(b"one\n"));
        let deadline = Instant::now() + Duration::from_secs(60);
        assert_eq!(read_frame(&mut reader, Some(deadline)).unwrap(), b"one\n");
    }

    #[test]
    fn read_frame_with_an_expired_deadline_fails_before_reading() {
        let mut reader = BufReader::new(Cursor::new(b"one\n"));
        let expired = Instant::now() - Duration::from_secs(1);
        let error = read_frame(&mut reader, Some(expired)).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
    }

    /// Simulates a PIPE_NOWAIT named pipe: first read is a successful
    /// zero-byte completion (no data yet), later reads return the frame.
    struct EmptyThenFrame {
        sent_empty: bool,
        rest: Cursor<Vec<u8>>,
    }

    impl Read for EmptyThenFrame {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent_empty {
                self.sent_empty = true;
                return Ok(0);
            }
            self.rest.read(buf)
        }
    }

    /// Observes the first OS-level `read` so delayed-write tests can wait until
    /// the server has actually polled an empty nowait pipe before the client
    /// writes. Nested *inside* `RemapNowaitEmptyRead`.
    struct SignalAfterFirstRead<R> {
        inner: R,
        signal: Option<mpsc::Sender<()>>,
    }

    impl<R: Read> Read for SignalAfterFirstRead<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let result = self.inner.read(buf);
            if let Some(tx) = self.signal.take() {
                let _ = tx.send(());
            }
            result
        }
    }

    #[test]
    fn nowait_empty_read_without_remap_looks_like_peer_closed() {
        // Documents the actual failure path: BufReader latches Ok(0) as EOF
        // and read_frame reports peer-closed even though a frame follows.
        let inner = EmptyThenFrame {
            sent_empty: false,
            rest: Cursor::new(b"pong\n".to_vec()),
        };
        let mut reader = BufReader::new(inner);
        let deadline = Instant::now() + Duration::from_secs(2);
        let error = read_frame(&mut reader, Some(deadline)).unwrap_err();
        assert!(error.to_string().contains("peer closed"), "{error:#}");
    }

    #[test]
    fn nowait_empty_read_is_not_eof_when_data_follows() {
        let inner = EmptyThenFrame {
            sent_empty: false,
            rest: Cursor::new(b"pong\n".to_vec()),
        };
        let mut reader = BufReader::new(RemapNowaitEmptyRead(inner));
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(read_frame(&mut reader, Some(deadline)).unwrap(), b"pong\n");
    }

    #[test]
    fn response_deadline_accommodates_documented_long_waits() {
        // The server clamps wait timeouts to 7_200_000 ms and read waits to
        // 300 s; the client deadline must never undercut either.
        let wait = Request::Wait {
            session: "s".to_owned(),
            for_condition: WaitCondition::TuiIdle,
            timeout_ms: Some(7_200_000),
        };
        assert!(
            response_deadline_for(&wait) >= Duration::from_millis(7_200_000) + IPC_RESPONSE_SLACK
        );

        let read = Request::Read {
            session: "s".to_owned(),
            cursor: Some(0),
            limit: Some(1024),
            wait_ms: Some(300_000),
        };
        assert!(
            response_deadline_for(&read) >= Duration::from_millis(300_000) + IPC_RESPONSE_SLACK
        );

        // Ordinary requests still get a bounded default rather than forever.
        let status = Request::ServerStatus;
        assert_eq!(response_deadline_for(&status), IPC_RESPONSE_SLACK);
        assert!(response_deadline_for(&status) < Duration::from_secs(120));
    }

    #[test]
    fn write_response_round_trips_an_envelope() {
        let response = ResponseEnvelope::failure("r1", "boom", "kaput");
        let mut sink = Cursor::new(Vec::new());
        write_response(&mut sink, &response).unwrap();
        let frame = sink.into_inner();
        assert_eq!(decode_response(&frame).unwrap(), response);
    }

    #[test]
    fn read_frame_with_deadline_times_out_for_a_silent_peer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        // The peer never sends; the non-blocking read + deadline must give up
        // instead of pinning the thread forever.
        server.set_nonblocking(true).unwrap();
        let mut reader = BufReader::new(server);
        let started = Instant::now();
        let error = read_frame(
            &mut reader,
            Some(Instant::now() + Duration::from_millis(100)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn write_frame_all_times_out_for_a_peer_that_stops_draining() {
        // Exercise the Runtime's real IPC channel type (AF_UNIX socket on Unix,
        // named pipe on Windows) on a throwaway name — never the user's live
        // runtime path. Its pipe buffers are small and fixed on every platform,
        // so a peer that stops reading genuinely fills the channel — unlike a
        // TCP pair, where delayed ACKs and autotuned buffers keep freeing
        // headroom and can let a small frame slip through, hiding the write
        // deadline.
        #[cfg(unix)]
        let (name, _dir) = {
            let dir = ShortTempDir::new("write-deadline");
            let name = dir
                .path()
                .join("test.sock")
                .to_fs_name::<GenericFilePath>()
                .unwrap();
            (name, dir)
        };
        #[cfg(windows)]
        let name = {
            // A throwaway one-shot pipe name, never the live runtime name:
            // parallel tests must not collide with each other or with a real
            // server, and the pipe is bound only inside this test.
            static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
            format!(
                "grok-bridge-write-deadline-test-{}-{id}",
                std::process::id()
            )
            .to_ns_name::<GenericNamespaced>()
            .unwrap()
        };
        let listener = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .unwrap();
        let _client = Stream::connect(name).unwrap();
        let mut server = listener.accept().unwrap();
        // The peer never reads. Fill the pipe until a non-blocking write
        // signals backpressure: a WouldBlock/TimedOut error on either
        // platform, or Ok(0) on Windows PIPE_NOWAIT pipes, which report a
        // successful zero-byte write when the buffer is full. Unix pipes
        // instead accept bytes until the buffer is genuinely full and only
        // then start returning WouldBlock, so `filled` may stay 0 on Windows
        // but is expected to be large on Unix.
        server.set_nonblocking(true).unwrap();
        let filler = vec![0x55u8; 64 * 1024];
        let mut filled = 0usize;
        let pipe_full = loop {
            match server.write(&filler) {
                // Windows named pipes in nonblocking (PIPE_NOWAIT) mode return
                // Ok(0) when the write could not make progress (the pipe
                // buffer is full), with no implication that the peer closed;
                // WriteFileEx reports a successful zero-byte completion. That
                // is the platform's backpressure signal. On Unix, Ok(0) still
                // means the peer half-closed the connection and must not be
                // mistaken for a full buffer.
                #[cfg(windows)]
                Ok(0) => break true,
                #[cfg(not(windows))]
                Ok(0) => panic!("IPC peer closed while filling the send buffer"),
                Ok(written) => filled += written,
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    break true;
                }
                Err(error) => panic!("unexpected fill error: {error}"),
            }
            if filled > 16 * 1024 * 1024 {
                panic!("send buffer never blocked; cannot exercise the write deadline");
            }
        };
        assert!(
            pipe_full,
            "fill loop ended without a backpressure signal (WouldBlock/TimedOut, or Ok(0) on Windows) after {filled} bytes"
        );

        // A full-size frame can never fit in the small filled pipe, so the
        // stalled peer must surface as a bounded timeout instead of a
        // completed write.
        let started = Instant::now();
        let data = vec![0x66u8; MAX_FRAME_BYTES];
        let error = write_frame_all(&mut server, &data, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// PIPE_NOWAIT named-pipe WriteFile is all-or-nothing: a slice larger than
    /// the remaining buffer completes with Ok(0) instead of a partial write.
    /// Issue #27 on 0.8.9: retrying the full remaining frame stalled for 30s.
    struct AtomicNowaitPipe {
        max_atomic: usize,
        written: Vec<u8>,
    }

    impl Write for AtomicNowaitPipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.is_empty() || buf.len() > self.max_atomic {
                return Ok(0);
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn windows_nowait_write_zero_shrinks_until_a_one_byte_stall() {
        let mut write_cap = 700usize;
        assert!(!windows_nowait_write_zero_is_stall(&mut write_cap));
        assert_eq!(write_cap, 350);
        while write_cap > 1 {
            assert!(!windows_nowait_write_zero_is_stall(&mut write_cap));
        }
        assert_eq!(write_cap, 1);
        assert!(windows_nowait_write_zero_is_stall(&mut write_cap));
        assert_eq!(write_cap, 1);
    }

    #[test]
    fn write_frame_all_chunks_windows_nowait_oversize_zero_writes() {
        // Reporter's 600-char show / Skill create path: a modest frame larger
        // than the atomic nowait write size must still be delivered instead of
        // stalling until IPC_WRITE_DEADLINE.
        let payload = vec![b'x'; 700];
        let mut pipe = AtomicNowaitPipe {
            max_atomic: 64,
            written: Vec::new(),
        };
        write_frame_all_with(&mut pipe, &payload, Duration::from_secs(2), true).unwrap();
        assert_eq!(pipe.written, payload);
    }

    #[test]
    fn write_frame_all_without_nowait_chunking_fails_on_atomic_zero_writes() {
        // Documents the 0.8.9 failure: treating Windows Ok(0) only as a stall
        // and retrying the same oversized slice never writes a byte.
        let payload = vec![b'x'; 700];
        let mut pipe = AtomicNowaitPipe {
            max_atomic: 64,
            written: Vec::new(),
        };
        let error = write_frame_all_with(&mut pipe, &payload, Duration::from_millis(80), false)
            .unwrap_err();
        assert!(error.to_string().contains("peer closed"), "{error:#}");
        assert!(pipe.written.is_empty());
    }

    #[test]
    fn write_frame_all_times_out_when_even_one_byte_nowait_write_is_zero() {
        let mut pipe = AtomicNowaitPipe {
            max_atomic: 0,
            written: Vec::new(),
        };
        let started = Instant::now();
        let error = write_frame_all_with(&mut pipe, b"hello\n", Duration::from_millis(80), true)
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(pipe.written.is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_write_frame_all_treats_ok_zero_as_peer_closed() {
        let mut pipe = AtomicNowaitPipe {
            max_atomic: 0,
            written: Vec::new(),
        };
        let error = write_frame_all(&mut pipe, b"hello\n", Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("peer closed"), "{error:#}");
    }

    #[test]
    fn read_frame_waits_for_a_delayed_ipc_response() {
        // Issue #24: a client that polls before the server writes must not
        // treat an empty nowait read as EOF. The server sleeps, then writes
        // a frame; the non-blocking client must still receive it. Throwaway
        // pipe/socket name, never the live runtime endpoint.
        #[cfg(unix)]
        let (name, _dir) = {
            let dir = ShortTempDir::new("delayed-read");
            let name = dir
                .path()
                .join("test.sock")
                .to_fs_name::<GenericFilePath>()
                .unwrap();
            (name, dir)
        };
        #[cfg(windows)]
        let name = {
            static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
            format!("grok-bridge-delayed-read-test-{}-{id}", std::process::id())
                .to_ns_name::<GenericNamespaced>()
                .unwrap()
        };
        let listener = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .unwrap();
        let client = Stream::connect(name).unwrap();
        let mut server = listener.accept().unwrap();
        let server_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            server.write_all(b"pong\n").unwrap();
        });
        client.set_nonblocking(true).unwrap();
        #[cfg(windows)]
        let mut reader = BufReader::new(RemapNowaitEmptyRead(client));
        #[cfg(not(windows))]
        let mut reader = BufReader::new(client);
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(read_frame(&mut reader, Some(deadline)).unwrap(), b"pong\n");
        server_thread.join().unwrap();
    }

    #[test]
    fn read_frame_waits_for_a_delayed_ipc_request() {
        // Issue #27: the server accepts, sets PIPE_NOWAIT, and reads before the
        // client has written. An empty nowait read must not be treated as EOF.
        // Throwaway pipe/socket name, never the live runtime endpoint.
        assert_eq!(read_delayed_ipc_request(b"ping\n".to_vec()), b"ping\n");
    }

    #[test]
    fn read_frame_waits_for_a_delayed_long_ipc_request() {
        // Reporter's 600-char `show --session` repro: a delayed long frame
        // must still be read in full. Throwaway name, never the live endpoint.
        let mut payload = vec![b'a'; 600];
        payload.push(b'\n');
        let expected = payload.clone();
        assert_eq!(read_delayed_ipc_request(payload), expected);
    }

    #[test]
    fn write_frame_all_delivers_a_long_nowait_request_to_a_nowait_reader() {
        // Issue #27 on 0.8.9: both ends PIPE_NOWAIT, client write_frame_all of
        // a 600-byte show-sized frame. Production path in call_over_stream.
        let mut payload = vec![b'a'; 600];
        payload.push(b'\n');
        let expected = payload.clone();
        assert_eq!(write_nowait_ipc_request(payload), expected);
    }

    /// Server-as-reader delayed first byte: wait until the first nowait read
    /// has been attempted, then let the client write. Production wraps only
    /// on Windows; tests wrap too so Linux CI exercises the adapter.
    fn read_delayed_ipc_request(payload: Vec<u8>) -> Vec<u8> {
        #[cfg(unix)]
        let (name, _dir) = {
            let dir = ShortTempDir::new("delayed-req");
            let name = dir
                .path()
                .join("test.sock")
                .to_fs_name::<GenericFilePath>()
                .unwrap();
            (name, dir)
        };
        #[cfg(windows)]
        let name = {
            static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
            format!("grok-bridge-delayed-req-test-{}-{id}", std::process::id())
                .to_ns_name::<GenericNamespaced>()
                .unwrap()
        };
        let listener = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .unwrap();
        let mut client = Stream::connect(name).unwrap();
        let server = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        let (first_read_tx, first_read_rx) = mpsc::channel();
        let client_thread = thread::spawn(move || {
            first_read_rx.recv().unwrap();
            client.write_all(&payload).unwrap();
        });
        let mut reader = BufReader::new(RemapNowaitEmptyRead(SignalAfterFirstRead {
            inner: server,
            signal: Some(first_read_tx),
        }));
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = read_frame(&mut reader, Some(deadline)).unwrap();
        client_thread.join().unwrap();
        frame
    }

    /// Client uses the production `write_frame_all` path with both ends in
    /// nonblocking mode — the reporter's remaining 0.8.9 write-timeout case.
    fn write_nowait_ipc_request(payload: Vec<u8>) -> Vec<u8> {
        #[cfg(unix)]
        let (name, _dir) = {
            let dir = ShortTempDir::new("nowait-write");
            let name = dir
                .path()
                .join("test.sock")
                .to_fs_name::<GenericFilePath>()
                .unwrap();
            (name, dir)
        };
        #[cfg(windows)]
        let name = {
            static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
            format!("grok-bridge-nowait-write-test-{}-{id}", std::process::id())
                .to_ns_name::<GenericNamespaced>()
                .unwrap()
        };
        let listener = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .unwrap();
        let mut client = Stream::connect(name).unwrap();
        let server = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        client.set_nonblocking(true).unwrap();
        let client_thread = thread::spawn(move || {
            write_frame_all(&mut client, &payload, Duration::from_secs(2)).unwrap();
        });
        #[cfg(windows)]
        let mut reader = BufReader::new(RemapNowaitEmptyRead(server));
        #[cfg(not(windows))]
        let mut reader = BufReader::new(server);
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame = read_frame(&mut reader, Some(deadline)).unwrap();
        client_thread.join().unwrap();
        frame
    }

    #[test]
    fn read_frame_fails_fast_when_ipc_peer_disconnects() {
        // Real disconnect must still fail without hanging the full 30s
        // request deadline. Unix is not wrapped: Unix Ok(0) is real EOF.
        // Windows keeps the production wrap; PIPE_NOWAIT may report a
        // closed peer as Ok(0)/ERROR_NO_DATA, which the remap turns into
        // WouldBlock, so the short deadline is what bounds the wait.
        #[cfg(unix)]
        let (name, _dir) = {
            let dir = ShortTempDir::new("peer-drop");
            let name = dir
                .path()
                .join("test.sock")
                .to_fs_name::<GenericFilePath>()
                .unwrap();
            (name, dir)
        };
        #[cfg(windows)]
        let name = {
            static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed);
            format!("grok-bridge-peer-drop-test-{}-{id}", std::process::id())
                .to_ns_name::<GenericNamespaced>()
                .unwrap()
        };
        let listener = ListenerOptions::new()
            .name(name.clone())
            .create_sync()
            .unwrap();
        let client = Stream::connect(name).unwrap();
        let server = listener.accept().unwrap();
        drop(client);
        server.set_nonblocking(true).unwrap();
        #[cfg(windows)]
        let mut reader = BufReader::new(RemapNowaitEmptyRead(server));
        #[cfg(not(windows))]
        let mut reader = BufReader::new(server);
        let started = Instant::now();
        let error =
            read_frame(&mut reader, Some(Instant::now() + Duration::from_secs(2))).unwrap_err();
        let message = error.to_string().to_ascii_lowercase();
        #[cfg(not(windows))]
        assert!(
            message.contains("peer closed")
                || message.contains("broken pipe")
                || message.contains("eof"),
            "{error:#}"
        );
        #[cfg(windows)]
        assert!(
            message.contains("timed out")
                || message.contains("broken pipe")
                || message.contains("not connected")
                || message.contains("peer closed"),
            "{error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "disconnect must not hang the 30s request deadline, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn runtime_identity_is_namespaced_and_stable() {
        let first = runtime_identity();
        let second = runtime_identity();
        assert_eq!(first, second);
        assert!(
            first
                .to_string_lossy()
                .starts_with("agent-bridge-runtime-v1-")
        );
    }

    #[test]
    fn client_identity_resolves_explicit_codex_and_agy_hosts() {
        assert_eq!(
            client_identity_from(
                None,
                None,
                Some("thread-42".to_owned()),
                Some("session-7".to_owned()),
                None,
                None,
            )
            .unwrap(),
            Some((ClientKind::Codex, "codex:thread-42".to_owned()))
        );
        assert_eq!(
            client_identity_from(None, None, None, Some("session-7".to_owned()), None, None)
                .unwrap(),
            Some((ClientKind::Codex, "codex:session-7".to_owned()))
        );
        assert_eq!(
            client_identity_from(
                None,
                None,
                None,
                None,
                Some("conv-99".to_owned()),
                Some("traj-1".to_owned()),
            )
            .unwrap(),
            Some((ClientKind::Agy, "agy:conv-99".to_owned()))
        );
        assert_eq!(
            client_identity_from(
                Some("agy".to_owned()),
                Some("custom-1".to_owned()),
                Some("thread-42".to_owned()),
                None,
                None,
                None,
            )
            .unwrap(),
            Some((ClientKind::Agy, "agy:custom-1".to_owned()))
        );
        assert_eq!(
            client_identity_from(None, None, None, None, None, None).unwrap(),
            None
        );
        assert!(
            client_identity_from(
                None,
                None,
                Some("bad\nidentity".to_owned()),
                None,
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn connect_first_with_no_candidates_is_a_diagnosable_error_not_a_panic() {
        // Regression: `connect()` filters out candidates whose names fail to
        // construct, so an empty list must never reach `unwrap()` on the last
        // connection error.
        let empty: [Name<'_>; 0] = [];
        let error = connect_first(&empty).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no runtime IPC endpoint candidates"),
            "{error:#}"
        );
    }
}

/// Unix-only tests for owner-only Runtime IPC permissions and safe stale-socket
/// cleanup. Every test works on an explicit throwaway temp directory, never on
/// the user's real runtime path.
#[cfg(all(unix, test))]
mod unix_ipc_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    /// Binds a real Unix domain socket at `path`, leaving the socket file in
    /// place when the listener is dropped.
    fn bind_test_socket(path: &Path) -> std::os::unix::net::UnixListener {
        std::os::unix::net::UnixListener::bind(path).unwrap()
    }

    #[test]
    fn fresh_dir_is_created_owner_only() {
        let temp = TempDir::new("dir-fresh");
        let dir = temp.path().join("runtime");
        ensure_private_dir(&dir, current_uid()).unwrap();
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.uid(), current_uid());
        assert_eq!(meta.permissions().mode() & 0o777, RUNTIME_DIR_MODE);
    }

    #[test]
    fn existing_owner_dir_is_tightened_to_owner_only() {
        let temp = TempDir::new("dir-tighten");
        let dir = temp.path().join("runtime");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir, current_uid()).unwrap();
        let mode = std::fs::symlink_metadata(&dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, RUNTIME_DIR_MODE);
    }

    #[test]
    fn symlink_dir_is_rejected_without_being_followed() {
        let temp = TempDir::new("dir-link");
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = temp.path().join("runtime");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = ensure_private_dir(&link, current_uid()).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        // The link and its target must be untouched.
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(real.is_dir());
    }

    #[test]
    fn regular_file_dir_is_rejected_without_deletion() {
        let temp = TempDir::new("dir-file");
        let file = temp.path().join("runtime");
        std::fs::write(&file, b"not a dir").unwrap();
        let error = ensure_private_dir(&file, current_uid()).unwrap_err();
        assert!(error.to_string().contains("not a directory"), "{error:#}");
        assert_eq!(std::fs::read(&file).unwrap(), b"not a dir");
    }

    #[test]
    fn foreign_owner_dir_is_rejected() {
        let temp = TempDir::new("dir-owner");
        let dir = temp.path().join("runtime");
        std::fs::create_dir_all(&dir).unwrap();
        // Exercise the ownership check with a uid that is not ours; the real
        // path stays untouched and unrejected.
        let error = ensure_private_dir(&dir, current_uid() + 1).unwrap_err();
        assert!(error.to_string().contains("uid"), "{error:#}");
        assert!(dir.is_dir());
    }

    #[test]
    fn runtime_dir_uses_only_absolute_owner_only_xdg_bases() {
        let temp = TempDir::new("xdg-base");
        let trusted = temp.path().join("trusted");
        std::fs::create_dir_all(&trusted).unwrap();
        std::fs::set_permissions(&trusted, std::fs::Permissions::from_mode(0o700)).unwrap();
        let fallback = temp.path().join("fallback");
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o700)).unwrap();
        let trusted_canonical = std::fs::canonicalize(&trusted).unwrap();
        let fallback_canonical = std::fs::canonicalize(&fallback).unwrap();

        assert!(trusted_xdg_runtime_base(&trusted, current_uid()));
        // Trust semantics only: an unbounded length budget keeps the socket
        // path length out of this test (length rules have their own tests).
        assert_eq!(
            runtime_dir_from_with_limit(
                Some(trusted.clone().into_os_string()),
                fallback.clone(),
                current_uid(),
                usize::MAX,
            )
            .unwrap(),
            trusted_canonical.join(runtime_identity())
        );

        assert_eq!(
            runtime_dir_from_with_limit(
                Some(OsString::from("relative/runtime")),
                fallback.clone(),
                current_uid(),
                usize::MAX,
            )
            .unwrap(),
            fallback_canonical.join(runtime_identity())
        );
        assert!(!trusted_xdg_runtime_base(&trusted, current_uid() + 1));

        std::fs::set_permissions(&trusted, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!trusted_xdg_runtime_base(&trusted, current_uid()));
        assert_eq!(
            runtime_dir_from_with_limit(
                Some(trusted.into_os_string()),
                fallback.clone(),
                current_uid(),
                usize::MAX,
            )
            .unwrap(),
            fallback_canonical.join(runtime_identity())
        );
    }

    #[test]
    fn runtime_base_rejects_replaceable_parent_and_accepts_sticky_parent() {
        let temp = TempDir::new("runtime-parent");
        let fallback = temp.path().join("fallback");
        std::fs::create_dir_all(&fallback).unwrap();
        std::fs::set_permissions(&fallback, std::fs::Permissions::from_mode(0o700)).unwrap();

        let shared = temp.path().join("shared");
        let candidate = shared.join("candidate");
        std::fs::create_dir_all(&candidate).unwrap();
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!trusted_xdg_runtime_base(&candidate, current_uid()));
        assert_eq!(
            runtime_dir_from_with_limit(
                Some(candidate.clone().into_os_string()),
                fallback.clone(),
                current_uid(),
                usize::MAX,
            )
            .unwrap(),
            std::fs::canonicalize(&fallback)
                .unwrap()
                .join(runtime_identity())
        );

        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(trusted_xdg_runtime_base(&candidate, current_uid()));
        assert_eq!(
            runtime_dir_from_with_limit(
                Some(candidate.clone().into_os_string()),
                fallback,
                current_uid(),
                usize::MAX,
            )
            .unwrap(),
            std::fs::canonicalize(candidate)
                .unwrap()
                .join(runtime_identity())
        );
    }

    #[test]
    fn runtime_dir_prefers_a_short_trusted_xdg_base() {
        // 正常路径：可信且足够短的 XDG 基目录直接被采用，socket 路径保持
        // 在保守上限内。基目录建在 `/tmp` 的真实路径上（macOS 上 `/tmp`
        // 是指向 `/private/tmp` 的符号链接，canonicalize 后即真实目录），
        // 这样跨平台都得到一个短的可信基目录。`ShortTempDir` 在 drop 时
        // 清理目录，断言失败也一样。
        let xdg = ShortTempDir::new("xdg-fit");
        std::fs::set_permissions(xdg.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = runtime_dir_from(
            Some(xdg.path().as_os_str().to_os_string()),
            env::temp_dir(),
            current_uid(),
        )
        .unwrap();
        assert_eq!(
            result,
            std::fs::canonicalize(xdg.path())
                .unwrap()
                .join(runtime_identity())
        );
        assert!(
            socket_path_bytes(&result.join("runtime.sock")) <= UNIX_SOCKET_PATH_MAX_BYTES,
            "socket path must stay within the conservative limit"
        );
    }

    #[test]
    fn runtime_dir_rejects_an_overlong_xdg_base_and_falls_back() {
        // 长路径：XDG 基目录可信但让 socket 路径超过保守上限时，它必须被
        // 跳过，回退到下一个可信基目录（temp，必要时 `/tmp`），最终 socket
        // 路径仍在上限内。确定性 fallback 不引入新目录、不扩大 allowed
        // roots：结果始终是某个已通过信任检查的基目录。
        let temp = TempDir::new("xdg-overlong");
        let long_name = "a".repeat(150);
        let xdg = temp.path().join(&long_name);
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = runtime_dir_from(
            Some(xdg.clone().into_os_string()),
            temp.path().to_path_buf(),
            current_uid(),
        )
        .unwrap();
        assert!(
            socket_path_bytes(&result.join("runtime.sock")) <= UNIX_SOCKET_PATH_MAX_BYTES,
            "fallback socket path must stay within the conservative limit"
        );
        // 拒绝的是 XDG 本身，而不是它的某个子路径。
        assert!(!result.starts_with(std::fs::canonicalize(&xdg).unwrap()));
    }

    #[test]
    fn runtime_dir_selection_is_deterministic_for_identical_inputs() {
        // 稳定性：同一环境两次计算得到完全相同的结果，包括超限回退路径。
        let temp = TempDir::new("xdg-determinism");
        let xdg = temp.path().join("short");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
        let first = runtime_dir_from(
            Some(xdg.clone().into_os_string()),
            temp.path().to_path_buf(),
            current_uid(),
        )
        .unwrap();
        let second = runtime_dir_from(
            Some(xdg.into_os_string()),
            temp.path().to_path_buf(),
            current_uid(),
        )
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn runtime_startup_lock_serializes_concurrent_starters() {
        let temp = TempDir::new("startup-lock");
        let dir = temp.path().join("runtime");
        ensure_private_dir(&dir, current_uid()).unwrap();
        let first = acquire_runtime_startup_lock_at(&dir, Duration::from_secs(1)).unwrap();
        let acquired = Arc::new(AtomicBool::new(false));
        let acquired_in_thread = Arc::clone(&acquired);
        let thread_dir = dir.clone();
        let waiter = std::thread::spawn(move || {
            let second =
                acquire_runtime_startup_lock_at(&thread_dir, Duration::from_secs(2)).unwrap();
            acquired_in_thread.store(true, Ordering::Release);
            drop(second);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!acquired.load(Ordering::Acquire));
        drop(first);
        waiter.join().unwrap();
        assert!(acquired.load(Ordering::Acquire));
    }

    #[test]
    fn stale_socket_symlink_is_refused_without_being_followed() {
        let temp = TempDir::new("sock-link");
        let target = temp.path().join("target");
        std::fs::write(&target, b"x").unwrap();
        let link = temp.path().join("runtime.sock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = stale_socket_safe_to_remove(&link, current_uid()).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
        // The symlink itself is never followed or removed.
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
    }

    #[test]
    fn stale_socket_wrong_file_type_is_refused_without_deletion() {
        let temp = TempDir::new("sock-file");
        let file = temp.path().join("runtime.sock");
        std::fs::write(&file, b"keep me").unwrap();
        let error = stale_socket_safe_to_remove(&file, current_uid()).unwrap_err();
        assert!(
            error.to_string().contains("not a Unix domain socket"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
    }

    #[test]
    fn stale_socket_foreign_owner_is_refused() {
        let temp = TempDir::new("sock-owner");
        let socket = temp.path().join("runtime.sock");
        let listener = bind_test_socket(&socket);
        drop(listener);
        let error = stale_socket_safe_to_remove(&socket, current_uid() + 1).unwrap_err();
        assert!(error.to_string().contains("uid"), "{error:#}");
        assert!(
            std::fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_socket()
        );
    }

    #[test]
    fn owned_stale_socket_is_removed() {
        let temp = TempDir::new("sock-remove");
        let socket = temp.path().join("runtime.sock");
        let listener = bind_test_socket(&socket);
        drop(listener);
        assert!(socket.exists());
        assert!(remove_stale_socket_at(&socket).unwrap());
        assert!(!socket.exists());
    }

    #[test]
    fn missing_stale_socket_is_not_an_error() {
        let temp = TempDir::new("sock-missing");
        let socket = temp.path().join("runtime.sock");
        assert!(!remove_stale_socket_at(&socket).unwrap());
    }

    #[test]
    fn bound_socket_is_owner_only_socket_for_current_user() {
        let temp = TempDir::new("sock-mode");
        let path = temp.path().join("runtime.sock");
        let _listener = bind_listener_at(&path).unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.uid(), current_uid());
        assert_eq!(meta.permissions().mode() & 0o777, RUNTIME_SOCKET_MODE);
    }

    #[test]
    fn occupied_socket_bind_fails_with_addr_in_use() {
        let temp = TempDir::new("sock-occupied");
        let path = temp.path().join("runtime.sock");
        let _first = bind_listener_at(&path).unwrap();
        let error = bind_listener_at(&path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AddrInUse);
    }

    #[test]
    fn runtime_socket_path_lives_inside_the_runtime_dir() {
        assert_eq!(
            runtime_socket_path().unwrap(),
            runtime_dir().unwrap().join("runtime.sock")
        );
    }

    /// A throwaway namespaced IPC name, unique per test, never the live
    /// runtime name: parallel tests must not collide with each other or with a
    /// real server.
    fn unique_namespaced_name(label: &str) -> Name<'static> {
        static NEXT_NAMESPACED_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_NAMESPACED_ID.fetch_add(1, Ordering::Relaxed);
        format!(
            "grok-bridge-legacy-test-{label}-{}-{id}",
            std::process::id()
        )
        .to_ns_name::<GenericNamespaced>()
        .unwrap()
    }

    #[test]
    fn legacy_runtime_name_matches_the_old_binaries_namespaced_identity() {
        // Upgrade compatibility: the legacy endpoint must be exactly what an
        // older binary bound — the same per-user identity through the same
        // GenericNamespaced conversion. If either changes, clients can no
        // longer find a Runtime started by the previous version.
        let legacy = legacy_runtime_name().unwrap();
        let expected = runtime_identity()
            .to_ns_name::<GenericNamespaced>()
            .unwrap();
        assert_eq!(legacy, expected);
    }

    #[test]
    fn legacy_namespaced_runtime_is_selected_when_the_filesystem_endpoint_is_absent() {
        // Upgrade compatibility: a Runtime still running from an older binary
        // answers on the legacy GenericNamespaced name. The client must use it
        // instead of auto-starting a second Runtime (which would strand old
        // sessions and contend for the WebUI port). The filesystem candidate is
        // absent; the namespaced one is live, so the first live connection must
        // come from the legacy endpoint.
        let legacy = unique_namespaced_name("legacy-selected");
        let listener = ListenerOptions::new()
            .name(legacy.clone())
            .create_sync()
            .unwrap();
        let missing_fs = unique_short_socket_path("legacy-fs-absent")
            .to_fs_name::<GenericFilePath>()
            .unwrap();
        let stream = connect_first(&[missing_fs, legacy]).unwrap();
        drop(listener);
        drop(stream);
    }

    /// A unique throwaway filesystem socket path short enough for `sun_path`
    /// (TempDir lives under a deep macOS `TMPDIR`, which would overflow the
    /// socket address). Built on the canonical `/tmp` so it is a real, short
    /// directory on every Unix.
    fn unique_short_socket_path(label: &str) -> PathBuf {
        static NEXT_SHORT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_SHORT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
        let base = std::fs::canonicalize("/tmp").unwrap();
        base.join(format!("gbt-{label}-{}-{id}.sock", std::process::id()))
    }

    #[test]
    fn legacy_namespaced_runtime_is_not_used_when_the_filesystem_endpoint_is_live() {
        // The current endpoint wins: a live filesystem socket is preferred over
        // the legacy name, so after a clean migration the client never talks to
        // a stale namespaced listener.
        let legacy = unique_namespaced_name("legacy-second");
        let _legacy_listener = ListenerOptions::new()
            .name(legacy.clone())
            .create_sync()
            .unwrap();
        let fs_path = unique_short_socket_path("legacy-fs-live");
        let fs_listener = bind_listener_at(&fs_path).unwrap();
        let fs_name = fs_path.to_fs_name::<GenericFilePath>().unwrap();
        let stream = connect_first(&[fs_name, legacy]).unwrap();
        drop(fs_listener);
        drop(stream);
    }

    #[test]
    fn connect_first_fails_only_when_every_endpoint_is_unavailable() {
        // Auto-start must be the last resort: only when both the filesystem
        // endpoint and the legacy name are dead does the client consider
        // launching a new Runtime.
        let missing_fs = unique_short_socket_path("legacy-none")
            .to_fs_name::<GenericFilePath>()
            .unwrap();
        let missing_legacy = unique_namespaced_name("legacy-none");
        let error = connect_first(&[missing_fs, missing_legacy]).unwrap_err();
        assert!(
            error.to_string().contains("runtime server is not running"),
            "{error:#}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_detached_server_flags_try_breakaway_then_fallback() {
        let [with_breakaway, without_breakaway] = detached_server_creation_flags();
        assert_ne!(with_breakaway & CREATE_BREAKAWAY_FROM_JOB, 0);
        assert_eq!(without_breakaway & CREATE_BREAKAWAY_FROM_JOB, 0);
        assert_ne!(without_breakaway & CREATE_NO_WINDOW, 0);
        assert_ne!(without_breakaway & CREATE_NEW_PROCESS_GROUP, 0);
        assert!(should_retry_without_breakaway(Some(
            ERROR_ACCESS_DENIED as i32
        )));
        assert!(!should_retry_without_breakaway(Some(2)));
        assert!(!should_retry_without_breakaway(None));
    }
}
