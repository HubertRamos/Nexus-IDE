mod editor;
mod problems;
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
        Mode::ContextMenu => render_context_menu(frame, area, app),
        Mode::NavigationChooser => render_navigation_chooser(frame, area, app),
        Mode::Editor => {}
    }
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let panel_height = app.problems_panel_height();
    let (editor_area, panel_area) = if panel_height > 0 {
        let [editor, panel] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(panel_height)]).areas(area);
        (editor, Some(panel))
    } else {
        (area, None)
    };

    if let Some(width) = app.explorer_visible_width() {
        let width = width.min(editor_area.width.saturating_sub(1));
        if width > 0 {
            let [sidebar, main] =
                Layout::horizontal([Constraint::Length(width), Constraint::Min(0)])
                    .areas(editor_area);
            render_explorer(frame, sidebar, app);
            editor::render(frame, main, app);
        } else {
            editor::render(frame, editor_area, app);
        }
    } else {
        editor::render(frame, editor_area, app);
    }
    if let Some(panel) = panel_area {
        problems::render(frame, panel, app);
    }
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
            " Nenhum projeto recente. Abra um projeto com Ctrl+Shift+O (ou F3) ou um arquivo com Ctrl+O.",
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

    let theme = app.theme();
    let identity = Line::from(Span::styled(
        " NEXUS",
        Style::new()
            .fg(theme.accent())
            .bg(theme.surface())
            .add_modifier(Modifier::BOLD),
    ));

    let context = app
        .workspace()
        .map(|workspace| format!(" {} ", workspace.name()))
        .unwrap_or_default();

    frame.render_widget(
        Paragraph::new(identity).style(Style::new().bg(theme.surface())),
        top,
    );

    if !context.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                context,
                Style::new().fg(theme.muted()).bg(theme.surface()),
            )))
            .alignment(Alignment::Right)
            .style(Style::new().bg(theme.surface())),
            top,
        );
    }

    if app.has_open_documents() {
        render_tabs(frame, tabs, app);
    }
}

fn render_tabs(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }

    let theme = app.theme();
    let tabs = app.tabs();
    let slots = app.tab_slots(area.width);
    let mut spans = Vec::with_capacity(slots.len());

    for slot in slots {
        let Some(tab) = tabs.iter().find(|tab| tab.index == slot.index) else {
            continue;
        };

        let show_close = tab.active || app.hovered_tab() == Some(slot.index);
        let name = if tab.modified {
            format!("{} ●", tab.name)
        } else {
            tab.name.clone()
        };
        let label = fit_tab_label(&name, slot.width, show_close);
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

/// Ajusta o rótulo da aba à largura exata usada pelo mouse.
fn fit_tab_label(name: &str, width: u16, show_close: bool) -> String {
    let width = width as usize;
    if width == 0 {
        return String::new();
    }

    let tail = if show_close { 2 } else { 1 };
    let name_area = width.saturating_sub(1 + tail);
    let mut label = String::from(" ");
    let mut used = 0;

    for character in name.chars() {
        let character_width = char_width(character, used);
        if used + character_width > name_area {
            break;
        }
        label.push(character);
        used += character_width;
    }

    while used < name_area {
        label.push(' ');
        used += 1;
    }

    if show_close {
        label.push(' ');
        label.push('×');
    } else {
        label.push(' ');
    }

    label
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

fn render_context_menu(frame: &mut Frame, area: Rect, app: &App) {
    let Some(menu) = app.context_menu() else {
        return;
    };

    if area.is_empty() {
        return;
    }

    let popup = menu.rect(area.width, area.height);
    if popup.width < 3 || popup.height < 3 {
        return;
    }

    frame.render_widget(Clear, popup);

    let theme = app.theme();
    let lines: Vec<Line> = menu
        .items()
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let style = if index == menu.selected() {
                Style::new()
                    .fg(theme.selection_text())
                    .bg(theme.selection())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.foreground()).bg(theme.dialog())
            };
            Line::from(Span::styled(format!(" {} ", item.label), style))
        })
        .collect();

    let block = Block::bordered()
        .title(format!(" {} ", menu.title()))
        .border_style(Style::new().fg(theme.accent()).bg(theme.dialog()));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(theme.dialog())),
        popup,
    );
}

