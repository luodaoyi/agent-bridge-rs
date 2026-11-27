use eframe::egui;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CellPosition {
    pub(super) row: u16,
    pub(super) col: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Selection {
    pub(super) anchor: CellPosition,
    pub(super) head: CellPosition,
}

pub(super) fn cell_position(
    rect: egui::Rect,
    position: egui::Pos2,
    cell_width: f32,
    cell_height: f32,
    rows: u16,
    cols: u16,
) -> Option<CellPosition> {
    if rows == 0 || cols == 0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return None;
    }
    let x = (position.x - rect.left()).clamp(0.0, rect.width().max(1.0) - f32::EPSILON);
    let y = (position.y - rect.top()).clamp(0.0, rect.height().max(1.0) - f32::EPSILON);
    Some(CellPosition {
        row: ((y / cell_height).floor() as u16).min(rows - 1),
        col: ((x / cell_width).floor() as u16).min(cols - 1),
    })
}

pub(super) fn canonical_position(
    parser: Option<&vt100::Parser>,
    mut position: CellPosition,
) -> CellPosition {
    if parser.is_some_and(|parser| {
        parser
            .screen()
            .cell(position.row, position.col)
            .is_some_and(vt100::Cell::is_wide_continuation)
    }) {
        position.col = position.col.saturating_sub(1);
    }
    position
}

pub(super) fn normalized_selection(
    selection: Selection,
    rows: u16,
    cols: u16,
) -> Option<(CellPosition, CellPosition)> {
    if rows == 0 || cols == 0 {
        return None;
    }
    let clamp = |position: CellPosition| CellPosition {
        row: position.row.min(rows - 1),
        col: position.col.min(cols - 1),
    };
    let anchor = clamp(selection.anchor);
    let head = clamp(selection.head);
    if (anchor.row, anchor.col) <= (head.row, head.col) {
        Some((anchor, head))
    } else {
        Some((head, anchor))
    }
}

pub(super) fn position_selected(
    screen: &vt100::Screen,
    row: u16,
    col: u16,
    normalized: Option<(CellPosition, CellPosition)>,
) -> bool {
    let Some((start, end)) = normalized else {
        return false;
    };
    let col = if screen
        .cell(row, col)
        .is_some_and(vt100::Cell::is_wide_continuation)
    {
        col.saturating_sub(1)
    } else {
        col
    };
    (start.row, start.col) <= (row, col) && (row, col) <= (end.row, end.col)
}

pub(super) fn selection_text(screen: &vt100::Screen, selection: Selection) -> String {
    let (rows, cols) = screen.size();
    let Some((start, end)) = normalized_selection(selection, rows, cols) else {
        return String::new();
    };
    let mut output = String::new();

    for row in start.row..=end.row {
        let first_col = if row == start.row { start.col } else { 0 };
        let last_col = if row == end.row { end.col } else { cols - 1 };
        let row_start = output.len();
        for col in first_col..=last_col {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            if cell.has_contents() {
                output.push_str(cell.contents());
            } else {
                output.push(' ');
            }
        }
        while output.len() > row_start && output.ends_with(' ') {
            output.pop();
        }
        if row != end.row && !screen.row_wrapped(row) {
            output.push('\n');
        }
    }
    output
}
