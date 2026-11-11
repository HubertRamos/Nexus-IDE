mod editor;
mod status_bar;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, ConfirmChoice, ConfirmKind, Mode};
use crate::view::char_width;

/// Desenha a interface completa para o estado atual da aplicação.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let header_height = app.header_height().min(area.height);
    let [header, body, status] = Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, header, app);

    if app.start_screen().is_some() {
        render_start_screen(frame, body, app);
    } else {
        render_body(frame, body, app);
    }

    status_bar::render(frame, status, app);

    match app.mode() {
        Mode::Prompt(_) => render_prompt(frame, area, app),
        Mode::Help => render_help(frame, area, app),
        Mode::Confirm => render_confirm(frame, area, app),
        Mode::Editor => {}
    }
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    if let Some(width) = app.explorer_visible_width() {
        let width = width.min(area.width.saturating_sub(1));
        if width > 0 {
            let [sidebar, main] =
                Layout::horizontal([Constraint::Length(width), Constraint::Min(0)]).areas(area);
            render_explorer(frame, sidebar, app);
            editor::render(frame, main, app);
            return;
        }
    }

    editor::render(frame, area, app);
}

fn render_explorer(frame: &mut Frame, area: Rect, app: &App) {
    if area.width < 3 || area.height == 0 {
        return;
    }

    let theme = app.theme();
    let explorer = app.explorer();
    let title = app
        .workspace()
        .map(|workspace| workspace.name())
        .unwrap_or_else(|| "Explorador".to_owned());
    let focused = app.explorer_focused();

    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().fg(if focused {
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

    let rows = inner.height as usize;
    let selected = explorer.selected();
    let offset = selected.saturating_sub(rows.saturating_sub(1));
    let entries = explorer.entries();

    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(offset)
        .take(rows)
        .map(|(index, entry)| {
            let marker = if entry.is_dir {
                if entry.expanded { "▾" } else { "▸" }
            } else {
                "·"
            };
            let indent = " ".repeat(entry.depth * 2);
            let modified = app
                .tabs()
                .iter()
                .find(|tab| tab.path.as_deref() == Some(entry.path.as_path()))
                .is_some_and(|tab| tab.modified);
            let label = format!(
                " {indent}{marker} {}{}",
                entry.name,
                if modified { " ●" } else { "" }
            );
            let style = if index == selected {
                if focused {
                    Style::new()
                        .fg(theme.selection_text())
                        .bg(theme.selection())
                } else {
                    Style::new().fg(theme.accent()).bg(theme.panel())
                }
            } else {
                Style::new().fg(theme.foreground()).bg(theme.panel())
            };

            Line::from(Span::styled(label, style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).style(Style::new().bg(theme.panel())),
        inner,
    );
}

fn render_start_screen(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let Some(start) = app.start_screen() else {
        return;
    };

    let theme = app.theme();
    frame.render_widget(ratatui::widgets::Clear, area);

    let block = Block::bordered()
        .title(" Nexus · tela inicial ")
        .border_style(Style::new().fg(theme.accent()).bg(theme.panel()));

    let mut lines = vec![
        Line::from(Span::styled(
            " Projetos recentes",
            Style::new().fg(theme.accent()).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    if start.items().is_empty() {
        lines.push(Line::from(Span::styled(
            " Nenhum projeto recente. Abra um diretório ou arquivo com Ctrl+O.",
            Style::new().fg(theme.muted()),
        )));
    } else {
        for (index, entry) in start.items().iter().enumerate() {
            let style = if index == start.selected() {
                Style::new()
                    .fg(theme.selection_text())
                    .bg(theme.selection())
            } else {
                Style::new().fg(theme.foreground())
            };
            lines.push(Line::from(Span::styled(
                format!(" {}  {}", entry.name, entry.path.display()),
                style,
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑/↓ selecionar · Enter abrir · Esc fechar",
        Style::new().fg(theme.muted()),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(theme.panel())),
        area,
    );
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let [top, tabs] = if area.height > 1 {
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area)
    } else {
        [area, Rect::new(area.x, area.y, 0, 0)]
    };

    let identity_width = 24.min(top.width);
    let [left, right] =
        Layout::horizontal([Constraint::Length(identity_width), Constraint::Min(0)]).areas(top);

    let theme = app.theme();
    let identity = Line::from(vec![
        Span::styled(
            " NEXUS",
            Style::new()
                .fg(theme.accent())
                .bg(theme.surface())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  editor",
            Style::new().fg(theme.muted()).bg(theme.surface()),
        ),
    ]);

    let modified = if app.buffer().is_modified() || app.is_recovered() {
        if app.has_conflict() { " ⚠" } else { " ●" }
    } else if app.is_missing() {
        " ⚠"
    } else {
        ""
    };
    let tab_position = if app.tab_count() > 1 {
        format!(" · aba {}/{}", app.active_tab() + 1, app.tab_count())
    } else {
        String::new()
    };
    let document = format!(
        "{}{}  ·  {}  ·  UTF-8{} ",
        app.document_name(),
        modified,
        app.language_name(),
        tab_position
    );

    frame.render_widget(
        Paragraph::new(identity).style(Style::new().bg(theme.surface())),
        left,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            document,
            Style::new().fg(theme.foreground()).bg(theme.surface()),
        )))
        .alignment(Alignment::Right)
        .style(Style::new().bg(theme.surface())),
        right,
    );

    if !tabs.is_empty() && app.tab_count() > 1 {
        render_tabs(frame, tabs, app);
    }
}

fn render_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme();
    let mut spans = Vec::new();

    for tab in app.tabs() {
        let marker = if tab.modified { " ●" } else { "" };
        let label = format!(" {}{} ", tab.name, marker);
        let style = if tab.active {
            Style::new()
                .fg(theme.background())
                .bg(theme.accent())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.foreground()).bg(theme.surface())
        };
        spans.push(Span::styled(label, style));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(theme.surface())),
        area,
    );
}

fn render_prompt(frame: &mut Frame, area: Rect, app: &App) {
    let Some(prompt) = app.prompt() else {
        return;
    };

    if area.is_empty() {
        return;
    }

    let height = area.height.min(3);
    let popup = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        area.width,
        height,
    );

    frame.render_widget(Clear, popup);

    let bordered = popup.width >= 4 && popup.height >= 3;
    let inner_width = if bordered {
        popup.width.saturating_sub(2) as usize
    } else {
        popup.width as usize
    };

    let theme = app.theme();
    let characters: Vec<char> = prompt.input().chars().collect();
    let cursor = prompt.cursor().min(characters.len());
    let start = prompt.visible_start(inner_width).min(cursor);
    let before: String = characters[start..cursor].iter().collect();
    let after: String = characters.iter().skip(cursor + 1).collect();

    let cursor_style = theme.cursor_style();
    let cursor_span = match characters.get(cursor) {
        Some(character) if char_width(*character, 0) <= inner_width.max(1) => {
            Span::styled(character.to_string(), cursor_style)
        }
        _ => Span::styled(" ", cursor_style),
    };

    let text_style = Style::new().fg(theme.foreground()).bg(theme.dialog());
    let line = Line::from(vec![
        Span::styled(before, text_style),
        cursor_span,
        Span::styled(after, text_style),
    ]);

    if bordered {
        let block = Block::bordered()
            .title(format!(" {} ", prompt.kind().title()))
            .border_style(Style::new().fg(theme.accent()).bg(theme.dialog()));
        frame.render_widget(
            Paragraph::new(line)
                .block(block)
                .style(Style::new().bg(theme.dialog())),
            popup,
        );
    } else {
        frame.render_widget(
            Paragraph::new(line).style(Style::new().bg(theme.dialog())),
            popup,
        );
    }
}

fn render_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let Some(dialog) = app.confirm() else {
        return;
    };

    if area.is_empty() {
        return;
    }

    let width = area.width.min(58);
    let height = area.height.min(9);

    if width == 0 || height == 0 {
        return;
    }

    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let title = match dialog.kind() {
        ConfirmKind::Quit => " Sair ",
        ConfirmKind::Open => " Abrir outro arquivo ",
        ConfirmKind::CloseTab => " Fechar aba ",
        ConfirmKind::Delete => " Excluir ",
    };

    let message = match dialog.kind() {
        ConfirmKind::Quit => "Salvar alterações antes de sair?",
        ConfirmKind::Open => "Salvar alterações antes de abrir outro arquivo?",
        ConfirmKind::CloseTab => "Salvar alterações antes de fechar esta aba?",
        ConfirmKind::Delete => "Excluir este item? (lixeira por padrão)",
    };

    let theme = app.theme();
    let mut lines = vec![Line::from(Span::styled(
        format!(" {message}"),
        Style::new().fg(theme.foreground()),
    ))];

    if !dialog.save_available() && !dialog.destructive_only() {
        lines.push(Line::from(Span::styled(
            " Documento em recuperação: salvar indisponível.",
            Style::new().fg(theme.error()),
        )));
    }

    if let Some(target) = dialog.target() {
        lines.push(Line::from(Span::styled(
            format!(" {}", target.display()),
            Style::new().fg(theme.muted()),
        )));
    }

    lines.push(Line::from(""));
    lines.push(options_line(
        dialog.selected(),
        dialog.save_available(),
        dialog.destructive_only(),
        theme,
    ));

    if let Some(error) = dialog.error() {
        lines.push(Line::from(Span::styled(
            format!(" {error}"),
            Style::new().fg(theme.error()),
        )));
    }

    lines.push(Line::from(Span::styled(
        if dialog.destructive_only() {
            " D/Enter excluir · C/Esc cancelar"
        } else {
            " S salvar · D descartar · C/Esc cancelar"
        },
        Style::new().fg(theme.muted()),
    )));

    if popup.width >= 4 && popup.height >= 3 {
        let block = Block::bordered()
            .title(title)
            .border_style(Style::new().fg(theme.warning()).bg(theme.dialog()));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::new().bg(theme.dialog())),
            popup,
        );
    } else {
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().bg(theme.dialog())),
            popup,
        );
    }
}

