use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Focus, Mode};

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    draw_expression(frame, app, chunks[0]);
    draw_diff(frame, app, chunks[1]);
    draw_status(frame, app, chunks[2]);

    if matches!(app.mode, Mode::ConfirmApply) {
        draw_confirm(frame, app, area);
    }
}

fn draw_expression(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Expression && app.mode == Mode::Editing;
    let border = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = if focused {
        " expression (Tab: diff, paste escapes / \\ &) "
    } else {
        " expression "
    };

    let display = if app.expression.is_empty() && !focused {
        Line::from(Span::styled(
            "s/foo/bar/",
            Style::default().fg(Color::DarkGray),
        ))
    } else if app.expression.is_empty() {
        Line::from(Span::styled(
            " ",
            Style::default().bg(Color::Cyan).fg(Color::Black),
        ))
    } else {
        let cursor = app.cursor.min(app.expression.len());
        let before = &app.expression[..cursor];
        let after = &app.expression[cursor..];
        let mut spans = vec![Span::raw(before.to_string())];
        if focused {
            if after.is_empty() {
                spans.push(Span::styled(
                    " ",
                    Style::default().bg(Color::Cyan).fg(Color::Black),
                ));
            } else {
                let mut chars = after.chars();
                let ch = chars.next().unwrap();
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().bg(Color::Cyan).fg(Color::Black),
                ));
                spans.push(Span::raw(chars.collect::<String>()));
            }
        } else {
            spans.push(Span::raw(after.to_string()));
        }
        Line::from(spans)
    };

    let widget = Paragraph::new(display).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(title),
    );
    frame.render_widget(widget, area);
}

fn draw_diff(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Diff && app.mode == Mode::Editing;
    let border = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = if focused {
        " diff (j/k scroll, Tab: expression) "
    } else {
        " diff "
    };

    let height = area.height.saturating_sub(2) as usize;
    let lines = if app.diff_lines.is_empty() {
        vec![Line::from(Span::styled(
            if app.files.is_empty() {
                " No files found under . (or pass FILE args)"
            } else {
                " Type a sed expression to preview changes"
            },
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        let start = app.scroll.min(app.diff_lines.len().saturating_sub(1));
        let end = (start + height.max(1)).min(app.diff_lines.len());
        app.diff_lines[start..end].to_vec()
    };

    let widget = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border)
                .title(title),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let mut parts = Vec::new();

    if app.loading {
        parts.push(Span::styled("… ", Style::default().fg(Color::Yellow)));
    }

    if let Some(err) = &app.error {
        parts.push(Span::styled(
            format!("error: {err}  "),
            Style::default().fg(Color::Red),
        ));
    } else if let Some(msg) = &app.status_message {
        parts.push(Span::styled(
            format!("{msg}  "),
            Style::default().fg(Color::Green),
        ));
    }

    if app.preview_scanned > 0 {
        parts.push(Span::styled(
            format!(
                "changed:{}/{} scanned:{}  ",
                app.preview_shown, app.preview_changed, app.preview_scanned
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }

    if app.truncated {
        parts.push(Span::styled(
            "preview truncated  ",
            Style::default().fg(Color::Yellow),
        ));
    }

    let backup = match &app.backup_suffix {
        Some(s) if s.is_empty() => "backup:off".to_string(),
        Some(s) => format!("backup:{s}"),
        None => "backup:off".to_string(),
    };

    parts.push(Span::styled(
        format!(
            "files:{}  sed:{}  {backup}  | Ctrl+S apply  Esc quit",
            app.files.len(),
            app.sed_bin.display()
        ),
        Style::default().fg(Color::DarkGray),
    ));

    frame.render_widget(Paragraph::new(Line::from(parts)), area);
}

fn draw_confirm(frame: &mut Frame, app: &App, area: Rect) {
    let width = 60u16.min(area.width.saturating_sub(4));
    let height = 5u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    let suffix = match &app.backup_suffix {
        Some(s) if s.is_empty() => "-i".to_string(),
        Some(s) => format!("-i{s}"),
        None => "-i".to_string(),
    };

    let text = vec![
        Line::from(format!(
            "Apply to {} file(s) with sed {suffix} -e … ?",
            app.files.len()
        )),
        Line::from(""),
        Line::from(Span::styled(
            "y: yes    n/Esc: cancel",
            Style::default().fg(Color::Yellow),
        )),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta))
                .title(" confirm apply "),
        ),
        popup,
    );
}
