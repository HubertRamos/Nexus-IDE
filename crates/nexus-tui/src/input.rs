use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use crate::diagnostics::ProblemFilter;

/// Ações de alto nível produzidas a partir da entrada do usuário.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleProblems,
    FocusProblems,
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
    OutdentSelection,
    DuplicateLines,
    MoveLineUp,
    MoveLineDown,
    InsertNewline,
    Backspace,
    DeleteForward,
    DeletePermanently,
    Undo,
    Redo,
    SelectAll,
    CancelSelection,
    OpenFilePicker,
    OpenWorkspacePicker,
    Save,
    SaveAsPrompt,
    ToggleHelp,
    ToggleExplorer,
    GrowExplorer,
    ShrinkExplorer,
    GoToDefinition,
    GoToDeclaration,
    GoToTypeDefinition,
    FindReferences,
    NavigateBack,
    NavigateForward,
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

/// Atalhos globais da aplicação, válidos independentemente do foco.
///
/// `Ctrl+B` alterna o explorador, `Ctrl+O` abre arquivos e `Ctrl+Shift+O`
/// (ou `F3`, em terminais que não distinguem a combinação) abre um projeto.
/// O chamador decide em quais modos o atalho pode ser aplicado (por exemplo,
/// diálogos modais continuam isolados).
pub fn global_action(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::ALT) {
        return history_action(key);
    }

    if matches!(key.code, KeyCode::F(6)) {
        return Some(if key.modifiers.contains(KeyModifiers::SHIFT) {
            Action::FocusProblems
        } else if key.modifiers.is_empty() {
            Action::ToggleProblems
        } else {
            return None;
        });
    }

    // F3 é a alternativa confiável para abrir projeto em terminais que não
    // distinguem Ctrl+Shift+O de Ctrl+O.
    if key.modifiers.is_empty() && matches!(key.code, KeyCode::F(3)) {
        return Some(Action::OpenWorkspacePicker);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => return Some(quit_or_close_action(key)),
            KeyCode::Char('c') | KeyCode::Char('C') => return Some(Action::Quit),
            _ => {}
        }

        if let Some(action) = open_picker_action(key) {
            return Some(action);
        }

        if matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B')) {
            return Some(Action::ToggleExplorer);
        }
    }

    None
}

/// `Alt+←` e `Alt+→` percorrem o histórico de navegação.
///
/// A combinação é entregue por terminais que usam o protocolo estendido do
/// Crossterm. `Ctrl` e `Shift` são rejeitados para não capturar variantes de
/// seleção por palavra usadas por alguns terminais.
fn history_action(key: KeyEvent) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::SHIFT)
    {
        return None;
    }

    match key.code {
        KeyCode::Left => Some(Action::NavigateBack),
        KeyCode::Right => Some(Action::NavigateForward),
        _ => None,
    }
}

/// `Ctrl+Q` fecha a aba; `Ctrl+Shift+Q` encerra o aplicativo.
///
/// Assim como em `Ctrl+Shift+O`, protocolos estendidos podem remover `SHIFT` e
/// entregar `Q` maiúsculo. No Windows `SHIFT` é preservado, então `Q` sem
/// `SHIFT` continua sendo `Ctrl+Q` sob Caps Lock.
fn quit_or_close_action(key: KeyEvent) -> Action {
    let quit = key.modifiers.contains(KeyModifiers::SHIFT)
        || (!cfg!(windows)
            && matches!(key.code, KeyCode::Char('Q'))
            && !key.state.contains(KeyEventState::CAPS_LOCK));

    if quit { Action::Quit } else { Action::CloseTab }
}

