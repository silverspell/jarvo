//! Rendering. Pure layout and drawing: header, transcript, input, status, footer.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::app::{App, Role, Status, Weather};

/// Height of the input panel, borders included.
const INPUT_HEIGHT: u16 = 5;

/// Draw one frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, chat, input, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(INPUT_HEIGHT),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, header, app);
    draw_chat(frame, chat, app);
    draw_input(frame, input, app);
    draw_status(frame, status, app);
    draw_footer(frame, footer, app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let line = Line::from(vec![
        Span::styled(
            " Jarvo ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(app.header.clone(), Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_chat(frame: &mut Frame, area: Rect, app: &mut App) {
    let lines = transcript_lines(app);
    let block = Block::bordered()
        .title(" conversation ")
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });

    // Measure the wrapped height so paging and follow mode know the geometry,
    // then clamp the offset before rendering with it.
    app.viewport_height = inner.height;
    app.content_height = u16::try_from(paragraph.line_count(inner.width)).unwrap_or(u16::MAX);
    if app.follow {
        app.scroll = app.max_scroll();
    } else {
        app.scroll = app.scroll.min(app.max_scroll());
    }

    frame.render_widget(paragraph.block(block).scroll((app.scroll, 0)), area);
}

/// The transcript as styled lines, one per line of entry text.
fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in &app.entries {
        let (label, style) = match entry.role {
            Role::User => ("you", Style::default().fg(Color::Green)),
            Role::Assistant => ("assistant", Style::default().fg(Color::White)),
            Role::System => ("·", Style::default().fg(Color::DarkGray)),
        };
        debug_assert_eq!(label, entry.role.prefix());

        let mut parts = entry.text.split('\n');
        let first = parts.next().unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), style.add_modifier(Modifier::BOLD)),
            Span::styled(first.to_string(), style),
        ]));
        // Continuation lines are indented under the label.
        let indent = " ".repeat(label.chars().count() + 2);
        for part in parts {
            lines.push(Line::from(Span::styled(format!("{indent}{part}"), style)));
        }
    }
    lines
}

fn draw_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .title(if app.busy {
            " message (waiting for the model) "
        } else {
            " message "
        })
        .border_style(Style::default().fg(if app.busy {
            Color::DarkGray
        } else {
            Color::Cyan
        }));
    let inner = block.inner(area);

    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );

    if let Some((x, y)) = cursor_position(app, inner) {
        frame.set_cursor_position((x, y));
    }
}

