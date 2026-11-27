use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError, channel, sync_channel},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use eframe::egui;

use crate::gui_fonts::install_cjk_font;

mod input;
mod io;
mod paint;
mod selection;

use input::{copy_requested, input_bytes};
use io::{
    ReaderMessage, WRITER_QUEUE_CAPACITY, WriterCommand, WriterMessage, spawn_reader, spawn_writer,
};
use paint::{DEFAULT_BACKGROUND, DEFAULT_FOREGROUND, TerminalPaintOptions, paint_terminal};
use selection::{Selection, canonical_position, cell_position, selection_text};

const SCROLLBACK_ROWS: usize = 10_000;
const FONT_SIZE: f32 = 14.0;
const TERMINAL_PADDING: f32 = 4.0;
const MIN_COLS: u16 = 20;
const MAX_COLS: u16 = 500;
const MIN_ROWS: u16 = 5;
const MAX_ROWS: u16 = 200;

pub(crate) fn run(session: String) -> Result<()> {
    let title = format!("Agent Terminal - {session}");
    let app_id = format!("agent-bridge-terminal-{session}");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(app_id)
            .with_inner_size([1_080.0, 720.0])
            .with_min_inner_size([640.0, 360.0]),
        ..Default::default()
    };

    eframe::run_native(
        &title,
        options,
        Box::new(move |creation_context| {
            let font_status = match install_cjk_font(&creation_context.egui_ctx) {
                Ok(font) => font_status(&font),
                Err(error) => format!("Terminal font unavailable: {error:#}"),
            };
            Ok(Box::new(TerminalApp::new(
                session,
                creation_context.egui_ctx.clone(),
                font_status,
            )))
        }),
    )
    .map_err(|error| anyhow!(error.to_string()))
}

#[cfg(windows)]
fn font_status(font: &crate::gui_fonts::InstalledFont) -> String {
    format!(
        "Fonts: Consolas ({}) + Microsoft YaHei ({}, face {})",
        font.latin_path.display(),
        font.cjk_path.display(),
        font.cjk_face_index
    )
}

#[cfg(not(windows))]
fn font_status(font: &crate::gui_fonts::InstalledFont) -> String {
    format!(
        "CJK font: {} (face {})",
        font.cjk_path.display(),
        font.cjk_face_index
    )
}

struct TerminalApp {
    session: String,
    parser: Option<vt100::Parser>,
    reader_rx: Receiver<ReaderMessage>,
    writer_tx: SyncSender<WriterCommand>,
    writer_rx: Receiver<WriterMessage>,
    stop_reader: Arc<AtomicBool>,
    cursor: u64,
    last_resize: Option<(u16, u16)>,
    selection: Option<Selection>,
    scroll_remainder: f32,
    close_pending: bool,
    eof: bool,
    status: String,
    error: Option<String>,
    font_status: String,
}

impl TerminalApp {
    fn new(session: String, context: egui::Context, font_status: String) -> Self {
        let (reader_tx, reader_rx) = channel();
        let (writer_tx, writer_jobs) = sync_channel(WRITER_QUEUE_CAPACITY);
        let (writer_done, writer_rx) = channel();
        let stop_reader = Arc::new(AtomicBool::new(false));

        spawn_reader(
            session.clone(),
            context.clone(),
            Arc::clone(&stop_reader),
            reader_tx,
        );
        spawn_writer(session.clone(), context, writer_jobs, writer_done);

        Self {
            session,
            parser: None,
            reader_rx,
            writer_tx,
            writer_rx,
            stop_reader,
            cursor: 0,
            last_resize: None,
            selection: None,
            scroll_remainder: 0.0,
            close_pending: false,
            eof: false,
            status: "正在连接会话…".to_owned(),
            error: None,
            font_status,
        }
    }

    fn apply_reader_message(&mut self, message: ReaderMessage) {
        match message {
            ReaderMessage::Snapshot(snapshot) => {
                let rows = snapshot.rows.max(1);
                let cols = snapshot.cols.max(1);
                let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
                parser.process(&snapshot.state);
                self.parser = Some(parser);
                self.cursor = snapshot.cursor;
                self.last_resize = None;
                self.selection = None;
                self.scroll_remainder = 0.0;
                self.eof = false;
                self.error = None;
                self.status = format!("已连接 · {cols}×{rows}");
            }
            ReaderMessage::Output { data, next_cursor } => {
                if let Some(parser) = self.parser.as_mut() {
                    parser.process(&data);
                    self.cursor = next_cursor;
                    self.status = format!("实时读取 · cursor {}", self.cursor);
                }
            }
            ReaderMessage::Eof => {
                self.eof = true;
                self.status = "会话输出已结束".to_owned();
            }
            ReaderMessage::Error(error) => {
                self.error = Some(error);
                self.status = "读取已停止".to_owned();
            }
        }
    }

