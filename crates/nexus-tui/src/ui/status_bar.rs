use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use nexus_lsp::protocol::DiagnosticSeverity;

use crate::app::{App, MessageKind, Mode};
use crate::diagnostics;
use crate::theme::ThemeState;
use crate::view::char_width;

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let theme = app.theme();
    let left = left_content(app, theme);
    let right_text = right_content(app);

    let right_width = right_text
        .chars()
        .map(|character| char_width(character, 0))
        .sum::<usize>()
        .min(area.width as usize) as u16;

    // A mensagem operacional tem prioridade sobre informações secundárias em
    // terminais estreitos.
    const MIN_LEFT_WIDTH: u16 = 24;
    let right_width = right_width.min(area.width.saturating_sub(MIN_LEFT_WIDTH));

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(left)).style(
            Style::new()
                .bg(theme.status_bar())
                .fg(theme.status_bar_text()),
        ),
        left_area,
    );

    if right_width > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                right_text,
                Style::new().fg(theme.muted()),
            )))
            .alignment(Alignment::Right)
            .style(Style::new().bg(theme.status_bar())),
            right_area,
        );
    }
}

/// Conteúdo da esquerda: mensagem transitória, foco ou posição do cursor.
fn left_content(app: &App, theme: &ThemeState) -> Vec<Span<'static>> {
    if let Some(message) = app
        .status_message()
        .filter(|message| message.kind == MessageKind::Error)
    {
        let style = match message.kind {
            MessageKind::Info => Style::new().fg(theme.success()),
            MessageKind::Error => Style::new().fg(theme.error()),
        };
        return vec![Span::styled(format!(" {}", message.text), style)];
    }

    if let Some((message, severity)) = cursor_diagnostic(app) {
        return vec![Span::styled(
            format!(" {message}"),
            Style::new().fg(theme.diagnostic_color(severity)),
        )];
    }

    if let Some(message) = app.status_message() {
        let style = match message.kind {
            MessageKind::Info => Style::new().fg(theme.success()),
            MessageKind::Error => Style::new().fg(theme.error()),
        };
        return vec![Span::styled(format!(" {}", message.text), style)];
    }

    match app.mode() {
        Mode::Help => vec![Span::styled(
            " Ajuda · Esc fecha",
            Style::new().fg(theme.muted()),
        )],
        Mode::Confirm => vec![Span::styled(
            " Confirmação · S salvar · D descartar · Esc cancelar",
            Style::new().fg(theme.muted()),
        )],
        Mode::ContextMenu => vec![Span::styled(
            " Menu · ↑↓ navegar · Enter abrir · Esc fechar",
            Style::new().fg(theme.muted()),
        )],
        Mode::NavigationChooser => vec![Span::styled(
            " Definições · ↑↓/PgUp/PgDn navegar · Enter abrir · Esc cancelar",
            Style::new().fg(theme.muted()),
        )],
        Mode::Prompt(_) => Vec::new(),
        Mode::Editor if app.explorer_focused() => vec![Span::styled(
            " Explorer",
            Style::new().fg(theme.accent()).add_modifier(Modifier::BOLD),
        )],
        Mode::Editor if !app.has_open_documents() => Vec::new(),
        Mode::Editor if !app.buffer().is_consistent() => vec![Span::styled(
            " Recuperação pendente",
            Style::new().fg(theme.error()),
        )],
        Mode::Editor => match (
            app.view().cursor_line(app.buffer()),
            app.view().cursor_visual_column(app.buffer()),
        ) {
            (Ok(line), Ok(column)) => vec![Span::styled(
                format!(" Ln {}, Col {}", line + 1, column + 1),
                Style::new().fg(theme.muted()),
            )],
            _ => Vec::new(),
        },
    }
}

fn cursor_diagnostic(app: &App) -> Option<(String, Option<DiagnosticSeverity>)> {
    let path = app.path()?;
    let uri = nexus_lsp::uri::path_to_uri(path);
    let text = app.buffer().text().ok()?;
    let resolved = diagnostics::resolve_for_document(app.diagnostics(), &uri, &text);
    let diagnostic = diagnostics::at_cursor(&resolved, app.view().cursor().0)?;
    let severity = match diagnostic.severity {
        Some(DiagnosticSeverity::Error) => "Erro",
        Some(DiagnosticSeverity::Warning) => "Aviso",
        Some(DiagnosticSeverity::Information) => "Info",
        Some(DiagnosticSeverity::Hint) | None => "Dica",
    };
    let origin = match (diagnostic.source.as_deref(), diagnostic.code.as_deref()) {
        (Some(source), Some(code)) => format!(" · {source} {code}"),
        (Some(source), None) => format!(" · {source}"),
        (None, Some(code)) => format!(" · {code}"),
        (None, None) => String::new(),
    };
    Some((
        format!("{severity}{origin}: {}", diagnostic.message),
        diagnostic.severity,
    ))
}

/// Conteúdo da direita: apenas estados que exigem atenção.
fn right_content(app: &App) -> String {
    if app.mode() != Mode::Editor {
        return String::new();
    }

    let state = if app.has_conflict() {
        "⚠ conflito".to_owned()
    } else if app.is_missing() {
        "⚠ removido".to_owned()
    } else if app.is_recovered() {
        "● recuperado".to_owned()
    } else if app.has_open_documents() && app.buffer().is_modified() {
        "● não salvo".to_owned()
    } else {
        String::new()
    };
    let counts = diagnostics::counts(app.diagnostics());
    let summary = match (counts.errors, counts.warnings) {
        (0, 0) => String::new(),
        (errors, warnings) => format!("E{errors} W{warnings}"),
    };
    let history = navigation_history_content(app);

    let parts: Vec<_> = [state, summary, history]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();

    if parts.is_empty() {
        String::new()
    } else {
        format!("{} ", parts.join(" · "))
    }
}

/// Indicador discreto de disponibilidade do histórico de navegação.
fn navigation_history_content(app: &App) -> String {
    let back = if app.can_navigate_back() {
        app.navigation_back_steps()
    } else {
        0
    };
    let forward = if app.can_navigate_forward() {
        app.navigation_forward_steps()
    } else {
        0
    };

    match (back, forward) {
        (0, 0) => String::new(),
        (back, 0) => format!("←{back}"),
        (0, forward) => format!("→{forward}"),
        (back, forward) => format!("←{back} →{forward}"),
    }
}