fn options_line(
    selected: ConfirmChoice,
    save_available: bool,
    destructive: bool,
    theme: &crate::theme::ThemeState,
) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let choices: &[(ConfirmChoice, &str)] = if destructive {
        &[
            (ConfirmChoice::Discard, "Excluir"),
            (ConfirmChoice::Cancel, "Cancelar"),
        ]
    } else if save_available {
        &[
            (ConfirmChoice::Save, "Salvar"),
            (ConfirmChoice::Discard, "Descartar"),
            (ConfirmChoice::Cancel, "Cancelar"),
        ]
    } else {
        &[
            (ConfirmChoice::Discard, "Descartar"),
            (ConfirmChoice::Cancel, "Cancelar"),
        ]
    };

    for (index, (choice, label)) in choices.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("   "));
        }

        let style = if *choice == selected {
            Style::new()
                .fg(theme.background())
                .bg(theme.warning())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.foreground())
        };

        spans.push(Span::styled(format!(" {label} "), style));
    }

    Line::from(spans)
}

fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let theme = app.theme();

    let width = area.width.saturating_mul(3) / 4;
    let height = area.height.saturating_mul(3) / 4;

    if width == 0 || height == 0 {
        return;
    }

    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let body_style = Style::new().fg(theme.foreground());
    let color_mode = match theme.mode() {
        crate::theme::ColorMode::TrueColor => "true color",
        crate::theme::ColorMode::Ansi256 => "256 cores",
        crate::theme::ColorMode::Ansi16 => "16 cores",
        crate::theme::ColorMode::NoColor => "sem cor",
    };

    let mut lines = vec![
        Line::from(Span::styled(
            " Atalhos ",
            Style::new().fg(theme.accent()).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Navegação   ← ↑ ↓ →  ·  Home  ·  End  ·  PgUp  ·  PgDn",
            body_style,
        )),
        Line::from(Span::styled(
            " Documento   Ctrl+Home  ·  Ctrl+End",
            body_style,
        )),
        Line::from(Span::styled(
            " Seleção     Shift + movimento  ·  Ctrl+A  ·  Esc",
            body_style,
        )),
        Line::from(Span::styled(
            " Edição      texto  ·  Enter  ·  Tab  ·  Backspace  ·  Delete",
            body_style,
        )),
        Line::from(Span::styled(
            " Histórico   Ctrl+Z desfazer  ·  Ctrl+Y / Ctrl+Shift+Z refazer",
            body_style,
        )),
        Line::from(Span::styled(
            " Arquivos    Ctrl+O abrir  ·  Ctrl+S salvar  ·  Ctrl+Shift+S / F2 salvar como",
            body_style,
        )),
        Line::from(Span::styled(
            " Abas        Ctrl+W fechar  ·  Ctrl+Shift+W fechar tudo  ·  Ctrl+PgUp/PgDn alternar",
            body_style,
        )),
        Line::from(Span::styled(
            " Explorador  Ctrl+B mostrar/focar  ·  Enter abrir  ·  F2 renomear  ·  Delete excluir",
            body_style,
        )),
        Line::from(Span::styled(
            " Sidebar     Ctrl+← / Ctrl+→ redimensionam  ·  arraste a borda com o mouse",
            body_style,
        )),
        Line::from(Span::styled(
            " Recarga     Ctrl+R recarrega do disco  ·  Ctrl+Shift+H oculta arquivos",
            body_style,
        )),
        Line::from(Span::styled(
            " Saída       confirma alterações não salvas antes de sair",
            body_style,
        )),
        Line::from(Span::styled(
            " Interface   Ctrl+H / F1 ajuda  ·  Ctrl+Q / Ctrl+C sair",
            body_style,
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" Tema UI     {}", theme.ui_id()),
            body_style,
        )),
        Line::from(Span::styled(
            format!(" Tema sintaxe {}", theme.syntax_id()),
            body_style,
        )),
        Line::from(Span::styled(
            format!(" Cores       {color_mode}"),
            body_style,
        )),
    ];

    if let Some(paths) = theme.paths() {
        lines.push(Line::from(Span::styled(
            format!(" Temas       {}", paths.themes_dir.display()),
            body_style,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Esc ou Ctrl+H fecha a ajuda",
        Style::new().fg(theme.muted()),
    )));

    let block = Block::bordered()
        .title(" Nexus ")
        .border_style(Style::new().fg(theme.accent()).bg(theme.panel()));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(theme.panel())),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use nexus_syntax::TokenKind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::event::Event;
    use crate::paths::AppPaths;
    use crate::recent::{RecentProjects, RecentStore};
    use crate::view::gutter_width;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("nexus-ui-{}-{counter}-{label}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn render_terminal(app: &App, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render(frame, app))
            .expect("test render");

        terminal
    }

    fn render_to_string(app: &App, width: u16, height: u16) -> String {
        let terminal = render_terminal(app, width, height);
        let buffer = terminal.backend().buffer();
        let mut output = String::new();

        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }

        output
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn renders_identity_document_and_status_bar() {
        let app = App::with_text(80, 24, "fn main() {}\n");
        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("NEXUS"));
        assert!(screen.contains("editor"));
        assert!(screen.contains("sem título"));
        assert!(screen.contains("Ctrl+O abrir"));
        assert!(screen.contains("fn main() {}"));
        assert!(screen.contains("80×24"));
    }

    #[test]
    fn resize_changes_rendered_document_info() {
        let mut app = App::new(80, 24);
        app.update(Event::Resize {
            width: 120,
            height: 40,
        });

        let screen = render_to_string(&app, 120, 40);

        assert!(screen.contains("120×40"));
    }

    #[test]
    fn renders_at_very_small_sizes_without_panicking() {
        for (width, height) in [(1, 1), (2, 2), (10, 3), (20, 5), (40, 10)] {
            let app = App::new(width, height);
            render_to_string(&app, width, height);
        }
    }

    #[test]
    fn document_larger_than_screen_scrolls_with_cursor() {
        let text = (0..30)
            .map(|line| format!("linha {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = App::with_text(80, 10, &text);

        for _ in 0..15 {
            app.update(Event::Key(key(KeyCode::Down)));
        }

        let screen = render_to_string(&app, 80, 10);

        assert!(screen.contains("linha 15"));
        assert!(!screen.contains("linha 0"));
    }

    #[test]
    fn long_line_is_clipped_to_the_text_area() {
        let text = "0123456789".repeat(20);
        let app = App::with_text(40, 10, &text);

        let screen = render_to_string(&app, 40, 10);

        assert!(screen.contains("0123456789"));
        assert!(!screen.contains(&text));
    }

    #[test]
    fn selection_is_visually_distinct() {
        let mut app = App::with_text(40, 10, "abcdef");
        app.update(Event::Key(shift(KeyCode::Right)));
        app.update(Event::Key(shift(KeyCode::Right)));

        let terminal = render_terminal(&app, 40, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(1) + 1;
        let selection = app.theme().selection();

        assert_eq!(buffer[(text_x, 1)].style().bg, Some(selection));
        assert_eq!(buffer[(text_x + 1, 1)].style().bg, Some(selection));
        assert_ne!(buffer[(text_x + 2, 1)].style().bg, Some(selection));
    }

    #[test]
    fn cursor_is_visually_distinct() {
        let app = App::new(40, 10);
        let terminal = render_terminal(&app, 40, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(1) + 1;

        assert_eq!(buffer[(text_x, 1)].style().bg, Some(app.theme().cursor()));
    }

    #[test]
    fn current_line_is_subtly_highlighted() {
        let app = App::with_text(40, 10, "abc\ndef");
        let terminal = render_terminal(&app, 40, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(2) + 1;

        assert_eq!(
            buffer[(text_x + 1, 1)].style().bg,
            Some(app.theme().current_line())
        );
    }

    #[test]
    fn unicode_content_is_rendered() {
        let app = App::with_text(40, 10, "café 界");
        let screen = render_to_string(&app, 40, 10);

        assert!(screen.contains("café"));
        assert!(screen.contains("界"));
    }

    #[test]
    fn prompt_overlay_is_rendered() {
        let mut app = App::new(80, 24);
        app.update(Event::Key(ctrl(KeyCode::Char('o'))));

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("Abrir arquivo"));
    }

    #[test]
    fn help_overlay_is_rendered() {
        let mut app = App::new(80, 24);
        app.update(Event::Key(ctrl(KeyCode::Char('h'))));

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("Atalhos"));
        assert!(screen.contains("Ctrl+O abrir"));
    }

    #[test]
    fn quit_confirmation_overlay_is_rendered() {
        let mut app = App::with_text(80, 24, "abc");
        app.update(Event::Key(key(KeyCode::Char('x'))));
        app.update(Event::Key(ctrl(KeyCode::Char('q'))));

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("Sair"));
        assert!(screen.contains("Salvar alterações"));
        assert!(screen.contains("Salvar"));
        assert!(screen.contains("Descartar"));
        assert!(screen.contains("Cancelar"));
    }

    #[test]
    fn recovery_confirmation_reports_save_unavailable() {
        let mut app = App::with_text(80, 24, "abc");
        app.force_recovery_for_tests();
        app.update(Event::Key(ctrl(KeyCode::Char('q'))));

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("recuperação"));
        assert!(screen.contains("Descartar"));
        assert!(screen.contains("Cancelar"));
    }

    #[test]
    fn confirmation_renders_at_small_sizes_without_panicking() {
        for (width, height) in [(1, 1), (2, 2), (10, 3), (20, 5), (40, 10)] {
            let mut app = App::with_text(width, height, "abc");
            app.update(Event::Key(key(KeyCode::Char('x'))));
            app.update(Event::Key(ctrl(KeyCode::Char('q'))));

            render_to_string(&app, width, height);
        }
    }

    #[test]
    fn long_prompt_path_scrolls_to_keep_cursor_visible() {
        let mut app = App::new(40, 10);
        app.update(Event::Key(ctrl(KeyCode::Char('o'))));

        let path = format!("/home/usuario/{}", "a".repeat(80));
        for character in path.chars() {
            app.update(Event::Key(key(KeyCode::Char(character))));
        }

        let screen = render_to_string(&app, 40, 10);

        assert!(screen.contains("aaaa"));
        assert!(!screen.contains("/home/usuario/"));
    }

    #[test]
    fn narrow_status_bar_keeps_feedback_visible() {
        let mut app = App::new(30, 8);
        app.set_info_for_tests("arquivo salvo");

        let screen = render_to_string(&app, 30, 8);

        assert!(screen.contains("arquivo salvo"));
        assert!(screen.contains("EDIT"));
    }

    #[test]
    fn narrow_terminal_with_long_prompt_renders() {
        let mut app = App::new(10, 5);
        app.update(Event::Key(ctrl(KeyCode::Char('o'))));

        for character in "a".repeat(50).chars() {
            app.update(Event::Key(key(KeyCode::Char(character))));
        }

        let screen = render_to_string(&app, 10, 5);

        assert!(screen.contains('a'));
    }

    #[test]
    fn prompt_with_unicode_path_is_rendered() {
        let mut app = App::new(40, 10);
        app.update(Event::Key(ctrl(KeyCode::Char('o'))));

        for character in "café 界".chars() {
            app.update(Event::Key(key(KeyCode::Char(character))));
        }

        let screen = render_to_string(&app, 40, 10);

        assert!(screen.contains("café"));
        assert!(screen.contains("界"));
    }

    #[test]
    fn rust_tokens_use_syntax_theme_colors() {
        let app = App::with_document(60, 10, Path::new("main.rs"), "fn main() { let x = 42; }");
        let terminal = render_terminal(&app, 60, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(1) + 1;

        let keyword = app.theme().syntax_color(TokenKind::Keyword);
        let number = app.theme().syntax_color(TokenKind::Number);

        // O cursor ocupa o primeiro caractere; o segundo ainda é `n` de `fn`.
        assert_eq!(buffer[(text_x + 1, 1)].style().fg, Some(keyword));
        assert_eq!(buffer[(text_x + 20, 1)].style().fg, Some(number));
    }

    #[test]
    fn all_initial_languages_render_highlighting() {
        let cases = [
            ("main.rs", "fn main() { let x = 1; }"),
            ("script.py", "def main():\n    return 1\n"),
            ("app.js", "function main() { return 1; }"),
            ("module.ts", "function main(): number { return 1; }"),
        ];

        for (path, source) in cases {
            let app = App::with_document(60, 12, Path::new(path), source);
            assert!(app.syntax().is_some(), "{path} deveria ter highlighting");

            let terminal = render_terminal(&app, 60, 12);
            let buffer = terminal.backend().buffer();
            let text_x = gutter_width(app.buffer().line_count().unwrap()) + 1;
            let keyword = app.theme().syntax_color(TokenKind::Keyword);
            let number = app.theme().syntax_color(TokenKind::Number);

            let mut keyword_seen = false;
            let mut number_seen = false;
            for y in 1..11 {
                for x in text_x..58 {
                    let fg = buffer[(x, y)].style().fg;
                    keyword_seen |= fg == Some(keyword);
                    number_seen |= fg == Some(number);
                }
            }

            assert!(keyword_seen, "{path} deveria renderizar palavras-chave");
            assert!(number_seen, "{path} deveria renderizar números");
        }
    }

    #[test]
    fn selection_overrides_syntax_color_and_cursor_has_precedence() {
        let mut app = App::with_document(60, 10, Path::new("main.rs"), "fn main() { let x = 42; }");
        for _ in 0..5 {
            app.update(Event::Key(shift(KeyCode::Right)));
        }

        let terminal = render_terminal(&app, 60, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(1) + 1;
        let selection = app.theme().selection();

        // A seleção cobre `fn ma`; o cursor está logo depois dela.
        assert_eq!(buffer[(text_x, 1)].style().bg, Some(selection));
        assert_eq!(buffer[(text_x + 1, 1)].style().bg, Some(selection));
        assert_eq!(
            buffer[(text_x + 5, 1)].style().bg,
            Some(app.theme().cursor())
        );
        assert_eq!(
            buffer[(text_x + 1, 1)].style().fg,
            Some(app.theme().selection_text())
        );
    }

    #[test]
    fn horizontal_scroll_keeps_highlighted_tokens_visible() {
        let source = format!("fn main() {{ {} let valor = 42; }}", "x".repeat(100));
        let mut app = App::with_document(40, 10, Path::new("main.rs"), &source);
        app.update(Event::Key(key(KeyCode::End)));

        assert!(app.view().left_col() > 0);

        let terminal = render_terminal(&app, 40, 10);
        let buffer = terminal.backend().buffer();
        let number = app.theme().syntax_color(TokenKind::Number);

        let mut number_seen = false;
        for y in 1..9 {
            for x in 0..40 {
                number_seen |= buffer[(x, y)].style().fg == Some(number);
            }
        }
        assert!(number_seen, "o número 42 deveria permanecer visível");
    }

    #[test]
    fn unicode_tokens_keep_highlighting_and_width() {
        let source = "// café 界 😀\nfn main() { let nome = \"ação\"; }";
        let app = App::with_document(50, 10, Path::new("main.rs"), source);
        let terminal = render_terminal(&app, 50, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(2) + 1;
        let comment = app.theme().syntax_color(TokenKind::Comment);

        assert_eq!(buffer[(text_x + 1, 1)].style().fg, Some(comment));

        let screen = render_to_string(&app, 50, 10);
        assert!(screen.contains("café 界"));
        assert!(screen.contains("😀"));
        assert!(screen.contains("ação"));
    }

    #[test]
    fn rendering_does_not_reparse_the_document() {
        let source = "fn main() {\n    let x = 42;\n}\n";
        let app = App::with_document(50, 10, Path::new("main.rs"), source);
        let before = app.syntax().unwrap().update_count();

        let _ = render_to_string(&app, 50, 10);
        let _ = render_to_string(&app, 50, 10);

        assert_eq!(app.syntax().unwrap().update_count(), before);
    }

    #[test]
    fn narrow_terminal_renders_highlighted_document() {
        let app = App::with_document(18, 6, Path::new("main.rs"), "fn x() { 42 }");
        let screen = render_to_string(&app, 18, 6);

        assert!(screen.contains("fn"));
        assert!(screen.contains("42"));
    }

    #[test]
    fn large_document_renders_only_visible_lines_without_reparsing() {
        let source = (0..500)
            .map(|line| format!("fn funcao_{line}() {{ let valor_{line} = {line}; }}"))
            .collect::<Vec<_>>()
            .join("\n");
        let app = App::with_document(80, 24, Path::new("main.rs"), &source);
        let before = app.syntax().unwrap().update_count();

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("funcao_0"));
        assert!(!screen.contains("funcao_499"));
        assert_eq!(app.syntax().unwrap().update_count(), before);
    }

    #[test]
    fn unknown_language_remains_plain_text() {
        let app = App::with_document(50, 10, Path::new("notas.txt"), "fn main() { let x = 1; }");

        assert!(app.syntax().is_none());
        assert_eq!(app.language_name(), "Texto");

        let screen = render_to_string(&app, 50, 10);
        assert!(screen.contains("fn main()"));
        assert!(screen.contains("Texto"));
    }

    #[test]
    fn header_shows_detected_language() {
        let app = App::with_document(60, 10, Path::new("main.rs"), "fn main() {}");
        let screen = render_to_string(&app, 60, 10);

        assert!(screen.contains("Rust"));
    }

    #[test]
    fn header_renders_tabs_for_multiple_documents() {
        let directory = TempDir::new("tabs");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut app = App::with_text(80, 24, "");

        app.open_document_in_new_tab(&first).unwrap();
        app.open_document_in_new_tab(&second).unwrap();

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("a.txt"));
        assert!(screen.contains("b.txt"));
        assert!(screen.contains("aba 2/2"));
    }

    #[test]
    fn explorer_sidebar_is_rendered_with_workspace() {
        let directory = TempDir::new("explorer");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("main.rs"), "fn main() {}").unwrap();
        let mut app = App::with_text(80, 24, "");

        app.open_workspace(&project).unwrap();
        app.open_document_in_new_tab(&project.join("main.rs"))
            .unwrap();

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("projeto"));
        assert!(screen.contains("main.rs"));
        assert!(screen.contains("fn main()"));
    }

    #[test]
    fn start_screen_lists_recent_projects() {
        let directory = TempDir::new("start");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let paths = AppPaths {
            config_file: config.join("config.toml"),
            themes_dir: config.join("themes"),
            root: config.clone(),
            session_file: config.join("session.toml"),
            recent_file: config.join("recent.toml"),
            recovery_dir: config.join("recovery"),
        };
        let store = RecentStore::new(paths.recent_file.clone());
        let mut projects = RecentProjects::default();
        projects.record("projeto", &project);
        store.save(&projects).unwrap();

        let mut app = App::with_paths(80, 24, paths);
        app.show_start_screen();

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("tela inicial"));
        assert!(screen.contains("Projetos recentes"));
        assert!(screen.contains("projeto"));
    }

    #[test]
    fn delete_confirmation_uses_explicit_destructive_label() {
        let directory = TempDir::new("delete-confirm");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut app = App::with_text(80, 24, "");

        app.open_workspace(&project).unwrap();
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::SHIFT,
        )));

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("Excluir"));
        assert!(screen.contains("Cancelar"));
        assert!(screen.contains("lixeira"));
    }
}