    fn apply_writer_message(&mut self, context: &egui::Context, message: WriterMessage) {
        match message {
            WriterMessage::Closed => {
                self.status = "会话已关闭".to_owned();
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            WriterMessage::Error {
                message,
                close_failed,
                resize_failed,
            } => {
                if close_failed {
                    self.close_pending = false;
                }
                if resize_failed {
                    self.last_resize = None;
                }
                self.error = Some(message);
            }
        }
    }

    fn queue_writer(&mut self, command: WriterCommand) -> bool {
        match self.writer_tx.try_send(command) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.error = Some("终端输入队列已满，请稍后重试".to_owned());
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.error = Some("终端 writer worker 已退出".to_owned());
                false
            }
        }
    }

    fn resize_terminal(&mut self, cols: u16, rows: u16) {
        if self.parser.is_none() {
            return;
        }
        if self.last_resize == Some((cols, rows)) {
            return;
        }
        if self.queue_writer(WriterCommand::Resize { cols, rows }) {
            self.last_resize = Some((cols, rows));
            if let Some(parser) = self.parser.as_mut() {
                parser.screen_mut().set_size(rows, cols);
            }
        }
    }

    fn send_input(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.queue_writer(WriterCommand::Write(bytes));
        }
    }

    fn scroll_terminal(&mut self, delta_y: f32, cell_height: f32) {
        if delta_y == 0.0 {
            return;
        }
        self.scroll_remainder += delta_y / cell_height;
        let rows = self.scroll_remainder.trunc() as isize;
        if rows == 0 {
            return;
        }
        self.scroll_remainder -= rows as f32;
        if let Some(parser) = self.parser.as_mut() {
            let current = parser.screen().scrollback() as isize;
            parser
                .screen_mut()
                .set_scrollback(current.saturating_add(rows).max(0) as usize);
        }
    }

    fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Grok Terminal");
            ui.separator();
            ui.monospace(&self.session);
            ui.separator();
            ui.label(&self.status);

            let close = ui.add_enabled(
                !self.close_pending,
                egui::Button::new(if self.close_pending {
                    "正在关闭会话…"
                } else {
                    "关闭会话"
                }),
            );
            if close.clicked() && self.queue_writer(WriterCommand::Close) {
                self.close_pending = true;
            }
        });
        ui.small(format!(
            "{} · 关闭窗口只会分离，不会关闭会话",
            self.font_status
        ));
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }
        ui.separator();
    }

    fn show_terminal(&mut self, ui: &mut egui::Ui) {
        let font_id = egui::FontId::monospace(FONT_SIZE);
        let (cell_width, cell_height) = ui.fonts_mut(|fonts| {
            (
                fonts.glyph_width(&font_id, 'M').max(1.0),
                fonts.row_height(&font_id).max(1.0),
            )
        });

        let available = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::click_and_drag());
        if response.clicked_by(egui::PointerButton::Primary) {
            response.request_focus();
            self.selection = None;
        } else if response.drag_started_by(egui::PointerButton::Primary) {
            response.request_focus();
        }
        if response.has_focus() {
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }

        let terminal_rect = rect.shrink(TERMINAL_PADDING);
        let cols = ((terminal_rect.width() / cell_width).floor() as u16).clamp(MIN_COLS, MAX_COLS);
        let rows =
            ((terminal_rect.height() / cell_height).floor() as u16).clamp(MIN_ROWS, MAX_ROWS);
        self.resize_terminal(cols, rows);

        if response.hovered() {
            let delta_y = ui.input(|input| input.smooth_scroll_delta.y);
            self.scroll_terminal(delta_y, cell_height);
        }

        let pointer_cell = response.interact_pointer_pos().and_then(|position| {
            cell_position(terminal_rect, position, cell_width, cell_height, rows, cols)
        });
        if response.drag_started_by(egui::PointerButton::Primary) {
            if let Some(position) = pointer_cell {
                let position = canonical_position(self.parser.as_ref(), position);
                self.selection = Some(Selection {
                    anchor: position,
                    head: position,
                });
            }
        } else if response.dragged_by(egui::PointerButton::Primary)
            && let Some(position) = pointer_cell
        {
            let position = canonical_position(self.parser.as_ref(), position);
            if let Some(selection) = self.selection.as_mut() {
                selection.head = position;
            }
        }

        let blink_on = ui.input(|input| ((input.time * 2.0) as u64).is_multiple_of(2));
        if let Some(parser) = self.parser.as_ref() {
            paint_terminal(
                ui,
                parser.screen(),
                TerminalPaintOptions {
                    rect: terminal_rect,
                    font_id: &font_id,
                    cell_width,
                    cell_height,
                    focused: response.has_focus(),
                    blink_on,
                    selection: self.selection,
                },
            );
        } else {
            ui.painter().rect_filled(rect, 0.0, DEFAULT_BACKGROUND);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "正在载入终端快照…",
                font_id,
                DEFAULT_FOREGROUND,
            );
        }

        if response.has_focus() {
            let (events, modifiers) = ui.input(|input| (input.events.clone(), input.modifiers));
            let application_cursor = self
                .parser
                .as_ref()
                .is_some_and(|parser| parser.screen().application_cursor());
            let bracketed_paste = self
                .parser
                .as_ref()
                .is_some_and(|parser| parser.screen().bracketed_paste());
            let has_selection = self.selection.is_some();
            if has_selection && copy_requested(&events) {
                let text = self
                    .parser
                    .as_ref()
                    .and_then(|parser| {
                        self.selection
                            .map(|selection| selection_text(parser.screen(), selection))
                    })
                    .unwrap_or_default();
                ui.ctx().copy_text(text);
            }
            if !self.close_pending && !self.eof {
                self.send_input(input_bytes(
                    &events,
                    modifiers,
                    application_cursor,
                    bracketed_paste,
                    has_selection,
                ));
            }
        }
    }
}

impl eframe::App for TerminalApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(message) = self.reader_rx.try_recv() {
            self.apply_reader_message(message);
        }
        while let Ok(message) = self.writer_rx.try_recv() {
            self.apply_writer_message(context, message);
        }
        context.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.add_space(6.0);
        self.show_toolbar(ui);
        self.show_terminal(ui);
    }
}

impl Drop for TerminalApp {
    fn drop(&mut self) {
        self.stop_reader.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
