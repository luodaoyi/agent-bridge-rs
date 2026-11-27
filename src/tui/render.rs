use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::install::{InstallationStatus, Paths};

use super::language::Language;

pub(super) fn render_menu(
    frame: &mut Frame,
    paths: &Paths,
    status: &InstallationStatus,
    selected: usize,
    message: &str,
    lang: Language,
) {
    let s = lang.strings();
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let title = Paragraph::new(s.title)
        .alignment(Alignment::Center)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, areas[0]);

    let hooks_summary = format!(
        "Grok: {} · Codex: {}",
        if status.grok_hooks_configured {
            s.configured
        } else {
            s.not_configured
        },
        if status.codex_hooks_configured {
            s.configured
        } else {
            s.not_configured
        }
    );
    let mut status_text = vec![
        Line::from(status_line(
            s.status_label,
            status.display(),
            status.binary_current && status.hooks_configured(),
        )),
        Line::from(status_line(
            s.binary_label,
            if status.binary_current {
                s.ready
            } else {
                s.needs_update
            },
            status.binary_current,
        )),
        Line::from(status_line(
            s.hooks_label,
            &hooks_summary,
            status.hooks_configured(),
        )),
        Line::from(format!("{}：{}", s.target_path, paths.agy_binary.display())),
    ];
    if let Some(legacy) = &status.legacy_grok_bridge_path {
        status_text.push(Line::from(Span::styled(
            format!("提示：发现旧版 {legacy}，可手动删除"),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(
        Paragraph::new(status_text)
            .block(
                Block::default()
                    .title(s.status_section)
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        areas[1],
    );

    let actions = [
        s.action_install,
        s.action_uninstall,
        s.action_webui,
        s.action_language,
        s.action_exit,
    ];
    let items: Vec<ListItem> = actions
        .iter()
        .map(|action| ListItem::new(*action))
        .collect();
    let actions_widget = List::new(items)
        .block(
            Block::default()
                .title(s.actions_section)
                .borders(Borders::ALL),
        )
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut list_state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(actions_widget, areas[2], &mut list_state);

    frame.render_widget(
        Paragraph::new(message)
            .block(
                Block::default()
                    .title(s.result_section)
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        areas[3],
    );

    frame.render_widget(
        Paragraph::new(s.help_bar)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        areas[4],
    );
}

fn status_line(label: &str, value: &str, ready: bool) -> Vec<Span<'static>> {
    vec![
        Span::raw(format!("{label}：")),
        Span::styled(
            value.to_owned(),
            Style::default().fg(if ready { Color::Green } else { Color::Yellow }),
        ),
    ]
}
