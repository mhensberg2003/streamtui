//! Rendering. Four rows: title (with the session tabs), body, pending
//! fetches, status.

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

/// Longest session name shown on a tab before it is cut short.
const TAB_NAME_MAX: usize = 18;

fn draw_header(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let playing = app.playing.len();
    let right = if playing == 0 {
        String::new()
    } else {
        format!("  ▶ {playing} playing")
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(" streamtui ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(right, Style::default().fg(Color::Green)),
    ])];
    // One session needs no tab strip — the file list already names it.
    if app.order.len() > 1 {
        lines.push(session_tabs(app, area.width));
    }
    frame.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::BOTTOM)), area);
}

/// The tab strip. When the sessions do not all fit, the ends are dropped to
/// keep a window around the current one, and `‹ ›` mark what was cut.
fn session_tabs(app: &App, width: u16) -> Line<'static> {
    let tabs: Vec<(bool, String)> = app
        .order
        .iter()
        .filter_map(|hash| {
            let session = app.sessions.get(hash)?;
            let mark = if app.pending.contains_key(hash) { "⟳ " } else { "" };
            let label = format!(" {mark}{} ", truncate(&session.name, TAB_NAME_MAX));
            Some((app.current.as_deref() == Some(hash.as_str()), label))
        })
        .collect();
    if tabs.is_empty() {
        return Line::from("");
    }

    let widths: Vec<usize> = tabs.iter().map(|(_, label)| label.chars().count()).collect();
    let active = tabs.iter().position(|(current, _)| *current).unwrap_or(0);
    let (first, last) = visible_window(&widths, active, usize::from(width));

    let mut spans = Vec::new();
    if first > 0 {
        spans.push(Span::styled("‹", Style::default().fg(Color::DarkGray)));
    }
    for (current, label) in &tabs[first..=last] {
        let style = if *current {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label.clone(), style));
    }
    if last + 1 < tabs.len() {
        spans.push(Span::styled("›", Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// Widest inclusive range around `active` that fits in `budget` columns. Grows
/// right first, then left, so the sessions after the current one stay visible.
/// The active tab is always included, even when it alone overflows.
fn visible_window(widths: &[usize], active: usize, budget: usize) -> (usize, usize) {
    let total: usize = widths.iter().sum();
    // Two columns are held back for the `‹` and `›` markers, but only when
    // something actually has to be cut.
    let budget = if total <= budget { total } else { budget.saturating_sub(2) };

    let (mut first, mut last) = (active, active);
    let mut used = widths[active];
    let mut grow_right = true;
    loop {
        let left = first.checked_sub(1);
        let right = (last + 1 < widths.len()).then_some(last + 1);
        let ordered = if grow_right { [right, left] } else { [left, right] };
        let next = ordered
            .into_iter()
            .flatten()
            .find(|&index| used + widths[index] <= budget);
        let Some(next) = next else { break };
        used += widths[next];
        if next > last { last = next } else { first = next }
        grow_right = !grow_right;
    }
    (first, last)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
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
    let Some(hash) = app.current.clone() else { return };
    let Some(session) = app.sessions.get_mut(&hash) else {
        return;
    };
    let title = format!(
        " {} {}",
        session.name,
        if session.filtered { "" } else { "(unfiltered) " }
    );
    let block = Block::default().borders(Borders::ALL).title(title);

    // Switching can land on a session whose files are not known yet.
    if session.files.is_empty() {
        let waiting = Line::from(Span::styled(
            "no files yet — still fetching",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(Paragraph::new(waiting).block(block), area);
        return;
    }

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
        .block(block)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, area, &mut session.list);
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
    // The tab hint only earns its space once there is somewhere to switch to.
    let many = app.order.len() > 1;
    let keys = match app.mode {
        Mode::Entry if many => "enter play  ctrl+v paste  tab session  esc back",
        Mode::Entry => "enter play  ctrl+v paste  esc back",
        Mode::Browse if many => "↑↓/jk move  enter play  tab session  x close  n new  q quit",
        Mode::Browse => "↑↓/jk move  enter play  x close  n new magnet  q quit",
        Mode::ConfirmFetch => "y fetch  n cancel",
    };
    frame.render_widget(
        Paragraph::new(vec![status, Line::from(Span::styled(keys, Style::default().fg(Color::DarkGray)))]),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tab_is_shown_when_they_all_fit() {
        assert_eq!(visible_window(&[10, 10, 10], 0, 40), (0, 2));
        assert_eq!(visible_window(&[10, 10, 10], 2, 30), (0, 2));
    }

    #[test]
    fn a_tight_strip_keeps_a_window_around_the_active_tab() {
        // 22 columns, less 2 for the markers, fits two 10-wide tabs.
        assert_eq!(visible_window(&[10, 10, 10, 10], 2, 22), (2, 3));
        // Nothing left to grow into on the right, so it grows left instead.
        assert_eq!(visible_window(&[10, 10, 10, 10], 3, 22), (2, 3));
    }

    #[test]
    fn the_active_tab_survives_even_when_it_alone_overflows() {
        assert_eq!(visible_window(&[30, 30, 30], 1, 10), (1, 1));
    }

    #[test]
    fn truncate_marks_what_it_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a-very-long-name", 6), "a-ver…");
        // Counts characters, not bytes.
        assert_eq!(truncate("æøå-film", 4), "æøå…");
    }
}
