use eframe::egui;

pub(super) fn copy_requested(events: &[egui::Event]) -> bool {
    events.iter().any(|event| match event {
        egui::Event::Copy => true,
        egui::Event::Key {
            key: egui::Key::C,
            pressed: true,
            modifiers,
            ..
        } => modifiers.ctrl,
        _ => false,
    })
}

pub(super) fn input_bytes(
    events: &[egui::Event],
    modifiers: egui::Modifiers,
    application_cursor: bool,
    bracketed_paste: bool,
    has_selection: bool,
) -> Vec<u8> {
    let has_paste = events
        .iter()
        .any(|event| matches!(event, egui::Event::Paste(_)));
    let mut output = Vec::new();

    for event in events {
        match event {
            egui::Event::Text(text) => output.extend(text_bytes(text, modifiers.alt)),
            egui::Event::Paste(text) => output.extend(paste_bytes(text, bracketed_paste)),
            egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                output.extend(text_bytes(text, modifiers.alt));
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if !(modifiers.ctrl
                && ((has_paste && *key == egui::Key::V)
                    || (has_selection && *key == egui::Key::C))) =>
            {
                if let Some(bytes) = key_bytes(*key, *modifiers, application_cursor) {
                    output.extend(bytes);
                }
            }
            _ => {}
        }
    }
    output
}

pub(super) fn text_bytes(text: &str, alt: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + usize::from(alt));
    if alt {
        bytes.push(0x1b);
    }
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

pub(super) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut bytes = Vec::with_capacity(text.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

pub(super) fn key_bytes(
    key: egui::Key,
    modifiers: egui::Modifiers,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    let bytes: &[u8] = if modifiers.ctrl {
        if let Some(control) = control_byte(key) {
            return Some(with_alt(vec![control], modifiers.alt));
        }
        match key {
            egui::Key::Space => return Some(with_alt(vec![0], modifiers.alt)),
            _ => key_sequence(key, modifiers.shift, application_cursor)?,
        }
    } else {
        key_sequence(key, modifiers.shift, application_cursor)?
    };
    Some(with_alt(bytes.to_vec(), modifiers.alt))
}

fn key_sequence(key: egui::Key, shift: bool, application_cursor: bool) -> Option<&'static [u8]> {
    let application = application_cursor;
    match key {
        egui::Key::Enter => Some(b"\r"),
        egui::Key::Tab if shift => Some(b"\x1b[Z"),
        egui::Key::Tab => Some(b"\t"),
        egui::Key::Backspace => Some(b"\x7f"),
        egui::Key::Escape => Some(b"\x1b"),
        egui::Key::ArrowUp if application => Some(b"\x1bOA"),
        egui::Key::ArrowDown if application => Some(b"\x1bOB"),
        egui::Key::ArrowRight if application => Some(b"\x1bOC"),
        egui::Key::ArrowLeft if application => Some(b"\x1bOD"),
        egui::Key::ArrowUp => Some(b"\x1b[A"),
        egui::Key::ArrowDown => Some(b"\x1b[B"),
        egui::Key::ArrowRight => Some(b"\x1b[C"),
        egui::Key::ArrowLeft => Some(b"\x1b[D"),
        egui::Key::Home if application => Some(b"\x1bOH"),
        egui::Key::End if application => Some(b"\x1bOF"),
        egui::Key::Home => Some(b"\x1b[H"),
        egui::Key::End => Some(b"\x1b[F"),
        egui::Key::Insert => Some(b"\x1b[2~"),
        egui::Key::Delete => Some(b"\x1b[3~"),
        egui::Key::PageUp => Some(b"\x1b[5~"),
        egui::Key::PageDown => Some(b"\x1b[6~"),
        egui::Key::F1 => Some(b"\x1bOP"),
        egui::Key::F2 => Some(b"\x1bOQ"),
        egui::Key::F3 => Some(b"\x1bOR"),
        egui::Key::F4 => Some(b"\x1bOS"),
        egui::Key::F5 => Some(b"\x1b[15~"),
        egui::Key::F6 => Some(b"\x1b[17~"),
        egui::Key::F7 => Some(b"\x1b[18~"),
        egui::Key::F8 => Some(b"\x1b[19~"),
        egui::Key::F9 => Some(b"\x1b[20~"),
        egui::Key::F10 => Some(b"\x1b[21~"),
        egui::Key::F11 => Some(b"\x1b[23~"),
        egui::Key::F12 => Some(b"\x1b[24~"),
        _ => None,
    }
}

fn control_byte(key: egui::Key) -> Option<u8> {
    match key {
        egui::Key::A => Some(1),
        egui::Key::B => Some(2),
        egui::Key::C => Some(3),
        egui::Key::D => Some(4),
        egui::Key::E => Some(5),
        egui::Key::F => Some(6),
        egui::Key::G => Some(7),
        egui::Key::H => Some(8),
        egui::Key::I => Some(9),
        egui::Key::J => Some(10),
        egui::Key::K => Some(11),
        egui::Key::L => Some(12),
        egui::Key::M => Some(13),
        egui::Key::N => Some(14),
        egui::Key::O => Some(15),
        egui::Key::P => Some(16),
        egui::Key::Q => Some(17),
        egui::Key::R => Some(18),
        egui::Key::S => Some(19),
        egui::Key::T => Some(20),
        egui::Key::U => Some(21),
        egui::Key::V => Some(22),
        egui::Key::W => Some(23),
        egui::Key::X => Some(24),
        egui::Key::Y => Some(25),
        egui::Key::Z => Some(26),
        _ => None,
    }
}

fn with_alt(mut bytes: Vec<u8>, alt: bool) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}
