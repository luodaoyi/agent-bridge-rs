use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    thread,
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use eframe::egui;

use crate::{
    protocol::{MAX_WRITE_BYTES, Request, ResponseEnvelope, ResponseResult},
    transport::call,
};

const READ_LIMIT: u32 = 65_536;
const READ_WAIT_MS: u64 = 500;
pub(super) const WRITER_QUEUE_CAPACITY: usize = 128;

pub(super) struct Snapshot {
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) state: Vec<u8>,
    pub(super) cursor: u64,
}

pub(super) enum ReaderMessage {
    Snapshot(Snapshot),
    Output { data: Vec<u8>, next_cursor: u64 },
    Eof,
    Error(String),
}

pub(super) enum WriterCommand {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

pub(super) enum WriterMessage {
    Closed,
    Error {
        message: String,
        close_failed: bool,
        resize_failed: bool,
    },
}

pub(super) fn spawn_reader(
    session: String,
    context: egui::Context,
    stop: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<ReaderMessage>,
) {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let snapshot = match read_snapshot(&session) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let _ = tx.send(ReaderMessage::Error(format!("{error:#}")));
                    context.request_repaint();
                    return;
                }
            };
            let mut cursor = snapshot.cursor;
            if tx.send(ReaderMessage::Snapshot(snapshot)).is_err() {
                return;
            }
            context.request_repaint();

            while !stop.load(Ordering::Acquire) {
                let read = match read_increment(&session, cursor) {
                    Ok(read) => read,
                    Err(error) => {
                        let _ = tx.send(ReaderMessage::Error(format!("{error:#}")));
                        context.request_repaint();
                        return;
                    }
                };
                if read.truncated {
                    break;
                }

                let data = match BASE64.decode(&read.data_base64) {
                    Ok(data) => data,
                    Err(error) => {
                        let _ = tx.send(ReaderMessage::Error(format!(
                            "runtime returned invalid terminal output: {error}"
                        )));
                        context.request_repaint();
                        return;
                    }
                };
                cursor = read.next_cursor;
                if !data.is_empty()
                    && tx
                        .send(ReaderMessage::Output {
                            data,
                            next_cursor: cursor,
                        })
                        .is_err()
                {
                    return;
                }
                if read.eof {
                    let _ = tx.send(ReaderMessage::Eof);
                    context.request_repaint();
                    return;
                }
                context.request_repaint();
            }
        }
    });
}

fn read_snapshot(session: &str) -> Result<Snapshot> {
    let response = call(
        Request::Show {
            session: session.to_owned(),
        },
        true,
    )?;
    let state = match successful_result(response)? {
        ResponseResult::Session(state) => state,
        _ => bail!("runtime returned an unexpected show response"),
    };
    let snapshot = BASE64
        .decode(&state.screen_ansi_base64)
        .context("runtime returned an invalid terminal snapshot")?;
    Ok(Snapshot {
        rows: state.rows,
        cols: state.cols,
        state: snapshot,
        cursor: state.last_cursor,
    })
}

fn read_increment(session: &str, cursor: u64) -> Result<crate::protocol::ReadResult> {
    let response = call(
        Request::Read {
            session: session.to_owned(),
            cursor: Some(cursor),
            limit: Some(READ_LIMIT),
            wait_ms: Some(READ_WAIT_MS),
        },
        true,
    )?;
    match successful_result(response)? {
        ResponseResult::Read(read) => Ok(read),
        _ => bail!("runtime returned an unexpected read response"),
    }
}

pub(super) fn spawn_writer(
    session: String,
    context: egui::Context,
    jobs: Receiver<WriterCommand>,
    done: std::sync::mpsc::Sender<WriterMessage>,
) {
    thread::spawn(move || {
        while let Ok(command) = jobs.recv() {
            let close = matches!(command, WriterCommand::Close);
            let resize = matches!(command, WriterCommand::Resize { .. });
            let result = match command {
                WriterCommand::Write(data) => write_all(&session, &data),
                WriterCommand::Resize { cols, rows } => call(
                    Request::Resize {
                        session: session.clone(),
                        cols,
                        rows,
                    },
                    true,
                )
                .and_then(expect_writer_success),
                WriterCommand::Close => call(
                    Request::Close {
                        session: session.clone(),
                    },
                    true,
                )
                .and_then(expect_writer_success),
            };
            match result {
                Ok(()) if close => {
                    let _ = done.send(WriterMessage::Closed);
                    context.request_repaint();
                    return;
                }
                Ok(()) => {}
                Err(error) => {
                    if done
                        .send(WriterMessage::Error {
                            message: format!("{error:#}"),
                            close_failed: close,
                            resize_failed: resize,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
            context.request_repaint();
        }
    });
}

fn write_all(session: &str, data: &[u8]) -> Result<()> {
    for chunk in data.chunks(MAX_WRITE_BYTES) {
        call(
            Request::Write {
                session: session.to_owned(),
                data_base64: BASE64.encode(chunk),
            },
            true,
        )
        .and_then(expect_writer_success)?;
    }
    Ok(())
}

fn successful_result(response: ResponseEnvelope) -> Result<ResponseResult> {
    if !response.ok {
        return Err(response_error(response));
    }
    response
        .result
        .ok_or_else(|| anyhow!("runtime returned a successful response without a result"))
}

fn expect_writer_success(response: ResponseEnvelope) -> Result<()> {
    match successful_result(response)? {
        ResponseResult::Accepted { accepted: true } | ResponseResult::Session(_) => Ok(()),
        ResponseResult::Accepted { accepted: false } => bail!("runtime rejected the request"),
        _ => bail!("runtime returned an unexpected writer response"),
    }
}

fn response_error(response: ResponseEnvelope) -> anyhow::Error {
    match response.error {
        Some(error) => anyhow!("{}: {}", error.code, error.message),
        None => anyhow!("runtime returned an unsuccessful response without an error"),
    }
}
