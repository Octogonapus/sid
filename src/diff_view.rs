use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

/// Build ratatui lines for a git-style unified hunk view (changed regions only).
/// Returns an empty vec when `old` and `new` are identical.
pub fn diff_lines(old: &str, new: &str, context: usize) -> Vec<Line<'static>> {
    if old == new {
        return Vec::new();
    }

    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();

    for (idx, group) in diff.grouped_ops(context).into_iter().enumerate() {
        if idx > 0 {
            out.push(Line::from(""));
        }

        if let Some((old_start, new_start, old_count, new_count)) = hunk_header(&group) {
            out.push(Line::from(Span::styled(
                format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        for op in &group {
            for change in diff.iter_changes(op) {
                let (prefix, style) = match change.tag() {
                    ChangeTag::Delete => (
                        "-",
                        Style::default().fg(Color::Red).bg(Color::Rgb(40, 0, 0)),
                    ),
                    ChangeTag::Insert => (
                        "+",
                        Style::default()
                            .fg(Color::Green)
                            .bg(Color::Rgb(0, 40, 0)),
                    ),
                    ChangeTag::Equal => (" ", Style::default().fg(Color::DarkGray)),
                };
                let mut text = change.to_string();
                if text.ends_with('\n') {
                    text.pop();
                    if text.ends_with('\r') {
                        text.pop();
                    }
                }
                out.push(Line::from(Span::styled(
                    format!("{prefix}{text}"),
                    style,
                )));
            }
        }
    }

    out
}

fn hunk_header(group: &[similar::DiffOp]) -> Option<(usize, usize, usize, usize)> {
    let first = group.first()?;
    let last = group.last()?;

    let old_start = first.old_range().start + 1;
    let new_start = first.new_range().start + 1;
    let old_count = last.old_range().end.saturating_sub(first.old_range().start);
    let new_count = last.new_range().end.saturating_sub(first.new_range().start);

    Some((
        old_start.max(1),
        new_start.max(1),
        old_count.max(1),
        new_count.max(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_substitution() {
        let lines = diff_lines("hello world\n", "hello rust\n", 3);
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("-hello world"))));
        assert!(lines.iter().any(|l| l.spans.iter().any(|s| s.content.contains("+hello rust"))));
    }

    #[test]
    fn empty_when_unchanged() {
        assert!(diff_lines("same\n", "same\n", 3).is_empty());
    }
}
