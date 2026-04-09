use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, HighlightSpacing, List, ListItem, Paragraph},
};

use crate::tmux::format_timestamp;
use crate::{App, Focus, HostStatus, InputMode, SidebarEntry, SIDEBAR_WIDTH};

pub fn ui(frame: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)])
        .split(frame.area());

    let sidebar_area = Rect {
        height: outer[0].height.saturating_sub(1),
        ..outer[0]
    };
    let main_area = Rect {
        height: outer[1].height.saturating_sub(1),
        ..outer[1]
    };
    let status_area = Rect {
        x: 0,
        y: frame.area().height.saturating_sub(1),
        width: frame.area().width,
        height: 1,
    };

    render_sidebar(frame, app, sidebar_area);
    render_main(frame, app, main_area);
    render_status(frame, app, status_area);

    match &app.mode {
        InputMode::NewSession(ref input) => render_new_session_dialog(frame, input),
        InputMode::AddHost(ref input) => render_add_host_dialog(frame, input),
        InputMode::ConfirmDeleteHost(ref name) => render_confirm_delete_dialog(frame, name),
        InputMode::Normal => {}
    }
}

fn render_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let active_session = app
        .terminal
        .as_ref()
        .map(|t| (t.host.as_str(), t.session_name.as_str()));

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| match entry {
            SidebarEntry::HostHeader { name, status } => {
                let status_icon = match status {
                    HostStatus::Ok => {
                        Span::styled(" ◆", Style::default().fg(Color::Green))
                    }
                    HostStatus::Fetching => {
                        Span::styled(" ◇", Style::default().fg(Color::Yellow))
                    }
                    HostStatus::Error(_) => {
                        Span::styled(" ✗", Style::default().fg(Color::Red))
                    }
                };
                let label = format!("── {} ", name);
                let line = Line::from(vec![
                    Span::styled(label, Style::default().fg(Color::DarkGray).bold()),
                    status_icon,
                ]);
                ListItem::new(vec![line])
            }
            SidebarEntry::Session(s) => {
                let is_active =
                    active_session == Some((s.host.as_str(), s.name.as_str()));
                let dot = if s.attached { " ●" } else { "" };
                let name_style = if is_active {
                    Style::default().fg(Color::Cyan).bold()
                } else {
                    Style::default().fg(Color::White)
                };

                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(&s.name, name_style),
                    Span::styled(dot, Style::default().fg(Color::Green)),
                ]);
                let meta = Line::from(Span::styled(
                    format!("  {}w · {}", s.windows, format_timestamp(s.created)),
                    Style::default().fg(Color::DarkGray),
                ));
                ListItem::new(vec![line, meta])
            }
        })
        .collect();

    let title = if focused { " ⣿ PILOT " } else { " ⣿ pilot " };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .title_style(Style::default().fg(Color::Cyan).bold())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .bold(),
        )
        .highlight_symbol("▸")
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_main(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Terminal;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };

    if app.terminal.is_some() {
        let t = app.terminal.as_ref().unwrap();
        let title = if t.host == "local" {
            format!(" {} ", t.session_name)
        } else {
            format!(" {} [{}] ", t.session_name, t.host)
        };
        let block = Block::default()
            .title(title)
            .title_style(Style::default().fg(Color::Cyan).bold())
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        app.update_term_size(inner.height, inner.width);
        app.terminal.as_ref().unwrap().render(inner, frame.buffer_mut());
    } else {
        let block = Block::default()
            .title(" terminal ")
            .title_style(Style::default().fg(Color::DarkGray))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let has_sessions = app.entries.iter().any(|e| e.is_session());
        let msg = if !has_sessions {
            "No tmux sessions.\n\nCtrl+G then 'n' to create one."
        } else {
            "Select a session and press Enter."
        };
        let p = Paragraph::new(msg)
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray));

        let offset = inner.height / 3;
        frame.render_widget(
            p,
            Rect {
                y: inner.y + offset,
                height: inner.height.saturating_sub(offset),
                ..inner
            },
        );
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let (label, color) = match app.focus {
        Focus::Terminal => (" TERMINAL ", Color::Cyan),
        Focus::Sidebar => (" SIDEBAR ", Color::Yellow),
    };

    let mut spans: Vec<Span> = vec![
        Span::styled(label, Style::default().bg(color).fg(Color::Black).bold()),
        Span::raw(" "),
    ];

    let keys: &[(&str, &str)] = match app.focus {
        Focus::Terminal => &[("^G", "Sidebar")],
        Focus::Sidebar => &[
            ("Ret", "Attach"),
            ("o", "Open"),
            ("n", "New"),
            ("x", "Kill"),
            ("a", "Host+"),
            ("d", "Host-"),
            ("r", "Refresh"),
            ("q", "Quit"),
            ("^G", "Terminal"),
        ],
    };

    for (k, d) in keys {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ));
        spans.push(Span::styled(
            format!(" {d} "),
            Style::default().fg(Color::DarkGray),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_new_session_dialog(frame: &mut Frame, input: &str) {
    let a = frame.area();
    let w = 44u16.min(a.width.saturating_sub(4));
    let h = 5u16;
    let dialog = Rect::new((a.width - w) / 2, (a.height - h) / 2, w, h);

    frame.render_widget(Clear, dialog);

    let block = Block::default()
        .title(" New Session ")
        .title_style(Style::default().fg(Color::Cyan).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!(" Name: {}▌", input)).style(Style::default().fg(Color::White)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" Enter: create · Esc: cancel").style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

fn render_add_host_dialog(frame: &mut Frame, input: &str) {
    let a = frame.area();
    let w = 52u16.min(a.width.saturating_sub(4));
    let h = 7u16;
    let dialog = Rect::new((a.width - w) / 2, (a.height - h) / 2, w, h);

    frame.render_widget(Clear, dialog);

    let block = Block::default()
        .title(" Add SSH Host ")
        .title_style(Style::default().fg(Color::Cyan).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!(" SSH: {}▌", input)).style(Style::default().fg(Color::White)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" Format: user@host or name=user@host:port")
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(" Enter: add · Esc: cancel").style(Style::default().fg(Color::DarkGray)),
        rows[4],
    );
}

fn render_confirm_delete_dialog(frame: &mut Frame, name: &str) {
    let a = frame.area();
    let w = 44u16.min(a.width.saturating_sub(4));
    let h = 5u16;
    let dialog = Rect::new((a.width - w) / 2, (a.height - h) / 2, w, h);

    frame.render_widget(Clear, dialog);

    let block = Block::default()
        .title(" Remove Host ")
        .title_style(Style::default().fg(Color::Red).bold())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!(" Remove \"{}\"?", name)).style(Style::default().fg(Color::White)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" y: confirm · Esc: cancel").style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}
