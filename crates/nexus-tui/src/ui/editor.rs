use nexus_core::buffer::Buffer;
use nexus_core::buffer::edit::BufferError;
use nexus_core::buffer::edit::TextRange;
use nexus_core::buffer::position::CharOffset;
use nexus_syntax::{HighlightSpan, SyntaxState, TokenKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::theme::ThemeState;
use crate::view::{char_width, gutter_width};

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let buffer = app.buffer();
    let theme = app.theme();

    if !buffer.is_consistent() {
        render_error(frame, area, "documento em recuperação pendente", theme);
        return;
    }

    let line_count = match buffer.line_count() {
        Ok(line_count) => line_count,
        Err(error) => {
            render_error(frame, area, &format!("buffer indisponível: {error}"), theme);
            return;
        }
    };

    let gutter = gutter_width(line_count).min(area.width);
    let rule = u16::from(area.width > gutter);

    let [gutter_area, rule_area, text_area] = Layout::horizontal([
        Constraint::Length(gutter),
        Constraint::Length(rule),
        Constraint::Min(0),
    ])
    .areas(area);

    let rows = text_area.height as usize;
    let cols = text_area.width as usize;
    let top = app.view().top_line();
    let left = app.view().left_col();
    let cursor = app.view().cursor();
    let selection = app.view().selection_range();

    let cursor_line = match buffer.line_of_char(cursor) {
        Ok(line) => line,
        Err(error) => {
            render_error(
                frame,
                text_area,
                &format!("cursor inválido: {error}"),
                theme,
            );
            return;
        }
    };

    let visible_end = line_count.min(top.saturating_add(rows));

    let numbers: Vec<Line> = (top..visible_end)
        .map(|line| {
            let style = if line == cursor_line {
                Style::new()
                    .fg(theme.foreground())
                    .bg(theme.surface())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.muted()).bg(theme.surface())
            };

            Line::from(Span::styled((line + 1).to_string(), style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(numbers)
            .alignment(Alignment::Right)
            .style(Style::new().bg(theme.surface())),
        gutter_area,
    );

    let rule_lines: Vec<Line> = (top..visible_end)
        .map(|_| {
            Line::from(Span::styled(
                "│",
                Style::new().fg(theme.border()).bg(theme.surface()),
            ))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(rule_lines).style(Style::new().bg(theme.surface())),
        rule_area,
    );

    let mut lines = Vec::with_capacity(visible_end.saturating_sub(top));
    let context = RenderContext {
        buffer,
        syntax: app.syntax(),
        theme,
        line_count,
        left,
        cols,
        cursor,
        cursor_line,
        selection,
    };

    for line in top..visible_end {
        match render_line(&context, line) {
            Ok(line) => lines.push(line),
            Err(error) => {
                render_error(
                    frame,
                    text_area,
                    &format!("linha indisponível: {error}"),
                    theme,
                );
                return;
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::new().fg(theme.foreground()).bg(theme.background())),
        text_area,
    );
}

struct RenderContext<'a> {
    buffer: &'a Buffer,
    syntax: Option<&'a SyntaxState>,
    theme: &'a ThemeState,
    line_count: usize,
    left: usize,
    cols: usize,
    cursor: CharOffset,
    cursor_line: usize,
    selection: Option<TextRange>,
}

fn render_line(context: &RenderContext<'_>, line: usize) -> Result<Line<'static>, BufferError> {
    let line_start = context.buffer.line_start(line)?;
    let line_end = context.buffer.line_end(line)?;
    let text = context.buffer.line_text(line)?;

    let highlights = context
        .syntax
        .map(|syntax| syntax.spans_for_range(TextRange::new(line_start, line_end)))
        .unwrap_or(&[]);

    let is_current = line == context.cursor_line;
    let base_style = Style::new()
        .fg(context.theme.foreground())
        .bg(if is_current {
            context.theme.current_line()
        } else {
            context.theme.background()
        });

    let limit = context.left.saturating_add(context.cols);
    let mut builder = SpanBuilder::new(base_style);
    let mut offset = line_start;
    let mut column = 0;
    let mut highlight_index = 0;

    for character in text.chars() {
        let width = char_width(character, column);
        let character_start = column;
        let character_end = column + width;

        column = character_end;

        if character_end <= context.left {
            offset = CharOffset(offset.0 + 1);
            continue;
        }

        if character_start >= limit {
            break;
        }

        let selected = context
            .selection
            .is_some_and(|range| offset.0 >= range.start.0 && offset.0 < range.end.0);
        let is_cursor = offset == context.cursor;

        let mut style = base_style;

        if let Some(kind) = token_at(highlights, offset.0, &mut highlight_index) {
            style = style.fg(context.theme.syntax_color(kind));
        }

        if selected {
            style = style.patch(context.theme.selection_style());
        }

        let visible_start = character_start.max(context.left);
        let visible_end = character_end.min(limit);
        let visible_width = visible_end.saturating_sub(visible_start);

        if visible_width == 0 {
            offset = CharOffset(offset.0 + 1);
            continue;
        }

        let clipped = character_start < context.left || character_end > limit;

        if character == '\t' || clipped {
            for cell in 0..visible_width {
                let cell_style = if is_cursor && cell == 0 {
                    cursor_style(style, context.theme)
                } else {
                    style
                };
                builder.push(" ", cell_style, 1);
            }
        } else {
            let character_style = if is_cursor {
                cursor_style(style, context.theme)
            } else {
                style
            };
            builder.push(&character.to_string(), character_style, width);
        }

        offset = CharOffset(offset.0 + 1);
    }

    let newline_selected = context
        .selection
        .is_some_and(|range| range.start.0 <= line_end.0 && range.end.0 > line_end.0)
        && line + 1 < context.line_count;

    let cursor_at_line_end = context.cursor == line_end && is_current;

    if (newline_selected || cursor_at_line_end) && builder.width() < context.cols {
        let mut style = base_style;

        if newline_selected {
            style = style.patch(context.theme.selection_style());
        }
        if cursor_at_line_end {
            style = cursor_style(style, context.theme);
        }

        builder.push(" ", style, 1);
    }

    if is_current && builder.width() < context.cols {
        let remaining = context.cols - builder.width();
        builder.push(&" ".repeat(remaining), base_style, remaining);
    }

    let (spans, _) = builder.finish();

    Ok(Line::from(spans))
}

fn token_at(highlights: &[HighlightSpan], offset: usize, index: &mut usize) -> Option<TokenKind> {
    while *index < highlights.len() && highlights[*index].range.end.0 <= offset {
        *index += 1;
    }

    highlights
        .get(*index)
        .filter(|span| span.range.start.0 <= offset && offset < span.range.end.0)
        .map(|span| span.kind)
}

fn cursor_style(style: Style, theme: &ThemeState) -> Style {
    style.patch(theme.cursor_style())
}

fn render_error(frame: &mut Frame, area: Rect, message: &str, theme: &ThemeState) {
    if area.is_empty() {
        return;
    }

    let line = Line::from(Span::styled(
        format!(" {message}"),
        Style::new().fg(theme.error()).bg(theme.background()),
    ));

    frame.render_widget(
        Paragraph::new(line).style(Style::new().bg(theme.background())),
        area,
    );
}

struct SpanBuilder {
    spans: Vec<Span<'static>>,
    text: String,
    style: Style,
    width: usize,
}

impl SpanBuilder {
    fn new(style: Style) -> Self {
        Self {
            spans: Vec::new(),
            text: String::new(),
            style,
            width: 0,
        }
    }

    fn width(&self) -> usize {
        self.width
    }

    fn push(&mut self, text: &str, style: Style, width: usize) {
        if style != self.style {
            self.flush();
            self.style = style;
        }

        self.text.push_str(text);
        self.width += width;
    }

    fn flush(&mut self) {
        if self.text.is_empty() {
            return;
        }

        let text = std::mem::take(&mut self.text);
        self.spans.push(Span::styled(text, self.style));
    }

    fn finish(mut self) -> (Vec<Span<'static>>, usize) {
        self.flush();
        (self.spans, self.width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn span_builder_groups_equal_styles() {
        let style = Style::new().fg(Color::Red);
        let mut builder = SpanBuilder::new(style);

        builder.push("a", style, 1);
        builder.push("b", style, 1);
        builder.push("c", Style::new().fg(Color::Blue), 1);

        let (spans, width) = builder.finish();

        assert_eq!(width, 3);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "ab");
        assert_eq!(spans[1].content, "c");
    }
}