/// Distingue Ctrl+O de Ctrl+Shift+O considerando as representações do Crossterm.
///
/// Em terminais Unix legados, `Ctrl+Shift+O` é indistinguível de `Ctrl+O`.
/// Quando o protocolo estendido está ativo, o Crossterm pode remover `SHIFT` e
/// entregar `Char('O')`; o caractere maiúsculo passa a ser o sinal disponível.
/// No Windows `SHIFT` é sempre preservado; um `O` maiúsculo sem `SHIFT` ali
/// indica Caps Lock e deve continuar abrindo arquivos.
fn open_picker_action(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('o') | KeyCode::Char('O') => {
            let workspace = key.modifiers.contains(KeyModifiers::SHIFT)
                || (!cfg!(windows)
                    && matches!(key.code, KeyCode::Char('O'))
                    && !key.state.contains(KeyEventState::CAPS_LOCK));

            Some(if workspace {
                Action::OpenWorkspacePicker
            } else {
                Action::OpenFilePicker
            })
        }
        _ => None,
    }
}

/// Traduz uma tecla em uma ação do editor.
///
/// O editor é modeless para digitação: caracteres imprimíveis são inseridos
/// diretamente. Atalhos usam `Ctrl` e seleção usa `Shift`. Eventos de liberação
/// são ignorados para evitar ações duplicadas no Windows.
pub fn action(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if key.modifiers.contains(KeyModifiers::ALT) {
        if control || shift {
            return None;
        }

        // Alt+↑/↓ não dependem de teclas de função e são entregues como
        // modificador comum pela maioria dos terminais modernos.
        return match key.code {
            KeyCode::Up => Some(Action::MoveLineUp),
            KeyCode::Down => Some(Action::MoveLineDown),
            _ => None,
        };
    }

    if control {
        return match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Quit),
            KeyCode::Char('q') | KeyCode::Char('Q') => Some(quit_or_close_action(key)),
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if shift {
                    Some(Action::SaveAsPrompt)
                } else {
                    Some(Action::Save)
                }
            }
            KeyCode::Char('o') | KeyCode::Char('O') => open_picker_action(key),
            KeyCode::Char('z') | KeyCode::Char('Z') => {
                if shift {
                    Some(Action::Redo)
                } else {
                    Some(Action::Undo)
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Action::Redo),
            KeyCode::Char('a') | KeyCode::Char('A') => Some(Action::SelectAll),
            KeyCode::Char('d') | KeyCode::Char('D') => Some(Action::DuplicateLines),
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
            // Ctrl+T é transmitido como um único caractere de controle, sem
            // depender de modificadores em teclas de função.
            KeyCode::Char('t') | KeyCode::Char('T') if !shift => Some(Action::GoToTypeDefinition),
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
            // Ctrl+F12 navega para a declaração. Ctrl+Shift+F12 continua
            // reservado para evitar conflito com Shift+F12 (referências).
            KeyCode::F(12) if !shift => Some(Action::GoToDeclaration),
            _ => None,
        };
    }

    let select = shift;

    match key.code {
        KeyCode::BackTab => Some(Action::OutdentSelection),
        KeyCode::Char(character) => Some(Action::InsertChar(character)),
        KeyCode::Enter => Some(Action::InsertNewline),
        KeyCode::Tab if shift => Some(Action::OutdentSelection),
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
        KeyCode::F(3) => Some(Action::OpenWorkspacePicker),
        // F12, Shift+F12 e Ctrl+F12 dependem de o terminal transmitir os
        // modificadores em teclas de função. Terminais que achatam a combinação
        // entregam apenas F12; nesse caso os modificadores não são
        // reinterpretados para evitar conflitos.
        KeyCode::F(12) => {
            if shift {
                Some(Action::FindReferences)
            } else {
                Some(Action::GoToDefinition)
            }
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationChoiceAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Activate,
    Cancel,
}

/// Teclas do seletor modal de definições.
pub fn navigation_choice_action(key: KeyEvent) -> Option<NavigationChoiceAction> {
    if key.kind == KeyEventKind::Release || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('K') => Some(NavigationChoiceAction::Up),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('J') => {
            Some(NavigationChoiceAction::Down)
        }
        KeyCode::PageUp => Some(NavigationChoiceAction::PageUp),
        KeyCode::PageDown => Some(NavigationChoiceAction::PageDown),
        KeyCode::Enter => Some(NavigationChoiceAction::Activate),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            Some(NavigationChoiceAction::Cancel)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemsAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Activate,
    Close,
    FocusEditor,
    Filter(ProblemFilter),
}

/// Teclas exclusivas do painel; números não são capturados quando o foco está
/// no editor, portanto continuam inserindo texto normalmente.
pub fn problems_action(key: KeyEvent) -> Option<ProblemsAction> {
    if key.kind == KeyEventKind::Release || key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }
    if key.modifiers.is_empty() {
        return match key.code {
            KeyCode::Up => Some(ProblemsAction::Up),
            KeyCode::Down => Some(ProblemsAction::Down),
            KeyCode::PageUp => Some(ProblemsAction::PageUp),
            KeyCode::PageDown => Some(ProblemsAction::PageDown),
            KeyCode::Enter => Some(ProblemsAction::Activate),
            KeyCode::Esc => Some(ProblemsAction::Close),
            KeyCode::Tab => Some(ProblemsAction::FocusEditor),
            KeyCode::Char('0') => Some(ProblemsAction::Filter(ProblemFilter::All)),
            KeyCode::Char('1') => Some(ProblemsAction::Filter(ProblemFilter::Errors)),
            KeyCode::Char('2') => Some(ProblemsAction::Filter(ProblemFilter::Warnings)),
            KeyCode::Char('3') => Some(ProblemsAction::Filter(ProblemFilter::Information)),
            KeyCode::Char('4') => Some(ProblemsAction::Filter(ProblemFilter::Hints)),
            _ => None,
        };
    }
    None
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

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn maps_alt_left_and_right_to_navigation_history() {
        assert_eq!(
            global_action(alt(KeyCode::Left)),
            Some(Action::NavigateBack)
        );
        assert_eq!(
            global_action(alt(KeyCode::Right)),
            Some(Action::NavigateForward)
        );
        assert_eq!(
            global_action(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            )),
            None
        );
        assert_eq!(
            global_action(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            None
        );
        assert_eq!(global_action(alt(KeyCode::Char('a'))), None);

        let release =
            KeyEvent::new_with_kind(KeyCode::Left, KeyModifiers::ALT, KeyEventKind::Release);
        assert_eq!(global_action(release), None);
    }

    #[test]
    fn maps_ctrl_c_to_quit() {
        assert_eq!(action(ctrl(KeyCode::Char('c'))), Some(Action::Quit));
    }

    #[test]
    fn f6_shortcuts_are_reserved_for_problems_without_capturing_editor_digits() {
        assert_eq!(
            global_action(key(KeyCode::F(6))),
            Some(Action::ToggleProblems)
        );
        assert_eq!(
            global_action(shift(KeyCode::F(6))),
            Some(Action::FocusProblems)
        );
        assert_eq!(
            action(key(KeyCode::Char('1'))),
            Some(Action::InsertChar('1'))
        );
        assert_eq!(
            problems_action(key(KeyCode::Char('1'))),
            Some(ProblemsAction::Filter(ProblemFilter::Errors))
        );
    }

    #[test]
    fn navigation_choice_maps_selection_confirmation_and_cancel() {
        assert_eq!(
            navigation_choice_action(key(KeyCode::Up)),
            Some(NavigationChoiceAction::Up)
        );
        assert_eq!(
            navigation_choice_action(key(KeyCode::Down)),
            Some(NavigationChoiceAction::Down)
        );
        assert_eq!(
            navigation_choice_action(key(KeyCode::Char('k'))),
            Some(NavigationChoiceAction::Up)
        );
        assert_eq!(
            navigation_choice_action(key(KeyCode::Char('j'))),
            Some(NavigationChoiceAction::Down)
        );
        assert_eq!(
            navigation_choice_action(key(KeyCode::PageUp)),
            Some(NavigationChoiceAction::PageUp)
        );
        assert_eq!(
            navigation_choice_action(key(KeyCode::PageDown)),
            Some(NavigationChoiceAction::PageDown)
        );
        assert_eq!(
            navigation_choice_action(key(KeyCode::Enter)),
            Some(NavigationChoiceAction::Activate)
        );
        assert_eq!(
            navigation_choice_action(key(KeyCode::Esc)),
            Some(NavigationChoiceAction::Cancel)
        );
        assert_eq!(navigation_choice_action(ctrl(KeyCode::Enter)), None);
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
        assert_eq!(
            action(shift(KeyCode::Tab)),
            Some(Action::OutdentSelection)
        );
        assert_eq!(
            action(key(KeyCode::BackTab)),
            Some(Action::OutdentSelection)
        );
        assert_eq!(action(key(KeyCode::Enter)), Some(Action::InsertNewline));
        assert_eq!(action(key(KeyCode::Backspace)), Some(Action::Backspace));
        assert_eq!(action(key(KeyCode::Delete)), Some(Action::DeleteForward));
        assert_eq!(action(key(KeyCode::Esc)), Some(Action::CancelSelection));
    }

    #[test]
    fn maps_block_editing_shortcuts_without_conflicts() {
        assert_eq!(
            action(ctrl(KeyCode::Char('d'))),
            Some(Action::DuplicateLines)
        );
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Char('D'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::DuplicateLines)
        );
        assert_eq!(action(alt(KeyCode::Up)), Some(Action::MoveLineUp));
        assert_eq!(action(alt(KeyCode::Down)), Some(Action::MoveLineDown));

        // Variantes com Shift e Ctrl+Alt não são reinterpretadas.
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            None
        );
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            )),
            None
        );

        // F6 continua sendo o caminho para o painel de problemas; Shift+Tab
        // agora pertence à desindentação.
        assert_eq!(
            global_action(key(KeyCode::F(6))),
            Some(Action::ToggleProblems)
        );
        assert_eq!(
            global_action(shift(KeyCode::F(6))),
            Some(Action::FocusProblems)
        );
    }

    #[test]
    fn ctrl_b_is_a_global_shortcut() {
        assert_eq!(
            global_action(ctrl(KeyCode::Char('b'))),
            Some(Action::ToggleExplorer)
        );
        assert_eq!(
            global_action(ctrl(KeyCode::Char('B'))),
            Some(Action::ToggleExplorer)
        );
        assert_eq!(
            global_action(KeyEvent::new(
                KeyCode::Char('B'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::ToggleExplorer)
        );
        assert_eq!(global_action(key(KeyCode::Char('b'))), None);
        assert_eq!(global_action(key(KeyCode::F(1))), None);

        let release = KeyEvent::new_with_kind(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );
        assert_eq!(global_action(release), None);
        assert_eq!(
            global_action(KeyEvent::new(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            None
        );
    }

    #[test]
    fn open_shortcuts_are_global() {
        assert_eq!(
            global_action(ctrl(KeyCode::Char('o'))),
            Some(Action::OpenFilePicker)
        );
        assert_eq!(
            global_action(KeyEvent::new(
                KeyCode::Char('O'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::OpenWorkspacePicker)
        );
        assert_eq!(
            global_action(KeyEvent::new(
                KeyCode::Char('o'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::OpenWorkspacePicker)
        );

        // Alguns terminais/protocolos removem SHIFT e entregam 'O' maiúsculo.
        let uppercase_without_shift = KeyEvent::new(KeyCode::Char('O'), KeyModifiers::CONTROL);
        if cfg!(windows) {
            // No Windows SHIFT é preservado; 'O' sem SHIFT é Caps Lock.
            assert_eq!(
                global_action(uppercase_without_shift),
                Some(Action::OpenFilePicker)
            );
        } else {
            assert_eq!(
                global_action(uppercase_without_shift),
                Some(Action::OpenWorkspacePicker)
            );

            let caps_lock = KeyEvent::new_with_kind_and_state(
                KeyCode::Char('O'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
                KeyEventState::CAPS_LOCK,
            );
            assert_eq!(global_action(caps_lock), Some(Action::OpenFilePicker));
        }

        assert_eq!(global_action(key(KeyCode::Char('o'))), None);
        assert_eq!(
            global_action(key(KeyCode::F(3))),
            Some(Action::OpenWorkspacePicker)
        );
        assert_eq!(
            global_action(KeyEvent::new(KeyCode::F(3), KeyModifiers::CONTROL)),
            None
        );

        for code in [KeyCode::Char('o'), KeyCode::Char('O')] {
            let with_alt = KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::ALT);
            assert_eq!(global_action(with_alt), None);

            let combined = KeyEvent::new(
                code,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
            );
            assert_eq!(global_action(combined), None);

            let release =
                KeyEvent::new_with_kind(code, KeyModifiers::CONTROL, KeyEventKind::Release);
            assert_eq!(global_action(release), None);
        }
    }

    #[test]
    fn legacy_terminal_ctrl_shift_o_collapses_to_ctrl_o_and_f3_remains_available() {
        // Em terminais Unix sem protocolo estendido, Ctrl+Shift+O e Ctrl+O
        // produzem o mesmo byte 0x0F e o mesmo KeyEvent.
        let legacy = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(global_action(legacy), Some(Action::OpenFilePicker));

        assert_eq!(
            global_action(key(KeyCode::F(3))),
            Some(Action::OpenWorkspacePicker)
        );
    }

    #[test]
    fn maps_editor_commands() {
        assert_eq!(action(ctrl(KeyCode::Char('z'))), Some(Action::Undo));
        assert_eq!(action(ctrl(KeyCode::Char('y'))), Some(Action::Redo));
        assert_eq!(action(ctrl(KeyCode::Char('a'))), Some(Action::SelectAll));
        assert_eq!(
            action(ctrl(KeyCode::Char('o'))),
            Some(Action::OpenFilePicker)
        );
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Char('O'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::OpenWorkspacePicker)
        );
        assert_eq!(action(ctrl(KeyCode::Char('s'))), Some(Action::Save));
        assert_eq!(action(ctrl(KeyCode::Char('h'))), Some(Action::ToggleHelp));
        assert_eq!(
            action(ctrl(KeyCode::Char('t'))),
            Some(Action::GoToTypeDefinition)
        );
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Char('T'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            None
        );
        assert_eq!(action(key(KeyCode::F(1))), Some(Action::ToggleHelp));
        assert_eq!(action(key(KeyCode::F(2))), Some(Action::SaveAsPrompt));
        assert_eq!(
            action(key(KeyCode::F(3))),
            Some(Action::OpenWorkspacePicker)
        );
        assert_eq!(action(key(KeyCode::F(12))), Some(Action::GoToDefinition));
        assert_eq!(action(shift(KeyCode::F(12))), Some(Action::FindReferences));
        assert_eq!(action(ctrl(KeyCode::F(12))), Some(Action::GoToDeclaration));
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::F(12),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            None
        );
        assert_eq!(action(ctrl(KeyCode::Char('q'))), Some(Action::CloseTab));
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::Quit)
        );
        assert_eq!(
            action(KeyEvent::new(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::Quit)
        );
    }

    #[test]
    fn global_action_maps_tab_close_and_quit() {
        assert_eq!(
            global_action(ctrl(KeyCode::Char('q'))),
            Some(Action::CloseTab)
        );
        assert_eq!(
            global_action(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Some(Action::Quit)
        );

        // Alguns protocolos removem SHIFT e entregam 'Q' maiúsculo.
        let uppercase_without_shift = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::CONTROL);
        if cfg!(windows) {
            assert_eq!(
                global_action(uppercase_without_shift),
                Some(Action::CloseTab)
            );
        } else {
            assert_eq!(global_action(uppercase_without_shift), Some(Action::Quit));

            let caps_lock = KeyEvent::new_with_kind_and_state(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
                KeyEventState::CAPS_LOCK,
            );
            assert_eq!(global_action(caps_lock), Some(Action::CloseTab));
        }
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
