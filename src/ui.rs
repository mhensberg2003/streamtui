//! Rendering. Four rows: title, body, pending fetches, status.

use crate::app::{App, Mode};
use crate::files::human_size;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let pending_height = if app.pending.is_empty() { 0 } else { app.pending.len() as u16 + 2 };
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(pending_height),
        Constraint::Length(2),
    ])
    .split(frame.area());

    draw_header(frame, areas[0], app);
    match app.mode {
        Mode::Entry => draw_entry(frame, areas[1], app),
        Mode::Browse => draw_files(frame, areas[1], app),
        Mode::ConfirmFetch => draw_confirm(frame, areas[1], app),
    }
    if pending_height > 0 {
        draw_pending(frame, areas[2], app);
    }
    draw_status(frame, areas[3], app);
}

fn draw_header(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let playing = app.playing.len();
    let right = if playing == 0 {
        String::new()
    } else {
        format!("  ▶ {playing} playing")
    };
    let title = Line::from(vec![
        Span::styled(" streamtui ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(right, Style::default().fg(Color::Green)),
    ]);
    frame.render_widget(Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM)), area);
}

fn draw_entry(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).title(" magnet link ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.input.is_empty() {
        Span::styled("magnet:?xt=urn:btih:…", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(app.input.as_str())
    };
    frame.render_widget(Paragraph::new(Line::from(text)).wrap(Wrap { trim: false }), inner);
}

fn draw_files(frame: &mut Frame, area: ratatui::layout::Rect, app: &mut App) {
    let Some(session) = app.current.as_ref().and_then(|h| app.sessions.get(h)) else {
        return;
    };
    let title = format!(
        " {} {}",
        session.name,
        if session.filtered { "" } else { "(unfiltered) " }
    );
    let items: Vec<ListItem> = session
        .files
        .iter()
        .map(|file| {
            ListItem::new(Line::from(vec![
                Span::raw(file.short_name().to_string()),
                Span::styled(
                    format!("  {}", human_size(file.size)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, area, &mut app.list);
}

fn draw_confirm(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let name = app.current.as_ref().and_then(|h| app.sessions.get(h)).map(|s| s.name.clone()).unwrap_or_default();
    let body = vec![
        Line::from(Span::styled(name, Style::default().add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from("Not cached on TorBox. Fetching costs one of 60 uncached"),
        Line::from("fetches per hour and one download slot, and can take minutes."),
        Line::from(""),
        Line::from(Span::styled("Fetch it?  [y] yes   [n] no", Style::default().fg(Color::Yellow))),
    ];
    frame.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(" not cached ")),
        area,
    );
}

fn draw_pending(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let lines: Vec<Line> = app
        .pending
        .values()
        .map(|p| {
            Line::from(vec![
                Span::styled("⟳ ", Style::default().fg(Color::Yellow)),
                Span::raw(p.name.clone()),
                Span::styled(
                    format!("  {:.0}%  {}", p.progress * 100.0, p.state),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" fetching ")),
        area,
    );
}

fn draw_status(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let status = match &app.error {
        Some(err) => Line::from(Span::styled(format!("✗ {err}"), Style::default().fg(Color::Red))),
        None => Line::from(Span::raw(app.status.clone())),
    };
    let keys = match app.mode {
        Mode::Entry => "enter play  ctrl+v paste  esc back",
        Mode::Browse => "↑↓/jk move  enter play  n new magnet  q quit",
        Mode::ConfirmFetch => "y fetch  n cancel",
    };
    frame.render_widget(
        Paragraph::new(vec![status, Line::from(Span::styled(keys, Style::default().fg(Color::DarkGray)))]),
        area,
    );
}
