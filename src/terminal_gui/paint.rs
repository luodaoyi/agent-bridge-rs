use eframe::egui;

use super::selection::{Selection, normalized_selection, position_selected};

pub(super) const DEFAULT_FOREGROUND: egui::Color32 = egui::Color32::from_rgb(216, 222, 233);
pub(super) const DEFAULT_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(11, 14, 20);

pub(super) struct TerminalPaintOptions<'a> {
    pub(super) rect: egui::Rect,
    pub(super) font_id: &'a egui::FontId,
    pub(super) cell_width: f32,
    pub(super) cell_height: f32,
    pub(super) focused: bool,
    pub(super) blink_on: bool,
    pub(super) selection: Option<Selection>,
}

pub(super) fn paint_terminal(
    ui: &egui::Ui,
    screen: &vt100::Screen,
    options: TerminalPaintOptions<'_>,
) {
    let TerminalPaintOptions {
        rect,
        font_id,
        cell_width,
        cell_height,
        focused,
        blink_on,
        selection,
    } = options;
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, DEFAULT_BACKGROUND);

    let (screen_rows, screen_cols) = screen.size();
    let visible_cols = ((rect.width() / cell_width).floor() as u16).min(screen_cols);
    let visible_rows = ((rect.height() / cell_height).floor() as u16).min(screen_rows);
    let normalized_selection =
        selection.and_then(|selection| normalized_selection(selection, visible_rows, visible_cols));

    for row in 0..visible_rows {
        for col in 0..visible_cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let mut foreground = terminal_color(cell.fgcolor(), DEFAULT_FOREGROUND, cell.bold());
            let mut background = terminal_color(cell.bgcolor(), DEFAULT_BACKGROUND, false);
            if cell.inverse() {
                std::mem::swap(&mut foreground, &mut background);
            }
            if cell.dim() {
                foreground = dim_color(foreground);
            }

            let left = rect.left() + f32::from(col) * cell_width;
            let top = rect.top() + f32::from(row) * cell_height;
            let cell_rect = egui::Rect::from_min_size(
                egui::pos2(left, top),
                egui::vec2(cell_width, cell_height),
            );
            painter.rect_filled(cell_rect, 0.0, background);
            if position_selected(screen, row, col, normalized_selection) {
                painter.rect_filled(
                    cell_rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(80, 135, 230, 100),
                );
            }

            if !cell.is_wide_continuation() && cell.has_contents() {
                let text_position = cell_rect.left_top();
                painter.text(
                    text_position,
                    egui::Align2::LEFT_TOP,
                    cell.contents(),
                    font_id.clone(),
                    foreground,
                );
                if cell.bold() {
                    painter.text(
                        text_position + egui::vec2(0.5, 0.0),
                        egui::Align2::LEFT_TOP,
                        cell.contents(),
                        font_id.clone(),
                        foreground,
                    );
                }
            }
            if cell.underline() {
                painter.line_segment(
                    [
                        egui::pos2(cell_rect.left(), cell_rect.bottom() - 1.0),
                        egui::pos2(cell_rect.right(), cell_rect.bottom() - 1.0),
                    ],
                    egui::Stroke::new(1.0, foreground),
                );
            }
        }
    }

    if screen.scrollback() == 0 && !screen.hide_cursor() && blink_on {
        let (row, col) = screen.cursor_position();
        if row < visible_rows && col < visible_cols {
            let cursor_width = if screen.cell(row, col).is_some_and(vt100::Cell::is_wide) {
                cell_width * 2.0
            } else {
                cell_width
            };
            let cursor_left = rect.left() + f32::from(col) * cell_width;
            let cursor_rect = egui::Rect::from_min_size(
                egui::pos2(cursor_left, rect.top() + f32::from(row) * cell_height),
                egui::vec2(cursor_width.min(rect.right() - cursor_left), cell_height),
            );
            let color = if focused {
                egui::Color32::from_rgba_unmultiplied(235, 235, 235, 110)
            } else {
                egui::Color32::from_rgba_unmultiplied(235, 235, 235, 55)
            };
            painter.rect_filled(cursor_rect, 0.0, color);
        }
    }
}

pub(super) fn terminal_color(
    color: vt100::Color,
    default: egui::Color32,
    bold: bool,
) -> egui::Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Idx(index) => {
            let index = if bold && index < 8 { index + 8 } else { index };
            indexed_color(index)
        }
        vt100::Color::Rgb(red, green, blue) => egui::Color32::from_rgb(red, green, blue),
    }
}

pub(super) fn indexed_color(index: u8) -> egui::Color32 {
    const BASIC: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 0, 0],
        [0, 205, 0],
        [205, 205, 0],
        [0, 0, 238],
        [205, 0, 205],
        [0, 205, 205],
        [229, 229, 229],
        [127, 127, 127],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [92, 92, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    if index < 16 {
        let [red, green, blue] = BASIC[usize::from(index)];
        return egui::Color32::from_rgb(red, green, blue);
    }
    if index < 232 {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let offset = index - 16;
        let red = LEVELS[usize::from(offset / 36)];
        let green = LEVELS[usize::from((offset % 36) / 6)];
        let blue = LEVELS[usize::from(offset % 6)];
        return egui::Color32::from_rgb(red, green, blue);
    }
    let level = 8 + 10 * (index - 232);
    egui::Color32::from_gray(level)
}

pub(super) fn dim_color(color: egui::Color32) -> egui::Color32 {
    egui::Color32::from_rgba_premultiplied(
        ((f32::from(color.r()) * 0.6).round()) as u8,
        ((f32::from(color.g()) * 0.6).round()) as u8,
        ((f32::from(color.b()) * 0.6).round()) as u8,
        color.a(),
    )
}
