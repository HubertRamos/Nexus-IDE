use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme();
    let counts = app.problem_counts();
    let filter = match app.problems_filter() {
        crate::diagnostics::ProblemFilter::All => "Todos",
        crate::diagnostics::ProblemFilter::Errors => "Erros",
        crate::diagnostics::ProblemFilter::Warnings => "Avisos",
        crate::diagnostics::ProblemFilter::Information => "Info",
        crate::diagnostics::ProblemFilter::Hints => "Hints",
    };
    let title = format!(
        " Problemas · {filter}  E:{} W:{} I:{} H:{} ",
        counts.errors, counts.warnings, counts.information, counts.hints
    );
    let border = if area.height >= 3 {
        Borders::TOP
    } else {
        Borders::NONE
    };
    let block = Block::default()
        .borders(border)
        .title(title)
        .border_style(Style::new().fg(if app.problems_focused() {
            theme.accent()
        } else {
            theme.border()
        }))
        .style(Style::new().bg(theme.panel()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let items = app.problem_items();
    let selected = app.selected_problem();
    let help_rows = usize::from(inner.height >= 4);
    let rows = inner.height as usize - help_rows;
    let offset = app.problems_selection_offset();
    let mut lines = Vec::new();
    for item in items.iter().skip(offset).take(rows) {
        let marker = match item.severity {
            Some(nexus_lsp::protocol::DiagnosticSeverity::Error) => "E",
            Some(nexus_lsp::protocol::DiagnosticSeverity::Warning) => "W",
            Some(nexus_lsp::protocol::DiagnosticSeverity::Information) => "I",
            Some(nexus_lsp::protocol::DiagnosticSeverity::Hint) | None => "H",
        };
        let active = Some(&item.identity) == selected.as_ref();
        let base = if active && app.problems_focused() {
            theme.selection_style()
        } else {
            Style::new().fg(theme.foreground()).bg(theme.panel())
        };
        let marker_style = base.fg(theme.diagnostic_color(item.severity));
        let mut text = format!(
            " {}:{}:{}  {}",
            item.file_label,
            item.line,
            item.column,
            item.message.replace(['\n', '\r'], " ")
        );
        if inner.width >= 100 {
            if let Some(source) = &item.source {
                text.push_str(&format!("  [{source}]"));
            }
            if let Some(code) = &item.code {
                text.push_str(&format!(" ({code})"));
            }
        }
        if item.freshness == crate::document_sync::DiagnosticFreshness::Unverified {
            text.push_str(" ·");
        }
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), marker_style),
            Span::styled(
                text.trim_start().to_owned(),
                if active {
                    base.add_modifier(Modifier::BOLD)
                } else {
                    base
                },
            ),
        ]));
    }
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            " Nenhum problema neste filtro",
            Style::new().fg(theme.muted()),
        )));
    }
    if help_rows > 0 {
        let help = if inner.width >= 70 {
            " 0 todos · 1 erros · 2 avisos · 3 info · 4 hints · ↑↓ mover · Enter abrir · Tab editor · Esc fechar"
        } else {
            " ↑↓ mover · Enter abrir · Tab editor · Esc fechar"
        };
        lines.push(Line::from(Span::styled(
            help,
            Style::new().fg(theme.muted()),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::new().bg(theme.panel())),
        inner,
    );
}