fn render_navigation_chooser(frame: &mut Frame, area: Rect, app: &App) {
    let Some(chooser) = app.navigation_chooser() else {
        return;
    };

    if area.is_empty() {
        return;
    }

    let popup = chooser.rect(area.width, area.height);
    if popup.width < 3 || popup.height < 3 {
        return;
    }

    frame.render_widget(Clear, popup);

    let theme = app.theme();
    let rows = popup.height.saturating_sub(2) as usize;
    let offset = chooser.offset(rows);
    let lines: Vec<Line> = chooser
        .choices()
        .iter()
        .enumerate()
        .skip(offset)
        .take(rows)
        .map(|(index, choice)| {
            let selected = index == chooser.selected();
            let style = if selected {
                Style::new()
                    .fg(theme.selection_text())
                    .bg(theme.selection())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.foreground()).bg(theme.dialog())
            };
            let marker = if selected { "▸" } else { " " };
            let context = choice
                .context
                .as_deref()
                .map(|context| format!("  {context}"))
                .unwrap_or_default();
            Line::from(Span::styled(
                format!(
                    " {marker} {}  {}:{}:{}{context} ",
                    choice.file_name,
                    choice.relative_path,
                    choice.line + 1,
                    choice.column + 1
                ),
                style,
            ))
        })
        .collect();

    let block = Block::bordered()
        .title(format!(
            " {} ({}) ",
            chooser.title(),
            chooser.choices().len()
        ))
        .border_style(Style::new().fg(theme.accent()).bg(theme.dialog()));

    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().bg(theme.dialog())),
        popup,
    );
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
            " LSP         F12 definição  ·  Ctrl+F12 declaração  ·  Ctrl+T tipo  ·  Shift+F12 referências  ·  Enter confirma",
            body_style,
        )),
        Line::from(Span::styled(
            " Seleção     Shift + movimento  ·  Ctrl+A  ·  Esc",
            body_style,
        )),
        Line::from(Span::styled(
            " Edição      texto  ·  Enter auto-indenta  ·  Tab / Shift+Tab indentam  ·  Backspace  ·  Delete",
            body_style,
        )),
        Line::from(Span::styled(
            " Blocos      Ctrl+D / Ctrl+Shift+D duplicam  ·  Alt+↑ / Alt+↓ movem linhas ou seleção",
            body_style,
        )),
        Line::from(Span::styled(
            " Histórico   Alt+← voltar  ·  Alt+→ avançar  ·  Ctrl+Z desfazer  ·  Ctrl+Y / Ctrl+Shift+Z refazer",
            body_style,
        )),
        Line::from(Span::styled(
            " Arquivos    Ctrl+O abrir  ·  Ctrl+Shift+O / F3 abrir projeto  ·  Ctrl+S salvar  ·  Ctrl+Shift+S / F2 salvar como",
            body_style,
        )),
        Line::from(Span::styled(
            " Abas        Ctrl+Q fechar  ·  Ctrl+Shift+Q / Ctrl+C sair  ·  Ctrl+PgUp/PgDn alternar",
            body_style,
        )),
        Line::from(Span::styled(
            " Explorador  Ctrl+B focar  ·  ↑↓ navegar  ·  → expandir  ·  ← recolher/pai  ·  Enter abrir  ·  Esc editor",
            body_style,
        )),
        Line::from(Span::styled(
            " Contexto    clique direito ou Shift+F10  ·  F2 renomear  ·  Delete excluir",
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
            " Saída       Ctrl+Shift+Q confirma alterações não salvas antes de sair",
            body_style,
        )),
        Line::from(Span::styled(
            " Interface   Ctrl+H / F1 ajuda  ·  Ctrl+Q fecha aba  ·  Ctrl+Shift+Q sai",
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
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::*;
    use crate::document_sync::{DiagnosticFreshness, DiagnosticsKey, StoredDiagnostics};
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

    fn press_ctrl(app: &mut App, code: KeyCode) {
        app.update(Event::Key(ctrl(code)));
    }

    fn press_quit(app: &mut App) {
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    #[test]
    fn renders_identity_document_and_status_bar() {
        let app = App::with_text(80, 24, "fn main() {}\n");
        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("NEXUS"));
        assert!(screen.contains("sem título"));
        assert!(screen.contains("fn main() {}"));
        assert!(screen.contains("Ln 1, Col 1"));
    }

    #[test]
    fn resize_changes_rendered_document_info() {
        let mut app = App::with_text(80, 24, "abc");
        app.update(Event::Resize {
            width: 120,
            height: 40,
        });

        let screen = render_to_string(&app, 120, 40);

        assert!(screen.contains("abc"));
        assert!(screen.contains("NEXUS"));
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
        let row = app.header_height();
        let selection = app.theme().selection();

        assert_eq!(buffer[(text_x, row)].style().bg, Some(selection));
        assert_eq!(buffer[(text_x + 1, row)].style().bg, Some(selection));
        assert_ne!(buffer[(text_x + 2, row)].style().bg, Some(selection));
    }

    #[test]
    fn cursor_is_visually_distinct() {
        let app = App::with_text(40, 10, "a");
        let terminal = render_terminal(&app, 40, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(1) + 1;
        let row = app.header_height();

        assert_eq!(buffer[(text_x, row)].style().bg, Some(app.theme().cursor()));
    }

    fn diagnostic_fixture(
        uri: &str,
        workspace: &str,
        severity: Option<nexus_lsp::protocol::DiagnosticSeverity>,
        start: (u32, u32),
        end: (u32, u32),
        freshness: DiagnosticFreshness,
        message: &str,
    ) -> StoredDiagnostics {
        use nexus_lsp::protocol::{Diagnostic, Position, Range};
        use std::collections::BTreeMap;

        StoredDiagnostics {
            key: DiagnosticsKey {
                workspace: workspace.into(),
                language_id: "rust".into(),
                session_id: 1,
                uri: uri.into(),
            },
            current_document_version: Some(1),
            publication: nexus_lsp::protocol::PublishDiagnosticsParams {
                uri: uri.into(),
                version: Some(1),
                extra: BTreeMap::new(),
                diagnostics: vec![Diagnostic {
                    range: Range {
                        start: Position {
                            line: start.0,
                            character: start.1,
                        },
                        end: Position {
                            line: end.0,
                            character: end.1,
                        },
                    },
                    severity,
                    code: Some(serde_json::json!("E42")),
                    source: Some("checker".into()),
                    message: message.into(),
                    code_description: None,
                    tags: Vec::new(),
                    related_information: Vec::new(),
                    data: None,
                    extra: BTreeMap::new(),
                }],
            },
            freshness,
        }
    }

    #[test]
    fn diagnostics_use_severity_styles_and_ignore_potentially_stale_ranges() {
        use nexus_lsp::protocol::DiagnosticSeverity as Severity;
        use ratatui::style::Modifier;

        let path = Path::new("/tmp/nexus-diagnostic-style.rs");
        let uri = nexus_lsp::uri::path_to_uri(path);
        for severity in [
            Severity::Error,
            Severity::Warning,
            Severity::Information,
            Severity::Hint,
        ] {
            let mut app = App::with_document(40, 10, path, "abc");
            app.set_diagnostics_for_tests(vec![diagnostic_fixture(
                &uri,
                "/tmp",
                Some(severity),
                (0, 0),
                (0, 2),
                DiagnosticFreshness::Current,
                "message",
            )]);
            let terminal = render_terminal(&app, 40, 10);
            let buffer = terminal.backend().buffer();
            let position = (gutter_width(1) + 2, app.header_height());
            let style = buffer[position].style();
            assert!(style.add_modifier.contains(Modifier::UNDERLINED));
            assert_eq!(style.fg, Some(app.theme().diagnostic_color(Some(severity))));
            assert_eq!(
                app.buffer().text().unwrap(),
                "abc",
                "renderização não altera o buffer"
            );
        }

        let mut app = App::with_document(40, 10, path, "abc");
        app.set_diagnostics_for_tests(vec![diagnostic_fixture(
            &uri,
            "/tmp",
            Some(Severity::Error),
            (0, 0),
            (0, 2),
            DiagnosticFreshness::PotentiallyStale,
            "old",
        )]);
        let terminal = render_terminal(&app, 40, 10);
        let style = terminal.backend().buffer()[(gutter_width(1) + 2, app.header_height())].style();
        assert!(!style.add_modifier.contains(Modifier::UNDERLINED));

        let mut app = App::with_document(40, 10, path, "abc");
        app.set_diagnostics_for_tests(vec![
            diagnostic_fixture(
                &uri,
                "/tmp",
                Some(Severity::Warning),
                (0, 0),
                (0, 2),
                DiagnosticFreshness::Current,
                "warning",
            ),
            diagnostic_fixture(
                &uri,
                "/tmp",
                Some(Severity::Error),
                (0, 0),
                (0, 2),
                DiagnosticFreshness::Current,
                "error",
            ),
        ]);
        let terminal = render_terminal(&app, 40, 10);
        assert_eq!(
            terminal.backend().buffer()[(gutter_width(1) + 2, app.header_height())]
                .style()
                .fg,
            Some(app.theme().error())
        );
        app.update(Event::Key(shift(KeyCode::Right)));
        let terminal = render_terminal(&app, 40, 10);
        assert_eq!(
            terminal.backend().buffer()[(gutter_width(1) + 1, app.header_height())]
                .style()
                .bg,
            Some(app.theme().selection())
        );
    }

    #[test]
    fn unverified_multiline_tab_and_wide_character_ranges_are_decorated_discreetly() {
        use nexus_lsp::protocol::DiagnosticSeverity as Severity;
        use ratatui::style::Modifier;

        let path = Path::new("/tmp/nexus-diagnostic-multiline.rs");
        let uri = nexus_lsp::uri::path_to_uri(path);
        let mut app = App::with_document(50, 10, path, "\t界x\nnext");
        app.set_diagnostics_for_tests(vec![diagnostic_fixture(
            &uri,
            "/tmp",
            Some(Severity::Warning),
            (0, 0),
            (1, 2),
            DiagnosticFreshness::Unverified,
            "message",
        )]);
        let terminal = render_terminal(&app, 50, 10);
        let buffer = terminal.backend().buffer();
        let x = gutter_width(2) + 1;
        let first_row = app.header_height();
        assert!(
            buffer[(x, first_row)]
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            buffer[(x + 4, first_row)]
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "ideograma após tab stop deve receber a decoração"
        );
        assert!(
            buffer[(x, first_row + 1)]
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED),
            "range multilinha deve continuar na linha seguinte"
        );
        assert!(
            buffer[(x, first_row)]
                .style()
                .add_modifier
                .contains(Modifier::DIM),
            "unverified deve ser discreto"
        );
    }

    #[test]
    fn cursor_message_prioritizes_severity_and_compact_counts_update_from_snapshot() {
        use nexus_lsp::protocol::DiagnosticSeverity as Severity;

        let path = Path::new("/tmp/nexus-context.rs");
        let uri = nexus_lsp::uri::path_to_uri(path);
        let mut app = App::with_document(80, 10, path, "abc");
        let warning = diagnostic_fixture(
            &uri,
            "/workspace-one",
            Some(Severity::Warning),
            (0, 0),
            (0, 2),
            DiagnosticFreshness::Current,
            "warning text",
        );
        let error = diagnostic_fixture(
            &uri,
            "/workspace-one",
            Some(Severity::Error),
            (0, 0),
            (0, 2),
            DiagnosticFreshness::Unverified,
            "error text",
        );
        let other = diagnostic_fixture(
            "file:///other.rs",
            "/workspace-two",
            Some(Severity::Error),
            (0, 0),
            (0, 1),
            DiagnosticFreshness::Current,
            "other",
        );
        let mut current = warning;
        current
            .publication
            .diagnostics
            .extend(error.publication.diagnostics);
        app.set_diagnostics_for_tests(vec![current, other.clone()]);
        let screen = render_to_string(&app, 80, 10);
        assert!(screen.contains("error text"));
        assert!(screen.contains("checker E42"));
        assert!(!screen.contains("warning text"));
        assert!(screen.contains("E2 W1"));

        app.set_diagnostics_for_tests(vec![other.clone()]);
        assert!(
            render_to_string(&app, 80, 10).contains("E1 W0"),
            "novo snapshot substitui a contagem anterior"
        );

        app.update(Event::Key(key(KeyCode::Right)));
        app.update(Event::Key(key(KeyCode::Right)));
        assert!(!render_to_string(&app, 80, 10).contains("error text"));

        app.set_diagnostics_for_tests(vec![diagnostic_fixture(
            &uri,
            "/workspace-one",
            Some(Severity::Error),
            (0, 0),
            (0, 1),
            DiagnosticFreshness::PotentiallyStale,
            "stale",
        )]);
        assert!(!render_to_string(&app, 80, 10).contains("E1 W0"));
    }

    #[test]
    fn current_line_is_subtly_highlighted() {
        let app = App::with_text(40, 10, "abc\ndef");
        let terminal = render_terminal(&app, 40, 10);
        let buffer = terminal.backend().buffer();
        let text_x = gutter_width(2) + 1;
        let row = app.header_height();

        assert_eq!(
            buffer[(text_x + 1, row)].style().bg,
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
        press_quit(&mut app);

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
        press_quit(&mut app);

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
            press_quit(&mut app);

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
        let row = app.header_height();

        let keyword = app.theme().syntax_color(TokenKind::Keyword);
        let number = app.theme().syntax_color(TokenKind::Number);

        // O cursor ocupa o primeiro caractere; o segundo ainda é `n` de `fn`.
        assert_eq!(buffer[(text_x + 1, row)].style().fg, Some(keyword));
        assert_eq!(buffer[(text_x + 20, row)].style().fg, Some(number));
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
            for y in app.header_height()..(12 - 1) {
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
        let row = app.header_height();
        let selection = app.theme().selection();

        // A seleção cobre `fn ma`; o cursor está logo depois dela.
        assert_eq!(buffer[(text_x, row)].style().bg, Some(selection));
        assert_eq!(buffer[(text_x + 1, row)].style().bg, Some(selection));
        assert_eq!(
            buffer[(text_x + 5, row)].style().bg,
            Some(app.theme().cursor())
        );
        assert_eq!(
            buffer[(text_x + 1, row)].style().fg,
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
        for y in app.header_height()..9 {
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
        let row = app.header_height();
        let comment = app.theme().syntax_color(TokenKind::Comment);

        assert_eq!(buffer[(text_x + 1, row)].style().fg, Some(comment));

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
    }

    #[test]
    fn header_shows_open_document_tab() {
        let app = App::with_document(60, 10, Path::new("main.rs"), "fn main() {}");
        let screen = render_to_string(&app, 60, 10);

        assert!(screen.contains("main.rs"));
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
        assert!(screen.contains('×'));
    }

    #[test]
    fn inactive_tab_reveals_close_button_on_hover() {
        let directory = TempDir::new("tab-hover");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut app = App::with_text(80, 24, "");

        app.open_document_in_new_tab(&first).unwrap();
        app.open_document_in_new_tab(&second).unwrap();

        let before = render_to_string(&app, 80, 24);
        assert_eq!(before.matches('×').count(), 1);

        let slot = app.tab_slots(80)[0];
        app.update(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: slot.start + 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));

        let after = render_to_string(&app, 80, 24);
        assert_eq!(after.matches('×').count(), 2);
    }

    #[test]
    fn closing_last_tab_renders_empty_editor_state() {
        let mut app = App::with_text(80, 24, "abc");
        assert!(render_to_string(&app, 80, 24).contains("abc"));

        press_ctrl(&mut app, KeyCode::Char('q'));

        assert!(!app.has_open_documents());
        let screen = render_to_string(&app, 80, 24);
        assert!(screen.contains("Nenhum documento aberto"));
        assert!(!screen.contains("abc"));
        assert!(!screen.contains('×'));
    }

    #[test]
    fn navigation_chooser_renders_destinations_and_selection() {
        use nexus_lsp::protocol::{NavigationTarget, Position};

        let directory = TempDir::new("navigation-chooser-render");
        let first = directory.join("primeiro.rs");
        let second = directory.join("segundo.rs");
        fs::write(&first, "fn primeiro() {}\n").unwrap();
        fs::write(&second, "fn segundo() {}\n").unwrap();
        let mut app = App::with_text(80, 24, "fn main() {}\n");
        app.open_workspace(&directory.path).unwrap();

        let destination = |path: &Path| {
            let range = nexus_lsp::protocol::Range {
                start: Position {
                    line: 0,
                    character: 3,
                },
                end: Position {
                    line: 0,
                    character: 11,
                },
            };
            NavigationTarget {
                uri: nexus_lsp::uri::path_to_uri(path),
                target_range: range,
                selection_range: range,
                origin_selection_range: None,
            }
        };
        app.open_navigation_chooser_for_tests(vec![destination(&first), destination(&second)]);

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("Definições (2)"));
        assert!(screen.contains("primeiro.rs"));
        assert!(screen.contains("segundo.rs"));
        assert!(screen.contains("fn primeiro()"));
        assert!(screen.contains("▸"));
    }

    #[test]
    fn references_chooser_renders_its_title_and_scrolls_large_lists() {
        use nexus_lsp::protocol::{NavigationTarget, Position};

        let directory = TempDir::new("references-chooser-render");
        let file = directory.join("main.rs");
        let text = (1..=40)
            .map(|line| format!("fn alvo_{line}() {{}}\n"))
            .collect::<String>();
        fs::write(&file, text).unwrap();
        let mut app = App::with_text(80, 24, "fn main() {}\n");
        app.open_workspace(&directory.path).unwrap();
        let uri = nexus_lsp::uri::path_to_uri(&file);
        let targets = (1..=40)
            .map(|line| {
                let range = nexus_lsp::protocol::Range {
                    start: Position {
                        line: line - 1,
                        character: 3,
                    },
                    end: Position {
                        line: line - 1,
                        character: 9,
                    },
                };
                NavigationTarget {
                    uri: uri.clone(),
                    target_range: range,
                    selection_range: range,
                    origin_selection_range: None,
                }
            })
            .collect::<Vec<_>>();
        app.open_references_chooser_for_tests(targets);

        let screen = render_to_string(&app, 80, 24);
        assert!(screen.contains("Referências (40)"));
        assert!(!screen.contains("Definições"));
        assert!(screen.contains("main.rs"));
        assert!(screen.contains(":2:4"));
        assert!(screen.contains("▸"));

        for _ in 0..30 {
            app.update(Event::Key(key(KeyCode::Down)));
        }

        let screen = render_to_string(&app, 80, 24);
        assert!(screen.contains(":31:4"));
        assert!(screen.contains("▸"));
        assert!(!screen.contains(":2:4"));
    }

    #[test]
    fn declaration_chooser_renders_its_own_title_and_destination() {
        use nexus_lsp::protocol::{NavigationTarget, Position};

        let directory = TempDir::new("declaration-chooser-render");
        let file = directory.join("declarations.rs");
        fs::write(&file, "fn alvo() {}\n").unwrap();
        let mut app = App::with_text(80, 24, "fn main() {}\n");
        app.open_workspace(&directory.path).unwrap();
        let range = nexus_lsp::protocol::Range {
            start: Position {
                line: 0,
                character: 3,
            },
            end: Position {
                line: 0,
                character: 7,
            },
        };
        app.open_declarations_chooser_for_tests(vec![NavigationTarget {
            uri: nexus_lsp::uri::path_to_uri(&file),
            target_range: range,
            selection_range: range,
            origin_selection_range: None,
        }]);

        let screen = render_to_string(&app, 80, 24);
        assert!(screen.contains("Declarações (1)"));
        assert!(!screen.contains("Definições"));
        assert!(!screen.contains("Referências"));
        assert!(screen.contains("declarations.rs"));
        assert!(screen.contains("fn alvo()"));
        assert!(screen.contains("▸"));
    }

    #[test]
    fn type_definition_chooser_renders_its_own_title_and_destination() {
        use nexus_lsp::protocol::{NavigationTarget, Position};

        let directory = TempDir::new("type-definition-chooser-render");
        let file = directory.join("types.py");
        fs::write(&file, "class Usuario:\n    pass\n").unwrap();
        let mut app = App::with_text(80, 24, "x = Usuario()\n");
        app.open_workspace(&directory.path).unwrap();
        let range = nexus_lsp::protocol::Range {
            start: Position {
                line: 0,
                character: 6,
            },
            end: Position {
                line: 0,
                character: 13,
            },
        };
        app.open_type_definitions_chooser_for_tests(vec![NavigationTarget {
            uri: nexus_lsp::uri::path_to_uri(&file),
            target_range: range,
            selection_range: range,
            origin_selection_range: None,
        }]);

        let screen = render_to_string(&app, 80, 24);
        assert!(screen.contains("Definições de Tipo (1)"));
        assert!(!screen.contains("Declarações"));
        assert!(!screen.contains("Referências"));
        assert!(screen.contains("types.py"));
        assert!(screen.contains("class Usuario:"));
        assert!(screen.contains("▸"));
    }

    #[test]
    fn context_menu_renders_actions_and_target() {
        let directory = TempDir::new("context-menu");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("main.rs"), "fn main() {}").unwrap();
        let mut app = App::with_text(80, 24, "");

        app.open_workspace(&project).unwrap();
        app.update(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        }));

        let screen = render_to_string(&app, 80, 24);

        assert!(screen.contains("main.rs"));
        assert!(screen.contains("Abrir"));
        assert!(screen.contains("Novo arquivo"));
        assert!(screen.contains("Nova pasta"));
        assert!(screen.contains("Renomear"));
        assert!(screen.contains("Excluir"));
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
    fn toggling_explorer_with_ctrl_b_updates_rendering() {
        let directory = TempDir::new("explorer-toggle-render");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::create_dir(project.join("src")).unwrap();
        fs::write(project.join("main.rs"), "fn main() {}").unwrap();
        let mut app = App::with_text(80, 24, "");

        app.open_workspace(&project).unwrap();
        app.open_document_in_new_tab(&project.join("main.rs"))
            .unwrap();
        app.mark_rendered();

        press_ctrl(&mut app, KeyCode::Char('b'));
        assert!(app.needs_redraw());
        let closed = render_to_string(&app, 80, 24);
        assert!(!closed.contains('▸'));
        assert!(closed.contains("main.rs"));

        app.mark_rendered();
        press_ctrl(&mut app, KeyCode::Char('b'));
        assert!(app.needs_redraw());
        let reopened = render_to_string(&app, 80, 24);
        assert!(reopened.contains('▸'));
        assert!(reopened.contains("main.rs"));
        assert!(reopened.contains("fn main()"));
    }

    #[test]
    fn narrow_terminal_hides_explorer_but_keeps_editor_visible() {
        let directory = TempDir::new("explorer-narrow-render");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut app = App::with_text(30, 10, "conteudo");

        app.open_workspace(&project).unwrap();
        assert!(!app.explorer().visible());
        assert!(!app.explorer_focused());
        assert!(app.explorer_visible_width().is_none());

        let screen = render_to_string(&app, 30, 10);

        assert!(!screen.contains('▸'));
        assert!(!screen.contains('▾'));
        assert!(screen.contains("conteudo"));
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

    #[test]
    fn problems_panel_renders_counts_file_message_and_adapts_to_small_terminal() {
        let path = Path::new("/tmp/nexus-problem-panel.rs");
        let uri = nexus_lsp::uri::path_to_uri(path);
        let mut app = App::with_document(60, 12, path, "let value = 1;");
        app.set_diagnostics_for_tests(vec![diagnostic_fixture(
            &uri,
            "/tmp",
            Some(nexus_lsp::protocol::DiagnosticSeverity::Error),
            (0, 4),
            (0, 9),
            DiagnosticFreshness::Current,
            "valor inválido",
        )]);
        app.update(Event::Key(key(KeyCode::F(6))));
        let screen = render_to_string(&app, 60, 12);
        assert!(screen.contains("Problemas"));
        assert!(screen.contains("E:1"));
        assert!(screen.contains("nexus-problem-panel.rs"));
        assert!(screen.contains("valor inválido"));
        let compact = render_to_string(&app, 30, 5);
        assert!(compact.contains("let value"));
    }
}
