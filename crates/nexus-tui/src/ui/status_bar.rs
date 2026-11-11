use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, MessageKind, Mode};
use crate::theme::ThemeState;
use crate::view::char_width;

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let theme = app.theme();
    let (mode_label, mode_style) = mode_style(app, theme);

    let mut left_spans = vec![Span::styled(mode_label, mode_style)];

    if let Some(message) = app.status_message() {
        let style = match message.kind {
            MessageKind::Info => Style::new().fg(theme.success()),
            MessageKind::Error => Style::new().fg(theme.error()),
        };
        left_spans.push(Span::styled(format!("  {}", message.text), style));
    } else {
        left_spans.push(Span::styled(
            "  Ctrl+O abrir · Ctrl+S salvar · Ctrl+H ajuda",
            Style::new().fg(theme.muted()),
        ));
    }

    let position = if app.buffer().is_consistent() {
        match (
            app.view().cursor_line(app.buffer()),
            app.view().cursor_visual_column(app.buffer()),
        ) {
            (Ok(line), Ok(column)) => format!("{}:{}", line + 1, column + 1),
            _ => "?:?".to_owned(),
        }
    } else {
        "?:?".to_owned()
    };

    let line_count = if app.buffer().is_consistent() {
        app.buffer()
            .line_count()
            .map(|lines| lines.to_string())
            .unwrap_or_else(|_| "?".to_owned())
    } else {
        "?".to_owned()
    };

    let saved_state = if app.buffer().is_modified() {
        "modificado"
    } else {
        "salvo"
    };

    let (width, height) = app.terminal_size();
    let right_text = format!(
        "{} · {} · {} · {} linhas · rev {} · {}×{}",
        app.document_name(),
        position,
        saved_state,
        line_count,
        app.buffer().revision(),
        width,
        height
    );

    let right_width = right_text
        .chars()
        .map(|character| char_width(character, 0))
        .sum::<usize>()
        .min(area.width as usize) as u16;

    // Em terminais estreitos, reserva espaço para o modo e a mensagem de
    // status antes das informações secundárias da direita. Sem isso, a
    // mensagem de abertura/salvamento poderia desaparecer por completo.
    const MIN_LEFT_WIDTH: u16 = 24;
    let right_width = right_width.min(area.width.saturating_sub(MIN_LEFT_WIDTH));

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(left_spans)).style(
            Style::new()
                .bg(theme.status_bar())
                .fg(theme.status_bar_text()),
        ),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            right_text,
            Style::new().fg(theme.status_bar_text()),
        )))
        .alignment(Alignment::Right)
        .style(Style::new().bg(theme.status_bar())),
        right_area,
    );
}

fn mode_style(app: &App, theme: &ThemeState) -> (&'static str, Style) {
    let label = match app.mode() {
        Mode::Prompt(_) => " PROMPT ",
        Mode::Help => " AJUDA ",
        Mode::Confirm => " CONFIRMAR ",
        Mode::Editor => {
            if app.buffer().is_consistent() {
                " EDIT "
            } else {
                " RECOVERY "
            }
        }
    };

    let style = match app.mode() {
        Mode::Editor if !app.buffer().is_consistent() => Style::new()
            .fg(theme.background())
            .bg(theme.error())
            .add_modifier(Modifier::BOLD),
        Mode::Prompt(_) | Mode::Confirm => Style::new()
            .fg(theme.background())
            .bg(theme.warning())
            .add_modifier(Modifier::BOLD),
        Mode::Help | Mode::Editor => Style::new()
            .fg(theme.background())
            .bg(theme.accent())
            .add_modifier(Modifier::BOLD),
    };

    (label, style)
}
