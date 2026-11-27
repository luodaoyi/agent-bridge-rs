use super::{
    input::{copy_requested, input_bytes, key_bytes, paste_bytes, text_bytes},
    paint::{DEFAULT_FOREGROUND, dim_color, indexed_color, terminal_color},
    selection::{CellPosition, Selection, normalized_selection, selection_text},
};
use eframe::egui;

#[test]
fn maps_terminal_keys_and_cursor_modes() {
    assert_eq!(
        key_bytes(egui::Key::Enter, egui::Modifiers::default(), false),
        Some(b"\r".to_vec())
    );
    assert_eq!(
        key_bytes(egui::Key::ArrowUp, egui::Modifiers::default(), false),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        key_bytes(egui::Key::ArrowUp, egui::Modifiers::default(), true),
        Some(b"\x1bOA".to_vec())
    );
    assert_eq!(
        key_bytes(
            egui::Key::Tab,
            egui::Modifiers {
                shift: true,
                ..Default::default()
            },
            false,
        ),
        Some(b"\x1b[Z".to_vec())
    );
    assert_eq!(
        key_bytes(egui::Key::F12, egui::Modifiers::default(), false),
        Some(b"\x1b[24~".to_vec())
    );
}

#[test]
fn maps_control_and_alt_keys() {
    assert_eq!(
        key_bytes(
            egui::Key::C,
            egui::Modifiers {
                ctrl: true,
                ..Default::default()
            },
            false,
        ),
        Some(vec![3])
    );
    assert_eq!(
        key_bytes(
            egui::Key::ArrowLeft,
            egui::Modifiers {
                alt: true,
                ..Default::default()
            },
            false,
        ),
        Some(b"\x1b\x1b[D".to_vec())
    );
    assert_eq!(text_bytes("中", true), b"\x1b\xe4\xb8\xad".to_vec());
}

#[test]
fn ctrl_c_copies_selection_instead_of_sending_interrupt() {
    let modifiers = egui::Modifiers {
        ctrl: true,
        ..Default::default()
    };
    let events = [egui::Event::Key {
        key: egui::Key::C,
        physical_key: Some(egui::Key::C),
        pressed: true,
        repeat: false,
        modifiers,
    }];
    assert!(copy_requested(&events));
    assert_eq!(
        input_bytes(&events, modifiers, false, false, false),
        vec![3]
    );
    assert!(input_bytes(&events, modifiers, false, false, true).is_empty());
}

#[test]
fn wraps_bracketed_paste_only_when_enabled() {
    assert_eq!(paste_bytes("a\nb", false), b"a\nb".to_vec());
    assert_eq!(
        paste_bytes("a\nb", true),
        b"\x1b[200~a\nb\x1b[201~".to_vec()
    );
}

#[test]
fn maps_indexed_and_truecolor_values() {
    assert_eq!(indexed_color(16), egui::Color32::from_rgb(0, 0, 0));
    assert_eq!(indexed_color(21), egui::Color32::from_rgb(0, 0, 255));
    assert_eq!(indexed_color(231), egui::Color32::from_rgb(255, 255, 255));
    assert_eq!(indexed_color(232), egui::Color32::from_gray(8));
    assert_eq!(indexed_color(255), egui::Color32::from_gray(238));
    assert_eq!(
        terminal_color(vt100::Color::Idx(1), DEFAULT_FOREGROUND, true),
        indexed_color(9)
    );
    assert_eq!(
        terminal_color(vt100::Color::Rgb(1, 2, 3), DEFAULT_FOREGROUND, false),
        egui::Color32::from_rgb(1, 2, 3)
    );
}

#[test]
fn dims_foreground_without_changing_alpha() {
    assert_eq!(
        dim_color(egui::Color32::from_rgba_unmultiplied(100, 150, 200, 77)),
        egui::Color32::from_rgba_unmultiplied(60, 90, 120, 77)
    );
}

#[test]
fn normalizes_and_clamps_cell_selection() {
    let selection = Selection {
        anchor: CellPosition { row: 9, col: 9 },
        head: CellPosition { row: 0, col: 2 },
    };
    assert_eq!(
        normalized_selection(selection, 3, 5),
        Some((
            CellPosition { row: 0, col: 2 },
            CellPosition { row: 2, col: 4 }
        ))
    );
    assert_eq!(normalized_selection(selection, 0, 5), None);
    assert_eq!(normalized_selection(selection, 3, 0), None);
}

#[test]
fn extracts_visible_selection_without_wide_continuation_duplicates() {
    let mut parser = vt100::Parser::new(3, 8, 0);
    parser.process("abc\r\nhello\r\n中A".as_bytes());

    assert_eq!(
        selection_text(
            parser.screen(),
            Selection {
                anchor: CellPosition { row: 1, col: 1 },
                head: CellPosition { row: 0, col: 1 },
            }
        ),
        "bc\nhe"
    );
    assert_eq!(
        selection_text(
            parser.screen(),
            Selection {
                anchor: CellPosition { row: 2, col: 0 },
                head: CellPosition { row: 2, col: 2 },
            }
        ),
        "中A"
    );
}

#[test]
fn joins_wrapped_rows_when_copying_selection() {
    let mut parser = vt100::Parser::new(2, 4, 0);
    parser.process(b"abcde");
    assert_eq!(
        selection_text(
            parser.screen(),
            Selection {
                anchor: CellPosition { row: 0, col: 0 },
                head: CellPosition { row: 1, col: 3 },
            }
        ),
        "abcde"
    );
}