/// Where the terminal cursor belongs inside the input panel.
///
/// Lines are wrapped at the panel width, matching the paragraph above, and the
/// cursor is hidden when it scrolls out of view.
fn cursor_position(app: &App, inner: Rect) -> Option<(u16, u16)> {
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let width = usize::from(inner.width);

    let before = &app.input[..app.cursor];
    let mut row = 0usize;
    let mut column = 0usize;
    for ch in before.chars() {
        if ch == '\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
            if column == width {
                row += 1;
                column = 0;
            }
        }
    }

    if row >= usize::from(inner.height) {
        return None;
    }
    Some((
        inner.x + u16::try_from(column).unwrap_or(0),
        inner.y + u16::try_from(row).unwrap_or(0),
    ))
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let color = match app.status {
        Status::Idle => Color::DarkGray,
        Status::Generating => Color::Yellow,
        Status::Tool(_) => Color::Magenta,
        Status::Error(_) => Color::Red,
    };
    let mut spans = vec![Span::styled(
        format!(" {}", app.status.text()),
        Style::default().fg(color),
    )];
    if !app.follow {
        spans.push(Span::styled(
            "  [scrolled back — End to follow]",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Width reserved for the context gauge, enough for a double-width glyph
/// plus `100%`.
const GAUGE_WIDTH: u16 = 9;

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    // The hints are clipped before the gauge is, so the gauge survives a
    // narrow terminal.
    let [hints, gauge] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(GAUGE_WIDTH)]).areas(area);

    let line = Line::from(Span::styled(
        " Enter send · Alt+Enter nl · PgUp/PgDn · /json /clear · Ctrl+C quit",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(line), hints);
    frame.render_widget(
        Paragraph::new(context_gauge(app)).alignment(Alignment::Right),
        gauge,
    );
}

/// The context-usage gauge: a weather glyph for the fifth of the window in
/// use, and the percentage itself.
///
/// A dimmed placeholder stands in until a provider reports usage, so an
/// unknown figure never reads as an empty context.
fn context_gauge(app: &App) -> Line<'static> {
    let Some(percent) = app.context_percent() else {
        return Line::from(Span::styled("· --% ", Style::default().fg(Color::DarkGray)));
    };

    let weather = Weather::for_percent(percent);
    let color = match weather {
        Weather::Sunny => Color::Green,
        Weather::PartlyCloudy => Color::LightGreen,
        Weather::Overcast => Color::Yellow,
        Weather::Rain => Color::LightRed,
        Weather::Storm => Color::Red,
    };
    Line::from(Span::styled(
        format!("{} {percent}% ", weather.icon()),
        Style::default().fg(color),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        App::new("ollama:llama3.2".into(), vec!["MCP: none".into()], 64_000)
    }

    #[test]
    fn transcript_indents_continuation_lines_under_the_label() {
        let mut app = app();
        app.entries.clear();
        app.push(Role::Assistant, "first\nsecond");

        let lines = transcript_lines(&app);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].spans[0].content.as_ref(), "           second");
    }

    #[test]
    fn the_cursor_wraps_with_the_input_text() {
        let mut app = app();
        let inner = Rect::new(1, 1, 10, 3);

        assert_eq!(cursor_position(&app, inner), Some((1, 1)));

        for ch in "0123456789ab".chars() {
            app.insert_char(ch);
        }
        assert_eq!(
            cursor_position(&app, inner),
            Some((3, 2)),
            "the cursor follows the wrap onto the second row"
        );

        app.input.clear();
        app.cursor = 0;
        app.insert_char('x');
        app.insert_newline();
        assert_eq!(cursor_position(&app, inner), Some((1, 2)));
    }

    #[test]
    fn the_cursor_is_hidden_in_a_zero_sized_panel() {
        assert_eq!(cursor_position(&app(), Rect::new(0, 0, 0, 0)), None);
    }

    #[test]
    fn a_full_frame_renders_and_sets_the_geometry() {
        let mut app = app();
        app.push(Role::User, "hello");
        app.push(Role::Assistant, "hi");

        let mut terminal =
            Terminal::new(TestBackend::new(60, 20)).expect("the test backend should start");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("drawing should succeed");

        assert!(app.viewport_height > 0, "the transcript height is measured");
        assert!(app.content_height > 0, "the transcript content is measured");

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Jarvo"), "the header is drawn");
        assert!(rendered.contains("ollama:llama3.2"), "the model is shown");
        assert!(rendered.contains("hello"), "the transcript is drawn");
        assert!(rendered.contains("idle"), "the status is drawn");
        assert!(rendered.contains("Ctrl+C"), "the footer is drawn");
    }

    /// The last row of a rendered frame, as text.
    fn footer_row(app: &mut App, width: u16) -> String {
        let height = 20;
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("the backend should start");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("drawing should succeed");
        let buffer = terminal.backend().buffer().clone();
        (0..width)
            .map(|x| buffer[(x, height - 1)].symbol())
            .collect()
    }

    #[test]
    fn the_footer_shows_the_gauge_without_overflowing_eighty_columns() {
        let mut app = app();
        let unknown = footer_row(&mut app, 80);
        assert_eq!(unknown.chars().count(), 80, "the row is exactly one screen");
        assert!(unknown.contains("Ctrl+C"), "the hints survive: {unknown:?}");
        assert!(
            unknown.contains("--%"),
            "usage is unknown up front: {unknown:?}"
        );

        // 32_000 of the helper's 64_000-token window is the middle fifth.
        app.apply_event(AgentEvent::ContextTokens(32_000));
        let overcast = footer_row(&mut app, 80);
        assert!(
            overcast.contains("50%"),
            "the figure is shown: {overcast:?}"
        );
        assert!(
            overcast.contains(Weather::Overcast.icon()),
            "half a window is overcast: {overcast:?}"
        );
        assert!(
            overcast.contains("Ctrl+C"),
            "the gauge does not push the hints off: {overcast:?}"
        );
    }

    #[test]
    fn a_long_transcript_scrolls_to_the_bottom_while_following() {
        let mut app = app();
        for index in 0..100 {
            app.push(Role::User, format!("line {index}"));
        }

        let mut terminal =
            Terminal::new(TestBackend::new(40, 20)).expect("the test backend should start");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("drawing should succeed");

        assert!(
            app.scroll > 0,
            "following pins the view to the newest output"
        );
        assert_eq!(app.scroll, app.max_scroll());
    }
}
