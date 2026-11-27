use super::*;

fn empty_status() -> InstallationStatus {
    InstallationStatus {
        binary_installed: false,
        binary_current: false,
        codex_skill_installed: false,
        agy_plugin_installed: false,
        grok_hooks_configured: false,
        codex_hooks_configured: false,
        legacy_grok_bridge_path: None,
    }
}

#[test]
fn default_language_is_zh_cn() {
    let lang = Language::ZhCn;
    let s = lang.strings();
    assert!(s.action_exit.contains("退出"));
}

#[test]
fn language_toggle_works() {
    assert_eq!(Language::ZhCn.toggle(), Language::En);
    assert_eq!(Language::En.toggle(), Language::ZhCn);
}

#[test]
fn language_switch_changes_menu_labels() {
    let zh = Language::ZhCn.strings();
    let en = Language::En.strings();

    assert!(zh.action_install.contains("安装"));
    assert!(en.action_install.contains("Install"));

    assert!(zh.action_exit.contains("退出"));
    assert!(en.action_exit.contains("Exit"));

    assert_ne!(zh.action_install, en.action_install);
    assert_ne!(zh.action_exit, en.action_exit);
}

#[test]
fn handle_key_quit_on_q() {
    let mut selected = 0;
    let mut message = String::new();
    let paths = Paths::discover().unwrap();
    let mut status = empty_status();
    let mut lang = Language::ZhCn;

    let key = KeyEvent::new(KeyCode::Char('q'), event::KeyModifiers::NONE);
    let should_exit = handle_key(
        key,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert!(should_exit, "pressing 'q' should exit");
}

#[test]
fn handle_key_quit_on_esc() {
    let mut selected = 0;
    let mut message = String::new();
    let paths = Paths::discover().unwrap();
    let mut status = empty_status();
    let mut lang = Language::ZhCn;

    let key = KeyEvent::new(KeyCode::Esc, event::KeyModifiers::NONE);
    let should_exit = handle_key(
        key,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert!(should_exit, "pressing Esc should exit");
}

#[test]
fn handle_key_navigation() {
    let mut selected = 1;
    let mut message = String::new();
    let paths = Paths::discover().unwrap();
    let mut status = empty_status();
    let mut lang = Language::ZhCn;

    let key_up = KeyEvent::new(KeyCode::Up, event::KeyModifiers::NONE);
    handle_key(
        key_up,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 0, "up arrow should decrease selection");

    let key_down = KeyEvent::new(KeyCode::Down, event::KeyModifiers::NONE);
    handle_key(
        key_down,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 1, "down arrow should increase selection");

    let key_j = KeyEvent::new(KeyCode::Char('j'), event::KeyModifiers::NONE);
    handle_key(
        key_j,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 2, "'j' should increase selection");

    let key_k = KeyEvent::new(KeyCode::Char('k'), event::KeyModifiers::NONE);
    handle_key(
        key_k,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 1, "'k' should decrease selection");
}

#[test]
fn handle_key_up_at_zero_stays_at_bound() {
    let mut selected = 0;
    let mut message = String::new();
    let paths = Paths::discover().unwrap();
    let mut status = empty_status();
    let mut lang = Language::ZhCn;

    let key_up = KeyEvent::new(KeyCode::Up, event::KeyModifiers::NONE);
    handle_key(
        key_up,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 0, "up arrow at index 0 should stay at 0");

    let key_k = KeyEvent::new(KeyCode::Char('k'), event::KeyModifiers::NONE);
    handle_key(
        key_k,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 0, "'k' at index 0 should stay at 0");
}

#[test]
fn handle_key_down_at_last_item_stays_at_bound() {
    let mut selected = 4;
    let mut message = String::new();
    let paths = Paths::discover().unwrap();
    let mut status = empty_status();
    let mut lang = Language::ZhCn;

    let key_down = KeyEvent::new(KeyCode::Down, event::KeyModifiers::NONE);
    handle_key(
        key_down,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 4, "down arrow at last item should stay");

    let key_j = KeyEvent::new(KeyCode::Char('j'), event::KeyModifiers::NONE);
    handle_key(
        key_j,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert_eq!(selected, 4, "'j' at last item should stay");
}

#[test]
fn handle_key_exit_action() {
    let mut selected = 4;
    let mut message = String::new();
    let paths = Paths::discover().unwrap();
    let mut status = empty_status();
    let mut lang = Language::ZhCn;

    let key = KeyEvent::new(KeyCode::Enter, event::KeyModifiers::NONE);
    let should_exit = handle_key(
        key,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();
    assert!(
        should_exit,
        "selecting exit action (4) and pressing Enter should exit"
    );
}

#[test]
fn handle_key_language_switch() {
    let mut selected = 3;
    let mut message = String::new();
    let paths = Paths::discover().unwrap();
    let mut status = empty_status();
    let mut lang = Language::ZhCn;

    let key = KeyEvent::new(KeyCode::Enter, event::KeyModifiers::NONE);
    handle_key(
        key,
        &mut selected,
        &mut message,
        &paths,
        &mut status,
        &mut lang,
    )
    .unwrap();

    assert_eq!(lang, Language::En, "language should switch to English");
    assert!(
        message.contains("Language switched") || message.contains("语言已切换"),
        "message should indicate language switch"
    );
}
