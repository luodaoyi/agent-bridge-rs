mod language;
mod render;
mod webui;

#[cfg(test)]
mod tests;

use std::{io, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::install::{self, InstallTarget, InstallationStatus, Paths};

use self::{language::Language, render::render_menu, webui::open_webui};

pub fn run() -> Result<()> {
    enable_raw_mode().context("Failed to enable raw mode")?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen).context("Failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;
    let result = run_loop(&mut terminal);
    terminal.show_cursor().ok();
    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let paths = Paths::discover()?;
    let mut status = install::status(&paths)?;
    let mut selected: usize = 0;
    let mut lang = Language::load();
    let mut message = String::from(lang.strings().msg_initial);

    loop {
        terminal.draw(|frame| render_menu(frame, &paths, &status, selected, &message, lang))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        if let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
            && handle_key(
                key,
                &mut selected,
                &mut message,
                &paths,
                &mut status,
                &mut lang,
            )?
        {
            return Ok(());
        }
    }
}

fn handle_key(
    key: KeyEvent,
    selected: &mut usize,
    message: &mut String,
    paths: &Paths,
    status: &mut InstallationStatus,
    lang: &mut Language,
) -> Result<bool> {
    let s = lang.strings();
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(4),
        KeyCode::Enter if key.kind == KeyEventKind::Press => match *selected {
            0 => match install::apply(InstallTarget::All) {
                Ok(result) => {
                    *status = install::status(paths)?;
                    *message = s.msg_install_success.replace("{}", &result.display());
                }
                Err(error) => *message = s.msg_install_failed.replace("{}", &format!("{error:#}")),
            },
            1 => match install::uninstall(InstallTarget::All) {
                Ok(true) => {
                    *status = install::status(paths)?;
                    *message = s.msg_uninstall_success.to_string();
                }
                Ok(false) => {
                    *message = s.msg_uninstall_none.to_string();
                }
                Err(error) => {
                    *message = s.msg_uninstall_failed.replace("{}", &format!("{error:#}"))
                }
            },
            2 => match open_webui(lang) {
                Ok(()) => *message = s.msg_webui_try.to_string(),
                Err(error) => *message = s.msg_webui_failed.replace("{}", &format!("{error:#}")),
            },
            3 => {
                *lang = lang.toggle();
                lang.save().ok();
                *message = lang.strings().msg_lang_switched.to_string();
            }
            _ => return Ok(true),
        },
        _ => {}
    }
    Ok(false)
}

struct RestoreTerminal;

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(io::stdout(), LeaveAlternateScreen).ok();
    }
}
