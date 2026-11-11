use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Ações de alto nível produzidas a partir da entrada do usuário.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    MoveLeft { select: bool },
    MoveRight { select: bool },
    MoveUp { select: bool },
    MoveDown { select: bool },
    MoveLineStart { select: bool },
    MoveLineEnd { select: bool },
    MoveDocStart { select: bool },
    MoveDocEnd { select: bool },
    MovePageUp { select: bool },
    MovePageDown { select: bool },
    InsertChar(char),
    InsertTab,
    InsertNewline,
    Backspace,
    DeleteForward,
    DeletePermanently,
    Undo,
    Redo,
    SelectAll,
    CancelSelection,
    OpenPrompt,
    Save,
    SaveAsPrompt,
    ToggleHelp,
    ToggleExplorer,
    GrowExplorer,
    ShrinkExplorer,
    CloseTab,
    NextTab,
    PreviousTab,
    ReloadFromDisk,
    NewFile,
    NewDirectory,
    ToggleHidden,
    CloseAllTabs,
    PromptChar(char),
    PromptBackspace,
    PromptDelete,
    PromptLeft,
    PromptRight,
    PromptHome,
    PromptEnd,
    PromptConfirm,
    PromptCancel,
}

/// Ações aceitas enquanto a confirmação de alterações está aberta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    Save,
    Discard,
    Cancel,
    MoveLeft,
    MoveRight,
    Activate,
}

/// Traduz uma tecla em uma ação do editor.
///
/// O editor é modeless para digitação: caracteres imprimíveis são inseridos
/// diretamente. Atalhos usam `Ctrl` e seleção usa `Shift`. Eventos de liberação
/// são ignorados para evitar ações duplicadas no Windows.
pub fn action(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if control {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Quit),
            KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if shift {
                    Some(Action::SaveAsPrompt)
                } else {
                    Some(Action::Save)
                }
            }
            KeyCode::Char('o') | KeyCode::Char('O') => Some(Action::OpenPrompt),
            KeyCode::Char('z') | KeyCode::Char('Z') => {
                if shift {
                    Some(Action::Redo)
                } else {
                    Some(Action::Undo)
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Action::Redo),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(Action::SelectAll),
            KeyCode::Char('h') | KeyCode::Char('H') => {
                if shift {
                    Some(Action::ToggleHidden)
                } else {
                    Some(Action::ToggleHelp)
                }
            }
            KeyCode::Char('b') | KeyCode::Char('B') => Some(Action::ToggleExplorer),
            KeyCode::Char('w') | KeyCode::Char('W') => {
                if shift {
                    Some(Action::CloseAllTabs)
                } else {
                    Some(Action::CloseTab)
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => Some(Action::ReloadFromDisk),
            KeyCode::Char('n') | KeyCode::Char('N') => {
                if shift {
                    Some(Action::NewDirectory)
                } else {
                    Some(Action::NewFile)
                }
            }
            KeyCode::Left => Some(Action::ShrinkExplorer),
            KeyCode::Right => Some(Action::GrowExplorer),
            KeyCode::Home => Some(Action::MoveDocStart { select: shift }),
            KeyCode::End => Some(Action::MoveDocEnd { select: shift }),
            KeyCode::PageUp => Some(Action::PreviousTab),
            KeyCode::PageDown => Some(Action::NextTab),
            _ => None,
        };
    }

    let select = shift;

    match key.code {
        KeyCode::Char(character) => Some(Action::InsertChar(character)),
        KeyCode::Enter => Some(Action::InsertNewline),
        KeyCode::Tab => Some(Action::InsertTab),
        KeyCode::Backspace => Some(Action::Backspace),
        KeyCode::Delete => {
            if shift {
                Some(Action::DeletePermanently)
            } else {
                Some(Action::DeleteForward)
            }
        }
        KeyCode::Left => Some(Action::MoveLeft { select }),
        KeyCode::Right => Some(Action::MoveRight { select }),
        KeyCode::Up => Some(Action::MoveUp { select }),
        KeyCode::Down => Some(Action::MoveDown { select }),
        KeyCode::Home => Some(Action::MoveLineStart { select }),
        KeyCode::End => Some(Action::MoveLineEnd { select }),
        KeyCode::PageUp => Some(Action::MovePageUp { select }),
        KeyCode::PageDown => Some(Action::MovePageDown { select }),
        KeyCode::Esc => Some(Action::CancelSelection),
        KeyCode::F(1) => Some(Action::ToggleHelp),
        KeyCode::F(2) => Some(Action::SaveAsPrompt),
        _ => None,
    }
}

/// Traduz uma tecla enquanto o prompt de caminho está aberto.
pub fn prompt_action(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Quit),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Char(character) => Some(Action::PromptChar(character)),
        KeyCode::Enter => Some(Action::PromptConfirm),
        KeyCode::Esc => Some(Action::PromptCancel),
        KeyCode::Backspace => Some(Action::PromptBackspace),
        KeyCode::Delete => Some(Action::PromptDelete),
        KeyCode::Left => Some(Action::PromptLeft),
        KeyCode::Right => Some(Action::PromptRight),
        KeyCode::Home => Some(Action::PromptHome),
        KeyCode::End => Some(Action::PromptEnd),
        _ => None,
    }
}

/// Traduz uma tecla na confirmação de alterações não salvas.
///
/// Somente teclas explícitas são aceitas. Atalhos do editor e do prompt não
/// vazam para o documento enquanto o diálogo está aberto.
pub fn confirm_action(key: KeyEvent) -> Option<ConfirmAction> {
    if key.kind == KeyEventKind::Release || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }

    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char('s') | KeyCode::Char('S') => Some(ConfirmAction::Save),
        KeyCode::Char('d') | KeyCode::Char('D') => Some(ConfirmAction::Discard),
        KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Esc => Some(ConfirmAction::Cancel),
        KeyCode::Enter => Some(ConfirmAction::Activate),
        KeyCode::Left => Some(ConfirmAction::MoveLeft),
        KeyCode::Right => Some(ConfirmAction::MoveRight),
        KeyCode::Tab => {
            if shift {
                Some(ConfirmAction::MoveLeft)
            } else {
                Some(ConfirmAction::MoveRight)
            }
        }
        KeyCode::BackTab => Some(ConfirmAction::MoveLeft),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn maps_ctrl_c_to_quit() {
        assert_eq!(action(ctrl(KeyCode::Char('c'))), Some(Action::Quit));
    }

    #[test]
    fn ignores_ctrl_c_release() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );

        assert_eq!(action(key), None);
    }

    #[test]
    fn plain_c_inserts_character() {
        assert_eq!(
            action(key(KeyCode::Char('c'))),
            Some(Action::InsertChar('c'))
        );
    }

    #[test]
    fn maps_navigation_and_selection() {
        assert_eq!(
            action(key(KeyCode::Left)),
            Some(Action::MoveLeft { select: false })
        );
        assert_eq!(
            action(shift(KeyCode::Left)),
            Some(Action::MoveLeft { select: true })
        );
        assert_eq!(
            action(key(KeyCode::Right)),
            Some(Action::MoveRight { select: false })
        );
        assert_eq!(
            action(shift(KeyCode::Up)),
            Some(Action::MoveUp { select: true })
        );
        assert_eq!(
            action(shift(KeyCode::Down)),
            Some(Action::MoveDown { select: true })
        );
        assert_eq!(
            action(key(KeyCode::Home)),
            Some(Action::MoveLineStart { select: false })
        );
        assert_eq!(
            action(shift(KeyCode::End)),
            Some(Action::MoveLineEnd { select: true })
        );
        assert_eq!(action(ctrl(KeyCode::Left)), Some(Action::ShrinkExplorer));
        assert_eq!(action(ctrl(KeyCode::Right)), Some(Action::GrowExplorer));
        assert_eq!(
            action(ctrl(KeyCode::Home)),
            Some(Action::MoveDocStart { select: false })
        );
        assert_eq!(
            action(ctrl(KeyCode::End)),
            Some(Action::MoveDocEnd { select: false })
        );
        assert_eq!(
            action(shift(KeyCode::PageUp)),
            Some(Action::MovePageUp { select: true })
        );
        assert_eq!(
            action(shift(KeyCode::PageDown)),
            Some(Action::MovePageDown { select: true })
        );
    }

    #[test]
    fn maps_editing_keys() {
        assert_eq!(
            action(key(KeyCode::Char(' '))),
            Some(Action::InsertChar(' '))
        );
        assert_eq!(
            action(key(KeyCode::Char('é'))),
            Some(Action::InsertChar('é'))
        );
        assert_eq!(action(key(KeyCode::Tab)), Some(Action::InsertTab));
        assert_eq!(action(key(KeyCode::Enter)), Some(Action::InsertNewline));
        assert_eq!(action(key(KeyCode::Backspace)), Some(Action::Backspace));
        assert_eq!(action(key(KeyCode::Delete)), Some(Action::DeleteForward));
        assert_eq!(action(key(KeyCode::Esc)), Some(Action::CancelSelection));
    }

    #[test]
    fn maps_editor_commands() {
        assert_eq!(action(ctrl(KeyCode::Char('z'))), Some(Action::Undo));
        assert_eq!(action(ctrl(KeyCode::Char('y'))), Some(Action::Redo));
        assert_eq!(action(ctrl(KeyCode::Char('a'))), Some(Action::SelectAll));
        assert_eq!(action(ctrl(KeyCode::Char('o'))), Some(Action::OpenPrompt));
        assert_eq!(action(ctrl(KeyCode::Char('s'))), Some(Action::Save));
        assert_eq!(action(ctrl(KeyCode::Char('h'))), Some(Action::ToggleHelp));
        assert_eq!(action(key(KeyCode::F(1))), Some(Action::ToggleHelp));
        assert_eq!(action(key(KeyCode::F(2))), Some(Action::SaveAsPrompt));
        assert_eq!(action(ctrl(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn maps_shifted_commands() {
        let save_as = KeyEvent::new(
            KeyCode::Char('S'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let redo = KeyEvent::new(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );

        assert_eq!(action(save_as), Some(Action::SaveAsPrompt));
        assert_eq!(action(redo), Some(Action::Redo));
    }

    #[test]
    fn ignores_alt_modified_keys() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);

        assert_eq!(action(key), None);
    }

    #[test]
    fn prompt_maps_text_and_control_keys() {
        assert_eq!(
            prompt_action(key(KeyCode::Char('a'))),
            Some(Action::PromptChar('a'))
        );
        assert_eq!(
            prompt_action(key(KeyCode::Char('é'))),
            Some(Action::PromptChar('é'))
        );
        assert_eq!(
            prompt_action(key(KeyCode::Enter)),
            Some(Action::PromptConfirm)
        );
        assert_eq!(prompt_action(key(KeyCode::Esc)), Some(Action::PromptCancel));
        assert_eq!(
            prompt_action(key(KeyCode::Backspace)),
            Some(Action::PromptBackspace)
        );
        assert_eq!(
            prompt_action(key(KeyCode::Delete)),
            Some(Action::PromptDelete)
        );
        assert_eq!(prompt_action(key(KeyCode::Left)), Some(Action::PromptLeft));
        assert_eq!(
            prompt_action(key(KeyCode::Right)),
            Some(Action::PromptRight)
        );
        assert_eq!(prompt_action(key(KeyCode::Home)), Some(Action::PromptHome));
        assert_eq!(prompt_action(key(KeyCode::End)), Some(Action::PromptEnd));
    }

    #[test]
    fn prompt_does_not_insert_control_characters() {
        assert_eq!(prompt_action(ctrl(KeyCode::Char('a'))), None);
    }

    #[test]
    fn confirm_maps_direct_choices() {
        assert_eq!(
            confirm_action(key(KeyCode::Char('s'))),
            Some(ConfirmAction::Save)
        );
        assert_eq!(
            confirm_action(key(KeyCode::Char('S'))),
            Some(ConfirmAction::Save)
        );
        assert_eq!(
            confirm_action(key(KeyCode::Char('d'))),
            Some(ConfirmAction::Discard)
        );
        assert_eq!(
            confirm_action(key(KeyCode::Char('c'))),
            Some(ConfirmAction::Cancel)
        );
        assert_eq!(
            confirm_action(key(KeyCode::Esc)),
            Some(ConfirmAction::Cancel)
        );
    }

    #[test]
    fn confirm_maps_navigation_and_activation() {
        assert_eq!(
            confirm_action(key(KeyCode::Left)),
            Some(ConfirmAction::MoveLeft)
        );
        assert_eq!(
            confirm_action(key(KeyCode::Right)),
            Some(ConfirmAction::MoveRight)
        );
        assert_eq!(
            confirm_action(key(KeyCode::Tab)),
            Some(ConfirmAction::MoveRight)
        );
        assert_eq!(
            confirm_action(shift(KeyCode::Tab)),
            Some(ConfirmAction::MoveLeft)
        );
        assert_eq!(
            confirm_action(key(KeyCode::BackTab)),
            Some(ConfirmAction::MoveLeft)
        );
        assert_eq!(
            confirm_action(key(KeyCode::Enter)),
            Some(ConfirmAction::Activate)
        );
    }

    #[test]
    fn confirm_does_not_leak_editor_shortcuts() {
        assert_eq!(confirm_action(ctrl(KeyCode::Char('q'))), None);
        assert_eq!(confirm_action(ctrl(KeyCode::Char('s'))), None);
        assert_eq!(confirm_action(ctrl(KeyCode::Char('z'))), None);
        assert_eq!(confirm_action(key(KeyCode::Char('x'))), None);
        assert_eq!(confirm_action(key(KeyCode::F(1))), None);
    }
}
