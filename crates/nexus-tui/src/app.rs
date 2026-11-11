use std::fmt;
use std::path::{Path, PathBuf};

use nexus_core::buffer::Buffer;
use nexus_core::buffer::Transaction;
use nexus_core::buffer::edit::BufferError;
use nexus_core::buffer::edit::{AppliedEdit, Edit, TextRange};
use nexus_core::buffer::position::CharOffset;
use nexus_syntax::{LanguageId, SyntaxState};

use crate::document::{Document, DocumentManager, TabInfo, detect_line_ending};
use crate::event::Event;
use crate::explorer::{Explorer, ExplorerActivation};
use crate::files::{self, FileError};
use crate::input::{Action, ConfirmAction};
use crate::paths::AppPaths;
use crate::recent::{RecentEntry, RecentProjects, RecentStore};
use crate::recovery::{RECOVERY_VERSION, RecoveryManager, RecoveryRecord, now_seconds};
use crate::session::{Session, SessionDocument, SessionStore};
use crate::theme::{ColorMode, ThemeState};
use crate::view::{Motion, View, char_width, editor_content_size};
use crate::watcher::{DetectedChange, FileFingerprint, FileWatcher, fingerprint};
use crate::workspace::Workspace;

/// Modo de interação da interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Editor,
    Prompt(PromptKind),
    Help,
    Confirm,
}

/// Motivo da confirmação aberta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Quit,
    Open,
    CloseTab,
    Delete,
}

/// Escolha oferecida na confirmação de alterações não salvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Save,
    Discard,
    Cancel,
}

impl ConfirmChoice {
    fn next(self, save_available: bool) -> Self {
        match (self, save_available) {
            (ConfirmChoice::Save, _) => ConfirmChoice::Discard,
            (ConfirmChoice::Discard, _) => ConfirmChoice::Cancel,
            (ConfirmChoice::Cancel, true) => ConfirmChoice::Save,
            (ConfirmChoice::Cancel, false) => ConfirmChoice::Discard,
        }
    }

    fn previous(self, save_available: bool) -> Self {
        match (self, save_available) {
            (ConfirmChoice::Save, _) => ConfirmChoice::Cancel,
            (ConfirmChoice::Discard, true) => ConfirmChoice::Save,
            (ConfirmChoice::Discard, false) => ConfirmChoice::Cancel,
            (ConfirmChoice::Cancel, _) => ConfirmChoice::Discard,
        }
    }
}

/// Ação que será concluída depois de salvar ou descartar as alterações.
#[derive(Debug, Clone)]
enum PendingAction {
    Quit,
    Open(PathBuf),
    CloseTab,
    Delete { path: PathBuf, permanent: bool },
}

impl PendingAction {
    fn is_delete(&self) -> bool {
        matches!(self, PendingAction::Delete { .. })
    }
}

/// Diálogo de confirmação de alterações não salvas.
#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pending: PendingAction,
    selected: ConfirmChoice,
    save_available: bool,
    error: Option<String>,
}

impl ConfirmDialog {
    pub fn kind(&self) -> ConfirmKind {
        match &self.pending {
            PendingAction::Quit => ConfirmKind::Quit,
            PendingAction::Open(_) => ConfirmKind::Open,
            PendingAction::CloseTab => ConfirmKind::CloseTab,
            PendingAction::Delete { .. } => ConfirmKind::Delete,
        }
    }

    pub fn selected(&self) -> ConfirmChoice {
        self.selected
    }

    pub fn save_available(&self) -> bool {
        self.save_available
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn target(&self) -> Option<&Path> {
        match &self.pending {
            PendingAction::Open(path) | PendingAction::Delete { path, .. } => Some(path),
            PendingAction::Quit | PendingAction::CloseTab => None,
        }
    }

    /// A ação destrutiva é confirmada por `Descartar/Excluir`, sem salvar.
    pub fn destructive_only(&self) -> bool {
        self.pending.is_delete()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Open,
    SaveAs,
    NewFile,
    NewDirectory,
    Rename,
}

impl PromptKind {
    pub fn title(self) -> &'static str {
        match self {
            PromptKind::Open => "Abrir arquivo",
            PromptKind::SaveAs => "Salvar como",
            PromptKind::NewFile => "Novo arquivo",
            PromptKind::NewDirectory => "Nova pasta",
            PromptKind::Rename => "Renomear",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusMessage {
    pub kind: MessageKind,
    pub text: String,
}

/// Conteúdo da tela inicial exibida quando o Nexus abre sem sessão.
#[derive(Debug, Clone, Default)]
pub struct StartScreen {
    items: Vec<RecentEntry>,
    selected: usize,
}

impl StartScreen {
    pub fn items(&self) -> &[RecentEntry] {
        &self.items
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }

        let maximum = self.items.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, maximum as isize) as usize;
    }

    fn selected_entry(&self) -> Option<&RecentEntry> {
        self.items.get(self.selected)
    }
}

/// Prompt de caminho de arquivo.
#[derive(Debug, Clone)]
pub struct Prompt {
    kind: PromptKind,
    input: String,
    cursor: usize,
    /// Caminho alvo de operações do explorador (pasta pai ou item renomeado).
    target: Option<PathBuf>,
}

impl Prompt {
    fn new(kind: PromptKind, input: String) -> Self {
        let cursor = input.chars().count();
        Self {
            kind,
            input,
            cursor,
            target: None,
        }
    }

    fn with_target(kind: PromptKind, input: String, target: PathBuf) -> Self {
        let mut prompt = Self::new(kind, input);
        prompt.target = Some(target);
        prompt
    }

    pub fn kind(&self) -> PromptKind {
        self.kind
    }

    pub fn target(&self) -> Option<&Path> {
        self.target.as_deref()
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Índice do primeiro caractere visível para uma largura interna.
    ///
    /// O conteúdo completo permanece em `input`; este método calcula apenas a
    /// janela horizontal usada na renderização, mantendo o cursor visível.
    pub fn visible_start(&self, width: usize) -> usize {
        prompt_visible_start(&self.input, self.cursor, width)
    }

    fn insert_char(&mut self, character: char) {
        let byte_index = self.byte_index(self.cursor);
        self.input.insert(byte_index, character);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }

        let start = self.byte_index(self.cursor - 1);
        let end = self.byte_index(self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }

        let start = self.byte_index(self.cursor);
        let end = self.byte_index(self.cursor + 1);
        self.input.replace_range(start..end, "");
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_index)
            .map(|(index, _)| index)
            .unwrap_or(self.input.len())
    }
}

#[derive(Debug)]
pub enum DocumentError {
    Buffer(BufferError),
    File(FileError),
}

impl fmt::Display for DocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentError::Buffer(error) => write!(formatter, "{error}"),
            DocumentError::File(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for DocumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DocumentError::Buffer(error) => Some(error),
            DocumentError::File(error) => Some(error),
        }
    }
}

impl From<BufferError> for DocumentError {
    fn from(error: BufferError) -> Self {
        DocumentError::Buffer(error)
    }
}

impl From<FileError> for DocumentError {
    fn from(error: FileError) -> Self {
        DocumentError::File(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    DeleteBackward,
    DeleteForward,
}

pub struct App {
    documents: DocumentManager,
    workspace: Option<Workspace>,
    explorer: Explorer,
    file_watcher: Option<FileWatcher>,
    recovery_manager: RecoveryManager,
    session_store: SessionStore,
    recent_store: RecentStore,
    paths: AppPaths,
    buffer: Buffer,
    view: View,
    path: Option<PathBuf>,
    document_name: String,
    line_ending: String,
    /// Fingerprint do disco conhecido para o documento ativo.
    disk: Option<FileFingerprint>,
    /// Conteúdo do documento ativo veio de uma cópia de recuperação.
    recovered: bool,
    /// O arquivo associado ao documento ativo desapareceu do disco.
    missing: bool,
    /// Detectamos uma alteração externa enquanto o buffer tinha edições.
    conflict: bool,
    /// Identificador da cópia de recuperação do documento ativo.
    recovery_id: Option<String>,
    /// Persistência em disco habilitada (desativada nos construtores de teste).
    persistence: bool,
    /// Explorador recebe as teclas de navegação.
    explorer_focused: bool,
    /// Próximo salvamento sobrescreve uma alteração externa já confirmada.
    overwrite_pending: bool,
    /// Tela inicial com projetos recentes.
    start_screen: Option<StartScreen>,
    /// Sequência usada para identificar documentos sem título.
    untitled_sequence: u64,
    running: bool,
    needs_redraw: bool,
    width: u16,
    height: u16,
    mode: Mode,
    prompt: Option<Prompt>,
    confirm: Option<ConfirmDialog>,
    pending_after_save: Option<PendingAction>,
    status: Option<StatusMessage>,
    session: Option<EditKind>,
    theme: ThemeState,
    syntax: Option<SyntaxState>,
}

impl App {
    /// Cria a aplicação com temas internos para testes determinísticos.
    #[cfg(test)]
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_text(width, height, "")
    }

    /// Cria a aplicação carregando temas externos e a seleção persistida.
    pub fn with_environment(width: u16, height: u16) -> Self {
        let theme = ThemeState::from_environment(ColorMode::detect());
        let mut app = Self::with_theme(width, height, "", theme);

        if let Some(paths) = AppPaths::from_environment() {
            app.paths = paths.clone();
            app.explorer = Explorer::new(paths.root.clone());
            app.explorer.set_visible(false);
            app.recovery_manager = RecoveryManager::new(paths.recovery_dir.clone());
            app.session_store = SessionStore::new(paths.session_file.clone());
            app.recent_store = RecentStore::new(paths.recent_file.clone());
            app.persistence = true;
        }

        if let Some(summary) = app.theme.warning_summary() {
            app.set_error(summary);
        }

        let warnings = app.restore_session();
        if let Some(first) = warnings.into_iter().next() {
            app.set_error(first);
        }

        app
    }

    /// Cria uma aplicação persistente com caminhos explícitos.
    ///
    /// Útil para integrações e testes que precisam isolar sessão, recuperação
    /// e projetos recentes do diretório real do usuário.
    #[allow(dead_code)]
    pub fn with_paths(width: u16, height: u16, paths: AppPaths) -> Self {
        let theme = ThemeState::builtin(ColorMode::TrueColor);
        let mut app = Self::with_theme(width, height, "", theme);
        app.explorer = Explorer::new(paths.root.clone());
        app.explorer.set_visible(false);
        app.recovery_manager = RecoveryManager::new(paths.recovery_dir.clone());
        app.session_store = SessionStore::new(paths.session_file.clone());
        app.recent_store = RecentStore::new(paths.recent_file.clone());
        app.paths = paths;
        app.persistence = true;

        let warnings = app.restore_session();
        if let Some(first) = warnings.into_iter().next() {
            app.set_error(first);
        }

        app.mark_rendered();
        app
    }

    /// Cria a aplicação com um documento inicial e temas internos.
    ///
    /// Usado pelos testes para manter execuções determinísticas; o conteúdo
    /// sempre pertence ao `Buffer` do Core.
    #[cfg(test)]
    pub fn with_text(width: u16, height: u16, text: &str) -> Self {
        Self::with_theme(
            width,
            height,
            text,
            ThemeState::builtin(ColorMode::TrueColor),
        )
    }

    /// Cria a aplicação com um documento já associado a um caminho.
    ///
    /// O texto é recebido já lido; a análise sintática é criada conforme a
    /// extensão do caminho.
    #[cfg(test)]
    pub fn with_document(width: u16, height: u16, path: &Path, text: &str) -> Self {
        let mut app = Self::with_text(width, height, text);
        app.path = Some(path.to_path_buf());
        app.document_name = document_name(path);
        app.syntax = LanguageId::from_path(path).map(|language| SyntaxState::new(language, text));
        app
    }

    fn with_theme(width: u16, height: u16, text: &str, theme: ThemeState) -> Self {
        let buffer = Buffer::from_text(text);
        let line_count = buffer.len_lines().unwrap_or(1);
        let (cols, rows) = editor_content_size(width, height, line_count);

        let mut view = View::new();
        view.set_content_size(cols, rows);

        let documents = DocumentManager::new();
        let paths = AppPaths::default();
        let mut explorer = Explorer::new(paths.root.clone());
        explorer.set_visible(false);
        let mut app = Self {
            documents,
            workspace: None,
            explorer,
            file_watcher: None,
            recovery_manager: RecoveryManager::new(paths.recovery_dir.clone()),
            session_store: SessionStore::new(paths.session_file.clone()),
            recent_store: RecentStore::new(paths.recent_file.clone()),
            paths,
            buffer,
            view,
            path: None,
            document_name: "sem título".to_owned(),
            line_ending: detect_line_ending(text).to_owned(),
            disk: None,
            recovered: false,
            missing: false,
            conflict: false,
            recovery_id: None,
            persistence: false,
            explorer_focused: false,
            overwrite_pending: false,
            start_screen: None,
            untitled_sequence: 0,
            running: true,
            needs_redraw: true,
            width,
            height,
            mode: Mode::Editor,
            prompt: None,
            confirm: None,
            pending_after_save: None,
            status: None,
            session: None,
            theme,
            syntax: None,
        };

        // Aplica o estado de sintaxe se o texto não estiver vazio.
        if !text.is_empty() {
            app.syntax = LanguageId::from_path(Path::new("text.txt"))
                .map(|language| SyntaxState::new(language, text));
        }

        app
    }

    /// Buffer do documento ativo. Os documentos inativos ficam no
    /// `DocumentManager` e só são materializados ao alternar abas.
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Estado de cursor/rolagem do documento ativo.
    pub fn view(&self) -> &View {
        &self.view
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn document_name(&self) -> &str {
        &self.document_name
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn needs_redraw(&self) -> bool {
        self.needs_redraw
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    pub fn confirm(&self) -> Option<&ConfirmDialog> {
        self.confirm.as_ref()
    }

    pub fn status_message(&self) -> Option<&StatusMessage> {
        self.status.as_ref()
    }

    pub fn theme(&self) -> &ThemeState {
        &self.theme
    }

    pub fn syntax(&self) -> Option<&SyntaxState> {
        self.syntax.as_ref()
    }

    /// Move o documento ativo para uma representação avulsa.
    ///
    /// O `DocumentManager` guarda apenas as abas inativas; esta conversão
    /// permite usar as operações dele sem duplicar a lógica de abas no `App`.
    fn take_active_document(&mut self) -> Document {
        Document {
            buffer: std::mem::take(&mut self.buffer),
            view: std::mem::take(&mut self.view),
            path: self.path.take(),
            name: std::mem::take(&mut self.document_name),
            line_ending: std::mem::take(&mut self.line_ending),
            syntax: self.syntax.take(),
            disk: self.disk.take(),
            recovered: self.recovered,
            missing: self.missing,
            conflict: self.conflict,
            recovery_id: self.recovery_id.take(),
        }
    }

    /// Instala o documento ativo de volta nos campos usados pela interface.
    fn install_active_document(&mut self, document: Document) {
        self.buffer = document.buffer;
        self.view = document.view;
        self.path = document.path;
        self.document_name = document.name;
        self.line_ending = document.line_ending;
        self.syntax = document.syntax;
        self.disk = document.disk;
        self.recovered = document.recovered;
        self.missing = document.missing;
        self.conflict = document.conflict;
        self.recovery_id = document.recovery_id;
    }

    /// Executa uma operação que precisa do documento ativo materializado.
    fn with_active_document<T>(
        &mut self,
        operation: impl FnOnce(&mut Document, &mut DocumentManager) -> T,
    ) -> T {
        let mut document = self.take_active_document();
        let result = operation(&mut document, &mut self.documents);
        self.install_active_document(document);
        result
    }

    /// Informações das abas na ordem conceitual, incluindo a ativa.
    pub fn tabs(&self) -> Vec<TabInfo> {
        let count = self.documents.len();
        let inactive = self.documents.inactive();

        (0..count)
            .map(|index| {
                if index == self.documents.active_index() {
                    TabInfo {
                        index,
                        name: self.document_name.clone(),
                        path: self.path.clone(),
                        modified: self.buffer.is_modified() || self.recovered,
                        active: true,
                    }
                } else {
                    let physical = if index < self.documents.active_index() {
                        index
                    } else {
                        index - 1
                    };
                    let document = &inactive[physical];
                    TabInfo {
                        index,
                        name: document.name.clone(),
                        path: document.path.clone(),
                        modified: document.is_modified() || document.recovered,
                        active: false,
                    }
                }
            })
            .collect()
    }

    pub fn tab_count(&self) -> usize {
        self.documents.len()
    }

    pub fn active_tab(&self) -> usize {
        self.documents.active_index()
    }

    pub fn workspace(&self) -> Option<&Workspace> {
        self.workspace.as_ref()
    }

    pub fn explorer(&self) -> &Explorer {
        &self.explorer
    }

    pub fn explorer_focused(&self) -> bool {
        self.explorer_focused
    }

    pub fn start_screen(&self) -> Option<&StartScreen> {
        self.start_screen.as_ref()
    }

    pub fn recent_projects(&self) -> RecentProjects {
        self.recent_store
            .load()
            .unwrap_or_else(|_| RecentProjects::default())
    }

    /// Nome da linguagem reconhecida, ou `Texto` quando não há highlighting.
    pub fn language_name(&self) -> &'static str {
        self.syntax
            .as_ref()
            .map(|syntax| syntax.language().display_name())
            .unwrap_or("Texto")
    }

    /// Documento ativo veio de uma cópia de recuperação.
    pub fn is_recovered(&self) -> bool {
        self.recovered
    }

    /// O arquivo ativo mudou no disco enquanto havia edições locais.
    pub fn has_conflict(&self) -> bool {
        self.conflict
    }

    /// O arquivo ativo desapareceu do disco.
    pub fn is_missing(&self) -> bool {
        self.missing
    }

    pub fn mark_rendered(&mut self) {
        self.needs_redraw = false;
    }

    /// Finaliza uma sessão de digitação quando o usuário fica inativo.
    ///
    /// O laço de eventos chama este método quando `poll` expira. Assim, uma
    /// pausa fecha a sessão sem exigir um timer próprio na aplicação.
    pub fn on_idle(&mut self) {
        if self.session.is_some() || self.buffer.has_active_transaction() {
            self.commit_session();
        }

        self.check_external_changes();
        self.persist_recovery();
    }

    /// Aplica um evento já recebido ao estado da aplicação.
    pub fn update(&mut self, event: Event) {
        match event {
            Event::Key(key) => {
                if self.start_screen.is_some() {
                    self.handle_start_key(key);
                    return;
                }

                if self.mode == Mode::Confirm {
                    if let Some(action) = crate::input::confirm_action(key) {
                        self.apply_confirm_action(action);
                    }
                    return;
                }

                if self.mode == Mode::Editor
                    && self.explorer_focused
                    && self.handle_explorer_key(key)
                {
                    return;
                }

                let action = match self.mode {
                    Mode::Prompt(_) => crate::input::prompt_action(key),
                    Mode::Editor | Mode::Help => crate::input::action(key),
                    Mode::Confirm => None,
                };

                if let Some(action) = action {
                    self.apply_action(action);
                }
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize { width, height } => {
                self.width = width;
                self.height = height;
                self.sync_viewport();
                if self.buffer.is_consistent() {
                    self.ensure_visible();
                }
                self.needs_redraw = true;
            }
        }
    }

    fn apply_action(&mut self, action: Action) {
        if action == Action::Quit {
            self.request_quit();
            return;
        }

        match self.mode {
            Mode::Prompt(_) => self.apply_prompt_action(action),
            Mode::Help => self.apply_help_action(action),
            Mode::Editor => self.apply_editor_action(action),
            Mode::Confirm => {}
        }
    }

    fn apply_help_action(&mut self, action: Action) {
        match action {
            Action::ToggleHelp | Action::CancelSelection => {
                self.mode = Mode::Editor;
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    fn apply_prompt_action(&mut self, action: Action) {
        match action {
            Action::PromptChar(character) => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.insert_char(character);
                    self.needs_redraw = true;
                }
            }
            Action::PromptBackspace => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.backspace();
                    self.needs_redraw = true;
                }
            }
            Action::PromptDelete => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.delete();
                    self.needs_redraw = true;
                }
            }
            Action::PromptLeft => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.move_left();
                    self.needs_redraw = true;
                }
            }
            Action::PromptRight => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.move_right();
                    self.needs_redraw = true;
                }
            }
            Action::PromptHome => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.move_home();
                    self.needs_redraw = true;
                }
            }
            Action::PromptEnd => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.move_end();
                    self.needs_redraw = true;
                }
            }
            Action::PromptConfirm => self.confirm_prompt(),
            Action::PromptCancel => self.cancel_prompt(),
            _ => {}
        }
    }

    fn apply_editor_action(&mut self, action: Action) {
        match action {
            Action::MoveLeft { select } => self.move_cursor(Motion::Left, select),
            Action::MoveRight { select } => self.move_cursor(Motion::Right, select),
            Action::MoveUp { select } => self.move_cursor(Motion::Up, select),
            Action::MoveDown { select } => self.move_cursor(Motion::Down, select),
            Action::MoveLineStart { select } => self.move_cursor(Motion::LineStart, select),
            Action::MoveLineEnd { select } => self.move_cursor(Motion::LineEnd, select),
            Action::MoveDocStart { select } => self.move_cursor(Motion::DocStart, select),
            Action::MoveDocEnd { select } => self.move_cursor(Motion::DocEnd, select),
            Action::MovePageUp { select } => self.move_cursor(Motion::PageUp, select),
            Action::MovePageDown { select } => self.move_cursor(Motion::PageDown, select),
            Action::InsertChar(character) => self.insert_char(character),
            Action::InsertTab => self.insert_text("\t", EditKind::Insert),
            Action::InsertNewline => {
                let newline = self.line_ending.clone();
                self.insert_text(&newline, EditKind::Insert);
            }
            Action::Backspace => self.backspace(),
            Action::DeleteForward => self.delete_forward(),
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::SelectAll => self.select_all(),
            Action::CancelSelection => {
                self.commit_session();
                self.view.clear_selection();
                self.needs_redraw = true;
            }
            Action::OpenPrompt => {
                if self.confirm.is_some() {
                    return;
                }
                self.commit_session();
                self.open_prompt(PromptKind::Open);
            }
            Action::Save => {
                if self.buffer.is_consistent() {
                    self.save();
                } else {
                    self.set_error("documento em recuperação pendente; salvar indisponível");
                }
            }
            Action::SaveAsPrompt => {
                self.commit_session();
                if self.buffer.is_consistent() {
                    self.open_prompt(PromptKind::SaveAs);
                } else {
                    self.set_error("documento em recuperação pendente; salvar indisponível");
                }
            }
            Action::ToggleHelp => {
                self.mode = Mode::Help;
                self.needs_redraw = true;
            }
            Action::ToggleExplorer => {
                let visible = self.explorer.toggle_visible();
                self.explorer_focused = visible;
                self.needs_redraw = true;
            }
            Action::GrowExplorer => {
                self.explorer.resize_width(4);
                self.needs_redraw = true;
            }
            Action::ShrinkExplorer => {
                self.explorer.resize_width(-4);
                self.needs_redraw = true;
            }
            Action::CloseTab => self.close_current_tab(),
            Action::NextTab => {
                if !self.next_tab() {
                    self.set_info("nenhuma outra aba aberta");
                }
            }
            Action::PreviousTab => {
                if !self.previous_tab() {
                    self.set_info("nenhuma outra aba aberta");
                }
            }
            Action::ReloadFromDisk => self.reload_active_from_disk(),
            Action::NewFile => self.open_explorer_prompt(PromptKind::NewFile),
            Action::NewDirectory => self.open_explorer_prompt(PromptKind::NewDirectory),
            Action::ToggleHidden => {
                let shown = self.explorer.toggle_hidden();
                self.needs_redraw = true;
                if shown {
                    self.set_info("arquivos ocultos exibidos");
                } else {
                    self.set_info("arquivos ocultos ocultados");
                }
            }
            Action::CloseAllTabs => {
                self.close_all_tabs();
            }
            Action::DeletePermanently => self.request_delete_selected(true),
            Action::Quit
            | Action::PromptChar(_)
            | Action::PromptBackspace
            | Action::PromptDelete
            | Action::PromptLeft
            | Action::PromptRight
            | Action::PromptHome
            | Action::PromptEnd
            | Action::PromptConfirm
            | Action::PromptCancel => {}
        }
    }

    fn move_cursor(&mut self, motion: Motion, select: bool) {
        self.commit_session();

        match self.view.move_cursor(&self.buffer, motion, select) {
            Ok(_) => self.needs_redraw = true,
            Err(error) => {
                self.report_buffer_error("não foi possível mover o cursor", &error);
            }
        }
    }

    fn insert_char(&mut self, character: char) {
        self.insert_text(&character.to_string(), EditKind::Insert);
    }

    fn insert_text(&mut self, text: &str, kind: EditKind) {
        if !self.buffer.is_consistent() {
            self.report_recovery();
            return;
        }

        if let Some(range) = self.view.selection_range() {
            self.commit_session();
            let inserted_len = text.chars().count();

            match self
                .buffer
                .apply_transaction(vec![Edit::new(range, text.to_owned())])
            {
                Ok(transaction) => {
                    if let Some(transaction) = &transaction {
                        self.apply_syntax_transaction(transaction);
                    }
                    self.view.clear_selection();
                    self.set_cursor_after_edit(CharOffset(range.start.0 + inserted_len));
                }
                Err(error) => {
                    self.report_buffer_error("não foi possível substituir a seleção", &error);
                }
            }

            return;
        }

        if !self.ensure_session(kind) {
            return;
        }

        let cursor = self.view.cursor();

        match self.buffer.insert(cursor, text) {
            Ok(Some(applied)) => {
                self.apply_syntax_edit(&applied);
                self.set_cursor_after_edit(CharOffset(cursor.0 + text.chars().count()));
            }
            Ok(None) => {}
            Err(error) => {
                self.report_buffer_error("não foi possível inserir texto", &error);
                self.commit_session();
            }
        }
    }

    fn backspace(&mut self) {
        if !self.buffer.is_consistent() {
            self.report_recovery();
            return;
        }

        if self.view.has_selection() {
            self.delete_selection();
            return;
        }

        let cursor = self.view.cursor();
        if cursor.0 == 0 {
            return;
        }

        let line = match self.view.cursor_line(&self.buffer) {
            Ok(line) => line,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar a linha do cursor", &error);
                return;
            }
        };

        let line_start = match self.buffer.line_start(line) {
            Ok(start) => start,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar o início da linha", &error);
                return;
            }
        };

        let range = if cursor > line_start {
            TextRange::new(CharOffset(cursor.0 - 1), cursor)
        } else {
            match self.buffer.line_end(line - 1) {
                Ok(previous_end) => TextRange::new(previous_end, cursor),
                Err(error) => {
                    self.report_buffer_error("não foi possível remover a quebra de linha", &error);
                    return;
                }
            }
        };

        if !self.ensure_session(EditKind::DeleteBackward) {
            return;
        }

        match self.buffer.delete(range) {
            Ok(Some(applied)) => {
                self.apply_syntax_edit(&applied);
                self.set_cursor_after_edit(range.start);
            }
            Ok(None) => {}
            Err(error) => {
                self.report_buffer_error("não foi possível apagar", &error);
                self.commit_session();
            }
        }
    }

    fn delete_forward(&mut self) {
        if !self.buffer.is_consistent() {
            self.report_recovery();
            return;
        }

        if self.view.has_selection() {
            self.delete_selection();
            return;
        }

        let cursor = self.view.cursor();
        let line = match self.view.cursor_line(&self.buffer) {
            Ok(line) => line,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar a linha do cursor", &error);
                return;
            }
        };

        let line_end = match self.buffer.line_end(line) {
            Ok(end) => end,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar o fim da linha", &error);
                return;
            }
        };

        let range = if cursor < line_end {
            TextRange::new(cursor, CharOffset(cursor.0 + 1))
        } else {
            let lines = match self.buffer.line_count() {
                Ok(lines) => lines,
                Err(error) => {
                    self.report_buffer_error("não foi possível contar as linhas", &error);
                    return;
                }
            };

            if line + 1 >= lines {
                return;
            }

            match self.buffer.line_start(line + 1) {
                Ok(next_start) => TextRange::new(cursor, next_start),
                Err(error) => {
                    self.report_buffer_error("não foi possível remover a quebra de linha", &error);
                    return;
                }
            }
        };

        if !self.ensure_session(EditKind::DeleteForward) {
            return;
        }

        match self.buffer.delete(range) {
            Ok(Some(applied)) => {
                self.apply_syntax_edit(&applied);
                self.set_cursor_after_edit(cursor);
            }
            Ok(None) => {}
            Err(error) => {
                self.report_buffer_error("não foi possível apagar", &error);
                self.commit_session();
            }
        }
    }

    fn delete_selection(&mut self) {
        let Some(range) = self.view.selection_range() else {
            return;
        };

        self.commit_session();

        match self
            .buffer
            .apply_transaction(vec![Edit::new(range, String::new())])
        {
            Ok(transaction) => {
                if let Some(transaction) = &transaction {
                    self.apply_syntax_transaction(transaction);
                }
                self.view.clear_selection();
                self.set_cursor_after_edit(range.start);
            }
            Err(error) => {
                self.report_buffer_error("não foi possível excluir a seleção", &error);
            }
        }
    }

    fn select_all(&mut self) {
        self.commit_session();

        match self.view.select_all(&self.buffer) {
            Ok(()) => self.needs_redraw = true,
            Err(error) => {
                self.report_buffer_error("não foi possível selecionar o documento", &error);
            }
        }
    }

    fn undo(&mut self) {
        self.commit_session();

        match self.buffer.undo() {
            Ok(true) => {
                self.after_history_change();
                self.set_info("desfeito");
            }
            Ok(false) => self.set_info("nada para desfazer"),
            Err(error) => self.report_buffer_error("não foi possível desfazer", &error),
        }
    }

    fn redo(&mut self) {
        self.commit_session();

        match self.buffer.redo() {
            Ok(true) => {
                self.after_history_change();
                self.set_info("refeito");
            }
            Ok(false) => self.set_info("nada para refazer"),
            Err(error) => self.report_buffer_error("não foi possível refazer", &error),
        }
    }

    fn after_history_change(&mut self) {
        self.view.clear_selection();
        self.sync_viewport();
        let cursor = self.view.cursor();

        if let Err(error) = self.view.set_cursor(&self.buffer, cursor) {
            self.report_buffer_error("não foi possível ajustar o cursor após o histórico", &error);
        }

        self.resync_syntax();
        self.needs_redraw = true;
    }

    fn apply_syntax_edit(&mut self, edit: &AppliedEdit) {
        if self.syntax.is_none() {
            return;
        }

        let result = self
            .syntax
            .as_mut()
            .map(|syntax| syntax.apply_edit(edit.range, &edit.deleted_text, &edit.inserted_text));

        if let Some(Err(error)) = result {
            self.disable_syntax(error);
        }
    }

    fn apply_syntax_transaction(&mut self, transaction: &Transaction) {
        for edit in transaction.edits() {
            if self.syntax.is_none() {
                return;
            }

            let result = self.syntax.as_mut().map(|syntax| {
                syntax.apply_edit(edit.range, &edit.deleted_text, &edit.inserted_text)
            });

            if let Some(Err(error)) = result {
                self.disable_syntax(error);
                return;
            }
        }
    }

    /// Recalcula a menor edição entre o texto atual e o estado sintático.
    ///
    /// Usado em Undo/Redo, quando o Core não expõe as edições aplicadas.
    fn resync_syntax(&mut self) {
        if self.syntax.is_none() {
            return;
        }

        let text = match self.buffer.text() {
            Ok(text) => text,
            Err(_) => {
                self.syntax = None;
                return;
            }
        };

        let result = self.syntax.as_mut().map(|syntax| syntax.resync(&text));

        if let Some(Err(error)) = result {
            self.disable_syntax(error);
        }
    }

    fn disable_syntax(&mut self, error: nexus_syntax::SyntaxError) {
        self.syntax = None;
        self.set_error(format!("highlighting desativado: {error}"));
    }

    /// Ajusta a linguagem depois de Salvar Como.
    ///
    /// Se a extensão não mudou, a árvore existente é preservada. Caso
    /// contrário, o estado é recriado a partir do texto já salvo.
    fn set_syntax_for_path(&mut self, path: &Path, text: &str) {
        let language = LanguageId::from_path(path);

        match (&self.syntax, language) {
            (Some(state), Some(language)) if state.language() == language => {}
            (None, None) => {}
            _ => {
                self.syntax = language.map(|language| SyntaxState::new(language, text));
            }
        }
    }

    fn ensure_session(&mut self, kind: EditKind) -> bool {
        if self.session == Some(kind) && self.buffer.has_active_transaction() {
            return true;
        }

        self.commit_session();

        if !self.buffer.is_consistent() {
            self.report_recovery();
            return false;
        }

        match self.buffer.begin_transaction() {
            Ok(()) => {
                self.session = Some(kind);
                true
            }
            Err(error) => {
                self.report_buffer_error("não foi possível iniciar a sessão de edição", &error);
                false
            }
        }
    }

    fn commit_session(&mut self) {
        let had_session = self.session.is_some() || self.buffer.has_active_transaction();
        self.session = None;

        if !self.buffer.has_active_transaction() {
            return;
        }

        match self.buffer.commit_transaction() {
            Ok(_) => {
                if had_session {
                    self.needs_redraw = true;
                }
            }
            Err(error) => {
                self.report_buffer_error("não foi possível concluir a sessão de edição", &error);
            }
        }
    }

    #[allow(dead_code)]
    fn cancel_session(&mut self) {
        self.session = None;

        if !self.buffer.has_active_transaction() {
            return;
        }

        match self.buffer.cancel_transaction() {
            Ok(()) => self.resync_syntax(),
            Err(error) => {
                self.report_buffer_error("não foi possível cancelar a sessão de edição", &error);
            }
        }
    }

    fn set_cursor_after_edit(&mut self, offset: CharOffset) {
        self.view.clear_selection();
        self.sync_viewport();

        if let Err(error) = self.view.set_cursor(&self.buffer, offset) {
            self.report_buffer_error("não foi possível atualizar o cursor", &error);
        }

        self.needs_redraw = true;
    }

    fn sync_viewport(&mut self) {
        let line_count = if self.buffer.is_consistent() {
            self.buffer.line_count().unwrap_or(1)
        } else {
            1
        };

        let (cols, rows) = editor_content_size(self.width, self.height, line_count);
        self.view.set_content_size(cols, rows);
    }

    fn ensure_visible(&mut self) {
        if let Err(error) = self.view.ensure_visible(&self.buffer) {
            self.report_buffer_error("não foi possível ajustar o viewport", &error);
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        let initial = match kind {
            PromptKind::Open => String::new(),
            PromptKind::SaveAs => self
                .path()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            PromptKind::NewFile | PromptKind::NewDirectory => String::new(),
            PromptKind::Rename => self
                .explorer
                .selected_entry()
                .map(|entry| entry.name.clone())
                .unwrap_or_default(),
        };

        self.prompt = Some(Prompt::new(kind, initial));
        self.mode = Mode::Prompt(kind);
        self.needs_redraw = true;
    }

    /// Abre o prompt de uma operação do explorador com o alvo correto.
    fn open_explorer_prompt(&mut self, kind: PromptKind) {
        if self.workspace.is_none() {
            self.set_error("nenhum Workspace aberto");
            return;
        }

        let Some(entry) = self.explorer.selected_entry().cloned() else {
            self.set_error("selecione um item no explorador");
            return;
        };

        let (input, target) = match kind {
            PromptKind::Rename => (entry.name.clone(), entry.path.clone()),
            PromptKind::NewFile | PromptKind::NewDirectory => {
                let parent = if entry.is_dir {
                    entry.path.clone()
                } else {
                    entry
                        .path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.explorer.root().to_path_buf())
                };
                (String::new(), parent)
            }
            PromptKind::Open | PromptKind::SaveAs => return self.open_prompt(kind),
        };

        self.prompt = Some(Prompt::with_target(kind, input, target));
        self.mode = Mode::Prompt(kind);
        self.needs_redraw = true;
    }

    fn confirm_prompt(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            self.mode = Mode::Editor;
            return;
        };

        let kind = prompt.kind;
        match kind {
            PromptKind::Open => {
                if prompt.input().is_empty() {
                    self.prompt = Some(prompt);
                    self.set_error(FileError::EmptyPath.to_string());
                    return;
                }

                let path = PathBuf::from(prompt.input());

                if self.buffer.is_modified() || !self.buffer.is_consistent() {
                    self.open_confirm(PendingAction::Open(path));
                    return;
                }

                match self.open_document_in_new_tab(&path) {
                    Ok(()) => self.mode = Mode::Editor,
                    Err(error) => {
                        self.prompt = Some(prompt);
                        self.set_error(error.to_string());
                    }
                }
            }
            PromptKind::SaveAs => match self.save_document_to(Path::new(prompt.input())) {
                Ok(()) => {
                    self.mode = Mode::Editor;
                    if let Some(pending) = self.pending_after_save.take() {
                        self.complete_pending(pending);
                    }
                }
                Err(error) => {
                    self.prompt = Some(prompt);
                    self.set_error(error.to_string());
                }
            },
            PromptKind::NewFile | PromptKind::NewDirectory | PromptKind::Rename => {
                self.confirm_explorer_prompt(prompt, kind);
            }
        }
    }

    fn confirm_explorer_prompt(&mut self, prompt: Prompt, kind: PromptKind) {
        let Some(target) = prompt.target().map(Path::to_path_buf) else {
            self.mode = Mode::Editor;
            self.set_error("operação do explorador sem alvo");
            return;
        };

        if prompt.input().trim().is_empty() {
            self.prompt = Some(prompt);
            self.set_error("o nome não pode ficar vazio");
            return;
        }

        let result = match kind {
            PromptKind::NewFile => self
                .explorer
                .create_file(&target, prompt.input())
                .map(|created| (created, "arquivo criado")),
            PromptKind::NewDirectory => self
                .explorer
                .create_dir(&target, prompt.input())
                .map(|created| (created, "pasta criada")),
            PromptKind::Rename => self
                .explorer
                .rename(&target, prompt.input())
                .map(|renamed| (renamed, "item renomeado")),
            PromptKind::Open | PromptKind::SaveAs => unreachable!(),
        };

        match result {
            Ok((path, message)) => {
                if kind == PromptKind::Rename {
                    self.remap_open_paths(&target, &path);
                }
                self.prompt = None;
                self.mode = Mode::Editor;
                self.explorer_focused = true;
                self.explorer.reveal(&path);
                self.set_info(message);
            }
            Err(error) => {
                self.prompt = Some(prompt);
                self.set_error(error.to_string());
            }
        }
    }

    fn cancel_prompt(&mut self) {
        self.prompt = None;
        self.mode = Mode::Editor;
        self.needs_redraw = true;

        match self.pending_after_save.take() {
            Some(PendingAction::Quit) => self.set_info("saída cancelada"),
            Some(PendingAction::Open(_)) => self.set_info("abertura cancelada"),
            Some(PendingAction::CloseTab) => self.set_info("fechamento de aba cancelado"),
            Some(PendingAction::Delete { .. }) => self.set_info("exclusão cancelada"),
            None => {}
        }
    }

    /// Abre um arquivo, substituindo o documento atual apenas se a leitura
    /// funcionar. Quando o arquivo já está aberto, a aba existente é ativada
    /// em vez de criar uma duplicata.
    fn open_document(&mut self, path: &Path) -> Result<(), crate::document::DocumentError> {
        let active = self.take_active_document();
        let existing = self.documents.index_of_path(&active, path);
        let already_active = existing == Some(self.documents.active_index());
        self.install_active_document(active);

        if already_active {
            return Ok(());
        }

        if let Some(index) = existing
            && self.switch_tab(index)
        {
            self.set_info("arquivo já aberto");
            return Ok(());
        }

        let document = Document::from_file(path)?;
        self.commit_session();
        self.install_active_document(document);
        self.recovery_id = self.path.as_deref().map(RecoveryManager::id_for_path);
        self.conflict = false;
        self.overwrite_pending = false;
        self.view.reset();
        self.sync_viewport();
        self.ensure_visible();
        self.refresh_watcher();
        self.set_info("arquivo aberto");

        Ok(())
    }

    /// Abre um arquivo em uma nova aba, evitando duplicatas.
    ///
    /// Um documento inicial vazio e sem alterações é reaproveitado para não
    /// deixar uma aba "sem título" inútil ao abrir o primeiro arquivo.
    pub(crate) fn open_document_in_new_tab(
        &mut self,
        path: &Path,
    ) -> Result<(), crate::document::DocumentError> {
        if self.is_pristine_untitled() {
            return self.open_document(path);
        }

        let active = self.take_active_document();
        let existing = self.documents.index_of_path(&active, path);
        self.install_active_document(active);

        if let Some(index) = existing {
            if index != self.documents.active_index() {
                self.switch_tab(index);
            }
            self.set_info("arquivo já aberto");
            return Ok(());
        }

        let document = Document::from_file(path)?;
        self.commit_session();

        let mut active = self.take_active_document();
        self.documents.add_after_active(&mut active, document);
        self.install_active_document(active);
        self.recovery_id = self.path.as_deref().map(RecoveryManager::id_for_path);
        self.conflict = false;
        self.overwrite_pending = false;
        self.sync_viewport();
        self.ensure_visible();
        self.refresh_watcher();
        self.set_info("arquivo aberto");

        Ok(())
    }

    fn is_pristine_untitled(&self) -> bool {
        self.path.is_none()
            && !self.recovered
            && !self.buffer.is_modified()
            && self.documents.len() == 1
            && self.buffer.is_empty().unwrap_or(false)
    }

    /// Salva o documento no caminho informado.
    ///
    /// A sessão de edição é concluída antes da leitura do texto. O estado de
    /// salvamento só é atualizado depois que a escrita é confirmada.
    fn save_document_to(&mut self, path: &Path) -> Result<(), DocumentError> {
        self.commit_session();

        let text = self.buffer.text()?;
        files::write_document(path, &text)?;
        self.buffer.mark_saved()?;

        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        self.set_syntax_for_path(&canonical, &text);
        self.path = Some(canonical.clone());
        self.document_name = document_name(&canonical);
        self.disk = fingerprint(&canonical).ok().flatten();
        self.missing = false;
        self.conflict = false;
        self.overwrite_pending = false;
        self.recovered = false;

        if self.recovery_id.is_none() {
            self.recovery_id = Some(RecoveryManager::id_for_path(&canonical));
        }

        // Uma gravação confirmada torna a cópia de recuperação obsoleta.
        if let Some(id) = self.recovery_id.as_deref() {
            let _ = self.recovery_manager.remove(id);
        }

        self.refresh_watcher();
        self.set_info("arquivo salvo");

        Ok(())
    }

    fn save(&mut self) {
        let Some(path) = self.path().map(Path::to_path_buf) else {
            self.open_prompt(PromptKind::SaveAs);
            return;
        };

        // Uma alteração externa nunca é sobrescrita sem uma segunda confirmação
        // explícita do usuário. O primeiro Ctrl+S apenas avisa.
        if self.conflict && !self.overwrite_pending {
            self.overwrite_pending = true;
            self.set_error(
                "arquivo alterado externamente; Ctrl+S novamente mantém a versão local ou Ctrl+R recarrega do disco",
            );
            return;
        }

        if let Err(error) = self.save_document_to(&path) {
            self.set_error(error.to_string());
        }
    }

    fn refresh_watcher(&mut self) {
        self.file_watcher = self.path.as_ref().map(|path| FileWatcher {
            path: path.clone(),
            fingerprint: self.disk,
        });
    }

    /// Ativa a aba no índice conceitual.
    pub fn switch_tab(&mut self, target: usize) -> bool {
        if target == self.documents.active_index() || target >= self.documents.len() {
            return false;
        }

        self.commit_session();
        let switched =
            self.with_active_document(|document, manager| manager.switch_to(document, target));

        if switched {
            self.overwrite_pending = false;
            if self.recovery_id.is_none() {
                self.recovery_id = self.path.as_deref().map(RecoveryManager::id_for_path);
            }
            self.sync_viewport();
            self.ensure_visible();
            self.refresh_watcher();
            self.needs_redraw = true;
        }

        switched
    }

    pub fn next_tab(&mut self) -> bool {
        if self.documents.len() <= 1 {
            return false;
        }
        let target = (self.documents.active_index() + 1) % self.documents.len();
        self.switch_tab(target)
    }

    pub fn previous_tab(&mut self) -> bool {
        if self.documents.len() <= 1 {
            return false;
        }
        let current = self.documents.active_index();
        let target = if current == 0 {
            self.documents.len() - 1
        } else {
            current - 1
        };
        self.switch_tab(target)
    }

    /// Fecha a aba ativa, protegendo alterações não salvas.
    pub fn close_current_tab(&mut self) {
        self.commit_session();

        if self.buffer.is_modified() || self.recovered {
            self.open_confirm(PendingAction::CloseTab);
            return;
        }

        self.close_tab_now(self.documents.active_index());
    }

    /// Fecha uma aba pelo índice conceitual.
    #[allow(dead_code)]
    pub fn close_tab(&mut self, index: usize) {
        if index >= self.documents.len() {
            return;
        }

        if index == self.documents.active_index() {
            self.close_current_tab();
            return;
        }

        let physical = if index < self.documents.active_index() {
            index
        } else {
            index - 1
        };
        let modified = self
            .documents
            .inactive()
            .get(physical)
            .is_some_and(|document| document.is_modified() || document.recovered);

        if modified {
            // Ativa a aba para que o diálogo padrão permita salvar/descartar.
            self.switch_tab(index);
            self.close_current_tab();
        } else if self.documents.close_inactive(index) {
            self.needs_redraw = true;
            self.set_info("aba fechada");
        }
    }

    fn close_tab_now(&mut self, index: usize) {
        if index == self.documents.active_index() {
            let last =
                self.with_active_document(|document, manager| manager.close_active(document));

            if !last {
                // O manager já instalou um documento vazio.
                self.untitled_sequence += 1;
                self.recovery_id = Some(RecoveryManager::new_untitled_id(self.untitled_sequence));
            } else if self.recovery_id.is_none() {
                self.recovery_id = self.path.as_deref().map(RecoveryManager::id_for_path);
            }

            self.overwrite_pending = false;
            self.sync_viewport();
            self.ensure_visible();
            self.refresh_watcher();
            self.needs_redraw = true;
            self.set_info("aba fechada");
        } else if self.documents.close_inactive(index) {
            self.needs_redraw = true;
            self.set_info("aba fechada");
        }
    }

    /// Fecha todas as abas somente quando nenhuma possui alterações pendentes.
    pub fn close_all_tabs(&mut self) -> bool {
        self.commit_session();

        let any_modified = self.buffer.is_modified()
            || self.recovered
            || self
                .documents
                .inactive()
                .iter()
                .any(|document| document.is_modified() || document.recovered);

        if any_modified {
            self.set_error(
                "há documentos com alterações não salvas; salve ou descarte antes de fechar tudo",
            );
            return false;
        }

        self.with_active_document(|document, manager| manager.reset(document));
        self.untitled_sequence += 1;
        self.recovery_id = Some(RecoveryManager::new_untitled_id(self.untitled_sequence));
        self.conflict = false;
        self.overwrite_pending = false;
        self.sync_viewport();
        self.ensure_visible();
        self.refresh_watcher();
        self.needs_redraw = true;
        self.set_info("todas as abas foram fechadas");
        true
    }

    /// Abre um diretório como Workspace e atualiza explorador e recentes.
    pub fn open_workspace(&mut self, path: &Path) -> Result<(), crate::workspace::WorkspaceError> {
        let workspace = Workspace::open(path)?;
        self.explorer.set_root(workspace.root());
        self.explorer.set_visible(true);
        self.workspace = Some(workspace);
        self.explorer_focused = true;
        self.record_recent_workspace();
        self.set_info("Workspace aberto");
        Ok(())
    }

    /// Exibe a tela inicial com os projetos recentes.
    pub fn show_start_screen(&mut self) {
        let projects = self.recent_projects();
        self.start_screen = Some(StartScreen {
            items: projects.entries,
            selected: 0,
        });
        self.needs_redraw = true;
    }

    fn record_recent_workspace(&mut self) {
        if !self.persistence {
            return;
        }
        let Some(workspace) = &self.workspace else {
            return;
        };

        let mut projects = self.recent_projects();
        projects.record(workspace.name(), workspace.root().to_path_buf());
        if let Err(error) = self.recent_store.save(&projects) {
            self.set_error(error.to_string());
        }
    }

    fn handle_start_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode;

        let Some(start) = self.start_screen.as_mut() else {
            return;
        };

        match key.code {
            KeyCode::Up => {
                start.move_selection(-1);
                self.needs_redraw = true;
            }
            KeyCode::Down => {
                start.move_selection(1);
                self.needs_redraw = true;
            }
            KeyCode::Enter => {
                let path = start.selected_entry().map(|entry| entry.path.clone());
                self.start_screen = None;
                match path {
                    Some(path) => {
                        if let Err(error) = self.open_workspace(&path) {
                            self.set_error(error.to_string());
                        }
                    }
                    None => self.set_info("digite ou use Ctrl+O para abrir um arquivo"),
                }
                self.needs_redraw = true;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.start_screen = None;
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    /// Navegação do explorador quando ele está em foco.
    ///
    /// Retorna `false` para teclas que devem continuar no editor ou nos
    /// atalhos globais (por exemplo, `Ctrl+S` e digitação normal).
    fn handle_explorer_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

        if key.kind == KeyEventKind::Release || key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }

        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Up => self.explorer.move_selection(-1),
            KeyCode::Down => self.explorer.move_selection(1),
            KeyCode::PageUp => self.explorer.move_selection(-10),
            KeyCode::PageDown => self.explorer.move_selection(10),
            KeyCode::Home => self.explorer.select(0),
            KeyCode::End => {
                let last = self.explorer.entries().len().saturating_sub(1);
                self.explorer.select(last);
            }
            KeyCode::Left => self.explorer.collapse_selected(),
            KeyCode::Right => self.explorer.expand_selected(),
            KeyCode::Enter => match self.explorer.activate_selected() {
                Some(ExplorerActivation::OpenFile(path)) => {
                    match self.open_document_in_new_tab(&path) {
                        Ok(()) => self.explorer_focused = false,
                        Err(error) => self.set_error(error.to_string()),
                    }
                }
                Some(ExplorerActivation::ToggleDirectory(_)) => {}
                None => self.set_error("nenhum item selecionado no explorador"),
            },
            KeyCode::Delete => {
                self.request_delete_selected(shift);
                return true;
            }
            KeyCode::F(2) => {
                self.open_explorer_prompt(PromptKind::Rename);
                return true;
            }
            KeyCode::Esc => {
                self.explorer_focused = false;
                self.set_info("foco no editor");
                return true;
            }
            _ => return false,
        }

        self.needs_redraw = true;
        true
    }

    /// Largura da barra lateral quando ela cabe no layout atual.
    pub fn explorer_visible_width(&self) -> Option<u16> {
        if !self.explorer.visible() || self.workspace.is_none() || self.width < 40 {
            return None;
        }

        let maximum = (self.width / 2).max(1);
        Some(self.explorer.width().min(maximum).max(1))
    }

    /// Altura do cabeçalho: duas linhas quando há abas visíveis.
    pub fn header_height(&self) -> u16 {
        if self.tab_count() > 1 { 2 } else { 1 }
    }

    /// Trata clique e rolagem do mouse sem depender de posições mágicas da UI.
    fn handle_mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        if self.mode != Mode::Editor || self.start_screen.is_some() {
            return;
        }

        let header = self.header_height();
        let body_top = header;
        let body_bottom = self.height.saturating_sub(1);
        let in_body = mouse.row >= body_top && mouse.row < body_bottom;
        let sidebar = self.explorer_visible_width();
        let over_explorer = sidebar.is_some_and(|width| mouse.column < width) && in_body;

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let forward = mouse.kind == MouseEventKind::ScrollDown;
                if over_explorer {
                    self.explorer.move_selection(if forward { 3 } else { -3 });
                    self.explorer_focused = true;
                } else {
                    let motion = if forward { Motion::Down } else { Motion::Up };
                    for _ in 0..3 {
                        self.move_cursor(motion, false);
                    }
                }
                self.needs_redraw = true;
            }
            MouseEventKind::Down(MouseButton::Left) if over_explorer => {
                let width = sidebar.unwrap_or(0);
                let offset = self.explorer.offset();
                let local = mouse.row.saturating_sub(body_top) as usize;
                let index = offset + local;
                if index < self.explorer.entries().len() {
                    self.explorer.select(index);
                    match self.explorer.activate_selected() {
                        Some(ExplorerActivation::OpenFile(path)) => {
                            match self.open_document_in_new_tab(&path) {
                                Ok(()) => self.explorer_focused = false,
                                Err(error) => self.set_error(error.to_string()),
                            }
                        }
                        Some(ExplorerActivation::ToggleDirectory(_)) => {
                            self.explorer_focused = true;
                        }
                        None => {}
                    }
                    if width == 0 {
                        self.explorer_focused = false;
                    }
                }
                self.needs_redraw = true;
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.tab_count() > 1 && mouse.row + 1 == header =>
            {
                let count = self.tab_count() as u16;
                let tab_width = (self.width / count).max(1);
                let target = (mouse.column / tab_width) as usize;
                if target < self.tab_count() {
                    self.switch_tab(target);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if sidebar.is_some() => {
                // Arrastar a borda da sidebar redimensiona o explorador.
                let width = mouse.column.saturating_add(1);
                self.explorer.set_width(width.min(self.width / 2));
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    /// Abas da interface em ordem; usado também pelo teste de integração.
    #[allow(dead_code)]
    pub fn tab_names(&self) -> Vec<String> {
        self.tabs().into_iter().map(|tab| tab.name).collect()
    }

    fn any_modified_under(&self, path: &Path) -> bool {
        let active = self
            .path
            .as_deref()
            .is_some_and(|candidate| path_within(candidate, path))
            && (self.buffer.is_modified() || self.recovered);

        active
            || self.documents.inactive().iter().any(|document| {
                document
                    .path
                    .as_deref()
                    .is_some_and(|candidate| path_within(candidate, path))
                    && (document.is_modified() || document.recovered)
            })
    }

    fn request_delete_selected(&mut self, permanent: bool) {
        if self.workspace.is_none() {
            self.set_error("nenhum Workspace aberto");
            return;
        }

        let Some(entry) = self.explorer.selected_entry().cloned() else {
            self.set_error("selecione um item no explorador");
            return;
        };

        if self.any_modified_under(&entry.path) {
            self.set_error(
                "há documentos modificados abertos sob este caminho; salve-os antes de excluir",
            );
            return;
        }

        self.open_confirm(PendingAction::Delete {
            path: entry.path,
            permanent,
        });
    }

    fn perform_delete(&mut self, path: &Path, permanent: bool) {
        if self.any_modified_under(path) {
            self.set_error(
                "há documentos modificados abertos sob este caminho; salve-os antes de excluir",
            );
            return;
        }

        match self.explorer.delete(path, permanent) {
            Ok(()) => {
                let removed = self.with_active_document(|document, manager| {
                    manager.remove_paths_under(document, path)
                });
                if !removed.is_empty() {
                    self.overwrite_pending = false;
                    self.sync_viewport();
                    self.ensure_visible();
                    self.refresh_watcher();
                }
                self.set_info(if permanent {
                    "item excluído permanentemente"
                } else {
                    "item movido para a lixeira"
                });
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    fn remap_open_paths(&mut self, old: &Path, new: &Path) {
        let old = canonical_for_compare(old);
        let new = canonical_for_compare(new);
        let remap = |path: &Path,
                     name: &mut String,
                     syntax: &mut Option<SyntaxState>,
                     disk: &mut Option<FileFingerprint>,
                     recovery_id: &mut Option<String>,
                     text: &str| {
            let candidate = canonical_for_compare(path);
            if candidate.starts_with(&old) {
                let suffix = candidate.strip_prefix(&old).unwrap_or(Path::new(""));
                let updated = new.join(suffix);
                *name = document_name(&updated);
                *syntax = LanguageId::from_path(&updated)
                    .map(|language| SyntaxState::new(language, text));
                *disk = fingerprint(&updated).ok().flatten();
                *recovery_id = Some(RecoveryManager::id_for_path(&updated));
                Some(updated)
            } else {
                None
            }
        };

        let text = self.buffer.text().unwrap_or_default();
        if let Some(path) = self.path.clone()
            && let Some(updated) = remap(
                &path,
                &mut self.document_name,
                &mut self.syntax,
                &mut self.disk,
                &mut self.recovery_id,
                &text,
            )
        {
            self.path = Some(updated);
        }

        for document in self.documents.inactive_mut() {
            let text = document.buffer.text().unwrap_or_default();
            if let Some(path) = document.path.clone()
                && let Some(updated) = remap(
                    &path,
                    &mut document.name,
                    &mut document.syntax,
                    &mut document.disk,
                    &mut document.recovery_id,
                    &text,
                )
            {
                document.path = Some(updated);
            }
        }

        self.refresh_watcher();
    }

    fn report_message(&mut self, kind: MessageKind, text: String) {
        match kind {
            MessageKind::Info => self.set_info(text),
            MessageKind::Error => self.set_error(text),
        }
    }

    /// Indica se a tela inicial deve cobrir um editor vazio sem sessão.
    pub fn should_show_start_screen(&self) -> bool {
        self.workspace.is_none() && self.is_pristine_untitled()
    }

    /// Expõe erros de inicialização (CLI) sem abrir um prompt artificial.
    pub fn report_error(&mut self, text: impl Into<String>) {
        self.set_error(text);
    }

    /// Grava cópias de recuperação para todas as abas modificadas.
    fn persist_recovery(&mut self) {
        if !self.persistence {
            return;
        }

        self.commit_session();
        let mut records = Vec::new();

        if (self.buffer.is_modified() || self.recovered)
            && let Ok(text) = self.buffer.text()
        {
            if self.recovery_id.is_none() {
                self.recovery_id = Some(match self.path.as_deref() {
                    Some(path) => RecoveryManager::id_for_path(path),
                    None => {
                        self.untitled_sequence += 1;
                        RecoveryManager::new_untitled_id(self.untitled_sequence)
                    }
                });
            }

            if let Some(id) = self.recovery_id.clone() {
                records.push(RecoveryRecord {
                    version: RECOVERY_VERSION,
                    id,
                    path: self.path.clone(),
                    name: self.document_name.clone(),
                    text,
                    saved_at: now_seconds(),
                });
            }
        }

        for document in self.documents.inactive_mut() {
            if !document.is_modified() && !document.recovered {
                continue;
            }

            let Ok(text) = document.buffer.text() else {
                continue;
            };

            if document.recovery_id.is_none() {
                document.recovery_id = Some(match document.path.as_deref() {
                    Some(path) => RecoveryManager::id_for_path(path),
                    None => {
                        self.untitled_sequence += 1;
                        RecoveryManager::new_untitled_id(self.untitled_sequence)
                    }
                });
            }

            if let Some(id) = document.recovery_id.clone() {
                records.push(RecoveryRecord {
                    version: RECOVERY_VERSION,
                    id,
                    path: document.path.clone(),
                    name: document.name.clone(),
                    text,
                    saved_at: now_seconds(),
                });
            }
        }

        for record in records {
            if let Err(error) = self.recovery_manager.save(&record) {
                self.set_error(error.to_string());
                break;
            }
        }
    }

    /// Persiste a ordem das abas, cursores, rolagem e estado do explorador.
    fn persist_session(&mut self) {
        if !self.persistence {
            return;
        }

        self.commit_session();

        let active_index = self.documents.active_index();
        let count = self.documents.len();
        let inactive = self.documents.inactive();
        let mut documents = Vec::with_capacity(count);
        let mut physical = 0;

        for conceptual in 0..count {
            if conceptual == active_index {
                documents.push(session_document(self.path.clone(), &self.view));
            } else {
                if let Some(document) = inactive.get(physical) {
                    documents.push(session_document(document.path.clone(), &document.view));
                }
                physical += 1;
            }
        }

        let mut session = Session::default();
        session.workspace = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf());
        session.documents = documents;
        session.active = active_index;
        session.explorer = self.explorer.session();

        if let Err(error) = self.session_store.save(&session) {
            self.set_error(error.to_string());
        }
    }

    /// Salva recuperação/sessão e remove cópias obsoletas antes de encerrar.
    fn persist_before_quit(&mut self) {
        if !self.persistence {
            return;
        }

        self.persist_recovery();

        if !self.buffer.is_modified() && !self.recovered {
            if let Some(id) = self.recovery_id.clone() {
                let _ = self.recovery_manager.remove(&id);
            }
            self.recovery_id = None;
        }

        for document in self.documents.inactive_mut() {
            if !document.is_modified() && !document.recovered {
                if let Some(id) = document.recovery_id.clone() {
                    let _ = self.recovery_manager.remove(&id);
                }
                document.recovery_id = None;
            }
        }

        self.persist_session();
    }

    /// Restaura sessão, documentos e cópias de recuperação.
    ///
    /// Cópias de recuperação que não pertencem a nenhuma aba da sessão são
    /// abertas como documentos não salvos, evitando perda de trabalho.
    fn restore_session(&mut self) -> Vec<String> {
        if !self.persistence {
            return Vec::new();
        }

        let session = match self.session_store.load() {
            Ok(Some(session)) => session,
            Ok(None) => return Vec::new(),
            Err(error) => return vec![error.to_string()],
        };

        let mut warnings = Vec::new();

        if let Some(root) = session.workspace.as_deref() {
            match Workspace::open(root) {
                Ok(workspace) => {
                    self.explorer.set_root(workspace.root());
                    self.workspace = Some(workspace);
                }
                Err(error) => warnings.push(error.to_string()),
            }
        }

        self.explorer.restore_session(&session.explorer);

        let records = self.recovery_manager.load_all();
        let mut used = vec![false; records.len()];
        let mut documents = Vec::new();

        for snapshot in &session.documents {
            let record_index = snapshot.path.as_deref().and_then(|path| {
                let id = RecoveryManager::id_for_path(path);
                records.iter().position(|record| record.id == id)
            });

            let document = match &snapshot.path {
                Some(path) => match Document::from_file(path) {
                    Ok(mut document) => {
                        if let Some(index) = record_index
                            && let Some(record) = records.get(index)
                        {
                            document.install_recovered(&record.text);
                            document.recovery_id = Some(record.id.clone());
                            used[index] = true;
                        }
                        Some(document)
                    }
                    Err(_) => match record_index
                        .and_then(|index| records.get(index).map(|r| (index, r)))
                    {
                        Some((index, record)) => {
                            let mut document = Document::from_unsaved_text(
                                record.name.clone(),
                                Some(path.clone()),
                                &record.text,
                            );
                            document.recovery_id = Some(record.id.clone());
                            used[index] = true;
                            Some(document)
                        }
                        None => {
                            warnings
                                .push(format!("não foi possível restaurar `{}`", path.display()));
                            None
                        }
                    },
                },
                None => {
                    let untitled = records
                        .iter()
                        .enumerate()
                        .find(|(index, record)| !used[*index] && record.path.is_none());
                    match untitled {
                        Some((index, record)) => {
                            let mut document = Document::from_unsaved_text(
                                record.name.clone(),
                                None,
                                &record.text,
                            );
                            document.recovery_id = Some(record.id.clone());
                            used[index] = true;
                            Some(document)
                        }
                        None => Some(Document::empty()),
                    }
                }
            };

            if let Some(mut document) = document {
                document.restore_view_state(
                    CharOffset(snapshot.cursor),
                    snapshot.anchor.map(CharOffset),
                    snapshot.top_line,
                    snapshot.left_col,
                );
                documents.push(document);
            }
        }

        for (index, record) in records.iter().enumerate() {
            if used[index] {
                continue;
            }

            let mut document =
                Document::from_unsaved_text(record.name.clone(), record.path.clone(), &record.text);
            document.recovery_id = Some(record.id.clone());
            documents.push(document);
        }

        if documents.is_empty() {
            return warnings;
        }

        let active_index = session.active.min(documents.len() - 1);
        let active = documents.remove(active_index);

        self.documents = DocumentManager::new();
        for document in documents {
            self.documents.push_inactive(document);
        }
        self.documents.set_active_index(active_index);
        self.install_active_document(active);
        self.untitled_sequence = session
            .documents
            .iter()
            .filter(|document| document.path.is_none())
            .count() as u64;
        self.overwrite_pending = false;
        self.sync_viewport();
        self.ensure_visible();
        self.refresh_watcher();
        self.needs_redraw = true;

        warnings
    }

    /// Verifica alterações externas em todas as abas.
    fn check_external_changes(&mut self) {
        // Mantém o watcher sincronizado mesmo quando a aba ativa mudou sem
        // passar por `refresh_watcher` (por exemplo, após remapear caminhos).
        let stale_watcher = self.file_watcher.as_ref().is_none_or(|watcher| {
            Some(watcher.path.as_path()) != self.path.as_deref() || watcher.fingerprint != self.disk
        });
        if stale_watcher {
            self.refresh_watcher();
        }

        let root = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf());
        let mut message = None;

        self.with_active_document(|document, _| {
            message = evaluate_external_change(document, root.as_deref());
        });

        if let Some((kind, text)) = message {
            self.report_message(kind, text);
        }

        let mut messages = Vec::new();
        for document in self.documents.inactive_mut() {
            if let Some(message) = evaluate_external_change(document, root.as_deref()) {
                messages.push(message);
            }
        }

        for (kind, text) in messages {
            self.report_message(kind, text);
        }

        self.refresh_watcher();
    }

    /// Recarrega a aba ativa do disco, descartando a versão local.
    fn reload_active_from_disk(&mut self) {
        let Some(path) = self.path.clone() else {
            self.set_error("o documento não possui arquivo no disco");
            return;
        };

        match files::read_document(&path) {
            Ok(text) => {
                let disk = fingerprint(&path).ok().flatten();
                self.with_active_document(|document, _| {
                    document.reload_text(&text);
                    document.disk = disk;
                });
                self.overwrite_pending = false;
                self.sync_viewport();
                self.ensure_visible();
                self.refresh_watcher();
                self.set_info("documento recarregado do disco");
            }
            Err(error) => self.set_error(error.to_string()),
        }
    }

    /// Solicita o encerramento, preservando alterações não salvas.
    fn request_quit(&mut self) {
        if self.confirm.is_some() {
            return;
        }

        // Um prompt aberto é cancelado antes de avaliar a saída para não deixar
        // uma operação de arquivo pendente nem duplicar confirmações.
        if self.prompt.is_some() {
            self.prompt = None;
            self.pending_after_save = None;
            self.mode = Mode::Editor;
        }

        self.commit_session();

        // Abas inativas não participam do diálogo de saída: exigir que elas
        // sejam resolvidas antes evita perder alterações silenciosamente.
        if self
            .documents
            .inactive()
            .iter()
            .any(|document| document.is_modified() || document.recovered)
        {
            self.set_error(
                "há abas com alterações não salvas; salve ou feche essas abas antes de sair",
            );
            return;
        }

        if self.buffer.is_consistent() && !self.buffer.is_modified() && !self.recovered {
            self.persist_before_quit();
            self.running = false;
            return;
        }

        self.open_confirm(PendingAction::Quit);
    }

    fn open_confirm(&mut self, pending: PendingAction) {
        if self.confirm.is_some() {
            return;
        }

        let save_available = self.buffer.is_consistent() && !pending.is_delete();
        let selected = if pending.is_delete() {
            ConfirmChoice::Cancel
        } else if save_available {
            ConfirmChoice::Save
        } else {
            ConfirmChoice::Cancel
        };

        self.confirm = Some(ConfirmDialog {
            pending,
            selected,
            save_available,
            error: None,
        });
        self.mode = Mode::Confirm;
        self.needs_redraw = true;
    }

    fn apply_confirm_action(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::MoveLeft => self.move_confirm_selection(false),
            ConfirmAction::MoveRight => self.move_confirm_selection(true),
            ConfirmAction::Save => self.resolve_confirm(ConfirmChoice::Save),
            ConfirmAction::Discard => self.resolve_confirm(ConfirmChoice::Discard),
            ConfirmAction::Cancel => self.cancel_confirm(),
            ConfirmAction::Activate => {
                if let Some(dialog) = self.confirm.as_ref() {
                    self.resolve_confirm(dialog.selected());
                }
            }
        }
    }

    fn move_confirm_selection(&mut self, forward: bool) {
        let Some(dialog) = self.confirm.as_mut() else {
            return;
        };

        dialog.error = None;
        let selected = dialog.selected;
        dialog.selected = if forward {
            selected.next(dialog.save_available)
        } else {
            selected.previous(dialog.save_available)
        };
        self.needs_redraw = true;
    }

    fn resolve_confirm(&mut self, choice: ConfirmChoice) {
        if choice == ConfirmChoice::Cancel {
            self.cancel_confirm();
            return;
        }

        let Some(dialog) = self.confirm.as_ref() else {
            return;
        };

        if choice == ConfirmChoice::Save && !dialog.save_available() {
            self.set_confirm_error(
                "documento em recuperação pendente; salvar indisponível".to_owned(),
            );
            return;
        }

        let pending = dialog.pending.clone();

        match choice {
            ConfirmChoice::Save => self.save_for_pending(pending),
            ConfirmChoice::Discard => self.complete_pending(pending),
            ConfirmChoice::Cancel => {}
        }
    }

    fn save_for_pending(&mut self, pending: PendingAction) {
        match self.path().map(Path::to_path_buf) {
            Some(path) => match self.save_document_to(&path) {
                Ok(()) => self.complete_pending(pending),
                Err(error) => self.set_confirm_error(error.to_string()),
            },
            None => {
                self.pending_after_save = Some(pending);
                self.confirm = None;
                self.open_prompt(PromptKind::SaveAs);
            }
        }
    }

    fn complete_pending(&mut self, pending: PendingAction) {
        self.confirm = None;
        self.pending_after_save = None;
        self.mode = Mode::Editor;

        match pending {
            PendingAction::Quit => {
                self.persist_before_quit();
                self.running = false;
            }
            PendingAction::Open(path) => {
                if let Err(error) = self.open_document(&path) {
                    self.set_error(error.to_string());
                }
            }
            PendingAction::CloseTab => {
                self.close_tab_now(self.documents.active_index());
            }
            PendingAction::Delete { path, permanent } => {
                self.perform_delete(&path, permanent);
            }
        }

        self.needs_redraw = true;
    }

    fn cancel_confirm(&mut self) {
        let kind = self.confirm.as_ref().map(ConfirmDialog::kind);
        self.confirm = None;
        self.pending_after_save = None;
        self.mode = Mode::Editor;
        self.needs_redraw = true;

        match kind {
            Some(ConfirmKind::Quit) => self.set_info("saída cancelada"),
            Some(ConfirmKind::Open) => self.set_info("abertura cancelada"),
            Some(ConfirmKind::CloseTab) => self.set_info("fechamento de aba cancelado"),
            Some(ConfirmKind::Delete) => self.set_info("exclusão cancelada"),
            None => {}
        }
    }

    fn set_confirm_error(&mut self, text: String) {
        if let Some(dialog) = self.confirm.as_mut() {
            dialog.error = Some(text.clone());
        }
        self.set_error(text);
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            kind: MessageKind::Info,
            text: text.into(),
        });
        self.needs_redraw = true;
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            kind: MessageKind::Error,
            text: text.into(),
        });
        self.needs_redraw = true;
    }

    fn report_buffer_error(&mut self, context: &str, error: &BufferError) {
        self.set_error(format!("{context}: {error}"));
    }

    fn report_recovery(&mut self) {
        self.set_error("o documento está em recuperação pendente; edição indisponível");
    }

    /// Coloca o documento em recuperação pendente para testes de segurança.
    #[cfg(test)]
    pub fn force_recovery_for_tests(&mut self) {
        self.commit_session();
        self.buffer.force_recovery_pending_for_tests();
        self.needs_redraw = true;
    }

    /// Define uma mensagem de status para testes de renderização.
    #[cfg(test)]
    pub fn set_info_for_tests(&mut self, text: &str) {
        self.set_info(text);
    }
}

/// Calcula o início da janela horizontal do prompt.
///
/// O cursor permanece visível. Quando está no fim do texto, a janela mostra o
/// máximo possível do sufixo; quando está no meio, o cursor é centralizado.
fn prompt_visible_start(input: &str, cursor: usize, width: usize) -> usize {
    let characters: Vec<char> = input.chars().collect();
    let cursor = cursor.min(characters.len());

    if width == 0 || characters.is_empty() {
        return cursor;
    }

    let mut columns = Vec::with_capacity(characters.len() + 1);
    let mut column = 0usize;
    columns.push(0);
    for &character in &characters {
        column += char_width(character, column);
        columns.push(column);
    }

    let cursor_column = columns[cursor];
    let cursor_width = if cursor < characters.len() {
        (columns[cursor + 1] - columns[cursor]).max(1)
    } else {
        1
    };

    if cursor_column + cursor_width <= width {
        return 0;
    }

    if cursor == characters.len() {
        let mut start = cursor;
        while start > 0 {
            let candidate = start - 1;
            if cursor_column - columns[candidate] + cursor_width > width {
                break;
            }
            start = candidate;
        }
        return start;
    }

    let target_left = cursor_column.saturating_sub(width / 2);
    let mut start = 0;
    while start < cursor && columns[start + 1] <= target_left {
        start += 1;
    }

    while start < cursor && cursor_column - columns[start] + cursor_width > width {
        start += 1;
    }

    start
}

impl Drop for App {
    fn drop(&mut self) {
        // O `Drop` não pode reportar erros. Em encerramento normal a sessão já
        // foi concluída; esta é apenas uma rede de segurança para panics.
        if self.buffer.has_active_transaction() {
            let _ = self.buffer.cancel_transaction();
        }
    }
}

fn document_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn session_document(path: Option<PathBuf>, view: &View) -> SessionDocument {
    let cursor = view.cursor().0;
    let anchor = view.selection_range().map(|range| {
        if range.end.0 == cursor {
            range.start.0
        } else {
            range.end.0
        }
    });

    SessionDocument {
        path,
        cursor,
        anchor,
        top_line: view.top_line(),
        left_col: view.left_col(),
    }
}

fn canonical_for_compare(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    crate::workspace::absolute_path(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_within(candidate: &Path, target: &Path) -> bool {
    canonical_for_compare(candidate).starts_with(canonical_for_compare(target))
}

/// Compara o disco com a versão conhecida e devolve a mensagem a exibir.
fn evaluate_external_change(
    document: &mut Document,
    workspace_root: Option<&Path>,
) -> Option<(MessageKind, String)> {
    match crate::watcher::detect_change(document, workspace_root) {
        DetectedChange::Unchanged => None,
        DetectedChange::Modified(current) => {
            let had_local_changes = document.is_modified() || document.recovered;

            if !had_local_changes
                && let Some(path) = document.path.clone()
                && let Ok(text) = files::read_document(&path)
            {
                document.reload_text(&text);
                document.disk = Some(current);
                return Some((
                    MessageKind::Info,
                    "arquivo alterado externamente e recarregado do disco".to_owned(),
                ));
            }

            document.disk = Some(current);
            document.missing = false;
            document.conflict = had_local_changes;

            Some(if had_local_changes {
                (
                    MessageKind::Error,
                    "arquivo alterado externamente; Ctrl+S novamente mantém a versão local ou Ctrl+R recarrega do disco".to_owned(),
                )
            } else {
                (
                    MessageKind::Info,
                    "arquivo alterado externamente; versão do disco registrada".to_owned(),
                )
            })
        }
        DetectedChange::Removed => {
            document.missing = true;
            document.disk = None;
            Some((
                MessageKind::Error,
                "arquivo removido do disco; salve para recriá-lo ou use Ctrl+R quando ele voltar"
                    .to_owned(),
            ))
        }
        DetectedChange::Renamed(new_path) => {
            let text = document.buffer.text().unwrap_or_default();
            document.name = document_name(&new_path);
            document.syntax =
                LanguageId::from_path(&new_path).map(|language| SyntaxState::new(language, &text));
            document.disk = fingerprint(&new_path).ok().flatten();
            document.missing = false;
            document.path = Some(new_path.clone());

            Some((
                MessageKind::Info,
                format!(
                    "arquivo renomeado externamente; agora em `{}`",
                    new_path.display()
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use nexus_syntax::TokenKind;
    use ratatui::crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-tui-app-{}-{}-{label}",
                std::process::id(),
                counter
            ));
            fs::create_dir_all(&path).expect("criar diretório temporário");
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

    fn app(text: &str) -> App {
        let mut app = App::with_text(80, 24, text);
        app.mark_rendered();
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.update(Event::Key(key(code)));
    }

    fn press_ctrl(app: &mut App, code: KeyCode) {
        app.update(Event::Key(ctrl(code)));
    }

    fn press_shift(app: &mut App, code: KeyCode) {
        app.update(Event::Key(shift(code)));
    }

    fn text(app: &App) -> String {
        app.buffer().text().unwrap()
    }

    fn has_token(app: &App, kind: TokenKind, needle: &str) -> bool {
        let Some(syntax) = app.syntax() else {
            return false;
        };
        let source = syntax.text();

        syntax.highlights().iter().any(|span| {
            if span.kind != kind {
                return false;
            }

            let start = source
                .char_indices()
                .nth(span.range.start.0)
                .map(|(byte, _)| byte)
                .unwrap_or(source.len());
            let end = source
                .char_indices()
                .nth(span.range.end.0)
                .map(|(byte, _)| byte)
                .unwrap_or(source.len());

            source[start..end].contains(needle)
        })
    }

    #[test]
    fn new_app_starts_empty_and_running() {
        let app = App::new(80, 24);

        assert!(app.is_running());
        assert_eq!(text(&app), "");
        assert_eq!(app.view().cursor(), CharOffset(0));
        assert!(!app.buffer().is_modified());
        assert_eq!(app.mode(), Mode::Editor);
    }

    #[test]
    fn ctrl_c_requests_quit() {
        let mut app = App::new(80, 24);

        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));

        assert!(!app.is_running());
    }

    #[test]
    fn unrelated_keys_do_not_change_state() {
        let mut app = App::new(80, 24);
        app.mark_rendered();

        app.update(Event::Key(KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::NONE,
        )));
        app.update(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        )));

        assert!(app.is_running());
        assert!(!app.needs_redraw());
    }

    #[test]
    fn resize_updates_viewport_and_requests_redraw() {
        let mut app = App::new(80, 24);
        app.mark_rendered();

        app.update(Event::Resize {
            width: 120,
            height: 40,
        });

        assert_eq!(app.terminal_size(), (120, 40));
        assert!(app.needs_redraw());
    }

    #[test]
    fn typing_is_grouped_into_one_transaction() {
        let mut app = app("");

        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Char('c'));

        assert_eq!(text(&app), "abc");
        assert_eq!(app.view().cursor(), CharOffset(3));
        assert!(app.buffer().has_active_transaction());

        app.on_idle();

        assert!(!app.buffer().has_active_transaction());
        assert_eq!(app.buffer().revision(), 1);
        assert!(app.buffer().can_undo());

        press_ctrl(&mut app, KeyCode::Char('z'));

        assert_eq!(text(&app), "");
        assert_eq!(app.view().cursor(), CharOffset(0));
    }

    #[test]
    fn movement_commits_the_typing_session() {
        let mut app = app("");

        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Left);

        assert!(!app.buffer().has_active_transaction());
        assert_eq!(app.view().cursor(), CharOffset(1));
        assert_eq!(text(&app), "ab");

        press_ctrl(&mut app, KeyCode::Char('z'));

        assert_eq!(text(&app), "");
    }

    #[test]
    fn changing_operation_starts_a_new_session() {
        let mut app = app("");

        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Backspace);
        app.on_idle();

        assert_eq!(text(&app), "a");

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "ab");

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "");
    }

    #[test]
    fn inserts_spaces_tabs_and_newlines() {
        let mut app = app("");

        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('x'));

        assert_eq!(text(&app), " \t\nx");
        assert_eq!(app.view().cursor(), CharOffset(4));
    }

    #[test]
    fn backspace_at_line_start_joins_lines() {
        let mut app = app("ab\ncd");
        app.view.set_cursor(&app.buffer, CharOffset(3)).unwrap();

        press(&mut app, KeyCode::Backspace);

        assert_eq!(text(&app), "abcd");
        assert_eq!(app.view().cursor(), CharOffset(2));

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "ab\ncd");
    }

    #[test]
    fn delete_at_line_end_joins_lines() {
        let mut app = app("ab\ncd");
        app.view.set_cursor(&app.buffer, CharOffset(2)).unwrap();

        press(&mut app, KeyCode::Delete);

        assert_eq!(text(&app), "abcd");
        assert_eq!(app.view().cursor(), CharOffset(2));

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "ab\ncd");
    }

    #[test]
    fn backspace_removes_previous_character() {
        let mut app = app("café");
        app.view.set_cursor(&app.buffer, CharOffset(4)).unwrap();

        press(&mut app, KeyCode::Backspace);

        assert_eq!(text(&app), "caf");
        assert_eq!(app.view().cursor(), CharOffset(3));
    }

    #[test]
    fn delete_removes_next_character() {
        let mut app = app("café");
        app.view.set_cursor(&app.buffer, CharOffset(3)).unwrap();

        press(&mut app, KeyCode::Delete);

        assert_eq!(text(&app), "caf");
        assert_eq!(app.view().cursor(), CharOffset(3));
    }

    #[test]
    fn selection_can_be_deleted() {
        let mut app = app("abcdef");
        app.view
            .set_selection(&app.buffer, CharOffset(1), CharOffset(4))
            .unwrap();

        press(&mut app, KeyCode::Delete);

        assert_eq!(text(&app), "aef");
        assert_eq!(app.view().cursor(), CharOffset(1));
        assert!(!app.view().has_selection());

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "abcdef");
    }

    #[test]
    fn selection_can_be_replaced_by_typing() {
        let mut app = app("abcdef");
        app.view
            .set_selection(&app.buffer, CharOffset(1), CharOffset(4))
            .unwrap();

        press(&mut app, KeyCode::Char('X'));

        assert_eq!(text(&app), "aXef");
        assert_eq!(app.view().cursor(), CharOffset(2));

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "abcdef");

        press_ctrl(&mut app, KeyCode::Char('y'));
        assert_eq!(text(&app), "aXef");
    }

    #[test]
    fn select_all_then_replace_entire_document() {
        let mut app = app("café\n界");

        press_ctrl(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('x'));

        assert_eq!(text(&app), "x");
        assert_eq!(app.view().cursor(), CharOffset(1));

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "café\n界");
    }

    #[test]
    fn escape_cancels_selection_without_touching_buffer() {
        let mut app = app("abc");

        press_shift(&mut app, KeyCode::Right);
        press_shift(&mut app, KeyCode::Right);
        assert!(app.view().has_selection());

        press(&mut app, KeyCode::Esc);

        assert!(!app.view().has_selection());
        assert_eq!(text(&app), "abc");
    }

    #[test]
    fn undo_and_redo_keep_cursor_valid() {
        let mut app = app("");

        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('b'));
        press(&mut app, KeyCode::Char('c'));
        app.on_idle();

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "");
        assert_eq!(app.view().cursor(), CharOffset(0));

        press_ctrl(&mut app, KeyCode::Char('y'));
        assert_eq!(text(&app), "abc");
        assert!(app.view().cursor().0 <= app.buffer().len_chars().unwrap());
        assert_eq!(app.view().cursor_line(app.buffer()).unwrap(), 0);
    }

    #[test]
    fn new_edit_after_undo_invalidates_redo() {
        let mut app = app("");

        press(&mut app, KeyCode::Char('a'));
        app.on_idle();
        press_ctrl(&mut app, KeyCode::Char('z'));
        assert!(app.buffer().can_redo());

        press(&mut app, KeyCode::Char('b'));
        app.on_idle();

        assert!(!app.buffer().can_redo());
        assert_eq!(text(&app), "b");
    }

    #[test]
    fn cancel_session_reverts_uncommitted_edits() {
        let mut app = app("abc");
        app.view.set_cursor(&app.buffer, CharOffset(3)).unwrap();

        press(&mut app, KeyCode::Char('d'));
        assert_eq!(text(&app), "abcd");
        assert!(app.buffer().has_active_transaction());

        app.cancel_session();

        assert_eq!(text(&app), "abc");
        assert!(!app.buffer().has_active_transaction());
        assert_eq!(app.buffer().revision(), 0);
        assert!(!app.buffer().is_modified());
    }

    #[test]
    fn open_document_replaces_current_document() {
        let directory = TempDir::new("open-valid");
        let path = directory.join("example.txt");
        fs::write(&path, "conteúdo\n").unwrap();
        let mut app = app("antigo");

        app.open_document(&path).unwrap();

        assert_eq!(text(&app), "conteúdo\n");
        assert_eq!(app.path(), Some(path.as_path()));
        assert_eq!(app.document_name(), "example.txt");
        assert_eq!(app.view().cursor(), CharOffset(0));
        assert!(!app.buffer().is_modified());
    }

    #[test]
    fn open_empty_document() {
        let directory = TempDir::new("open-empty");
        let path = directory.join("empty.txt");
        fs::write(&path, "").unwrap();
        let mut app = app("antigo");

        app.open_document(&path).unwrap();

        assert_eq!(text(&app), "");
        assert_eq!(app.buffer().line_count().unwrap(), 1);
    }

    #[test]
    fn failed_open_keeps_current_document() {
        let directory = TempDir::new("open-missing");
        let path = directory.join("missing.txt");
        let mut app = app("antigo");

        let result = app.open_document(&path);

        assert!(result.is_err());
        assert_eq!(text(&app), "antigo");
        assert_eq!(app.path(), None);
        assert_eq!(app.document_name(), "sem título");
    }

    #[test]
    fn invalid_utf8_open_keeps_current_document() {
        let directory = TempDir::new("open-invalid-utf8");
        let path = directory.join("invalid.bin");
        fs::write(&path, [0xFF, 0xFE]).unwrap();
        let mut app = app("antigo");

        let result = app.open_document(&path);

        assert!(result.is_err());
        assert_eq!(text(&app), "antigo");
    }

    #[test]
    fn save_preserves_line_endings_and_marks_document_saved() {
        let directory = TempDir::new("save");
        let path = directory.join("document.txt");
        let mut app = app("linha 1\r\nlinha 2\r\n");

        app.save_document_to(&path).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"linha 1\r\nlinha 2\r\n");
        assert!(!app.buffer().is_modified());
        assert_eq!(app.path(), Some(path.as_path()));
        assert_eq!(app.document_name(), "document.txt");
        assert_eq!(app.line_ending, "\r\n");
    }

    #[test]
    fn editing_after_save_marks_document_modified_again() {
        let directory = TempDir::new("save-then-edit");
        let path = directory.join("document.txt");
        let mut app = app("abc");

        app.save_document_to(&path).unwrap();
        assert!(!app.buffer().is_modified());

        app.view.set_cursor(&app.buffer, CharOffset(3)).unwrap();
        press(&mut app, KeyCode::Char('d'));
        app.on_idle();

        assert!(app.buffer().is_modified());

        app.save_document_to(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"abcd");
        assert!(!app.buffer().is_modified());
    }

    #[test]
    fn save_failure_keeps_document_modified() {
        let directory = TempDir::new("save-failure");
        let path = directory.join("missing").join("document.txt");
        let mut app = app("abc");

        press(&mut app, KeyCode::Char('d'));
        app.on_idle();
        assert!(app.buffer().is_modified());

        let result = app.save_document_to(&path);

        assert!(result.is_err());
        assert!(app.buffer().is_modified());
        assert_eq!(app.path(), None);
        assert!(!path.exists());
    }

    #[test]
    fn save_as_switches_language_and_creates_syntax_state() {
        let directory = TempDir::new("save-as-language");
        let path = directory.join("programa.rs");
        let mut app = app("fn main() {}");

        assert!(app.syntax().is_none());
        assert_eq!(app.language_name(), "Texto");

        app.save_document_to(&path).unwrap();

        assert!(app.syntax().is_some());
        assert_eq!(app.language_name(), "Rust");
        assert_eq!(app.syntax().unwrap().text(), text(&app));
        assert!(has_token(&app, TokenKind::Keyword, "fn"));
    }

    #[test]
    fn save_as_with_same_language_preserves_syntax_tree() {
        let directory = TempDir::new("save-as-same-language");
        let path = directory.join("outro.rs");
        let mut app = App::with_document(80, 24, Path::new("main.rs"), "fn main() {}");
        let before = app.syntax().unwrap().update_count();

        app.save_document_to(&path).unwrap();

        assert_eq!(app.syntax().unwrap().update_count(), before);
    }

    #[test]
    fn typing_and_undo_redo_keep_syntax_in_sync() {
        let mut app = App::with_document(80, 24, Path::new("main.rs"), "fn main() {}");
        app.mark_rendered();
        app.view.set_cursor(&app.buffer, CharOffset(12)).unwrap();

        press(&mut app, KeyCode::Enter);
        for character in "// nota".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        app.on_idle();

        assert_eq!(app.syntax().unwrap().text(), text(&app));
        assert!(has_token(&app, TokenKind::Comment, "nota"));

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(app.syntax().unwrap().text(), text(&app));
        assert!(!has_token(&app, TokenKind::Comment, "nota"));

        press_ctrl(&mut app, KeyCode::Char('y'));
        assert_eq!(app.syntax().unwrap().text(), text(&app));
        assert!(has_token(&app, TokenKind::Comment, "nota"));
    }

    #[test]
    fn deleting_text_updates_syntax_state() {
        let mut app = App::with_document(
            80,
            24,
            Path::new("main.rs"),
            "fn main() { let valor = 42; }",
        );
        app.view.set_cursor(&app.buffer, CharOffset(26)).unwrap();
        app.mark_rendered();

        assert!(has_token(&app, TokenKind::Number, "42"));

        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        app.on_idle();

        assert_eq!(app.syntax().unwrap().text(), text(&app));
        assert!(!has_token(&app, TokenKind::Number, "42"));
    }

    #[test]
    fn newline_insertion_uses_detected_line_ending() {
        let mut app = app("a\r\nb");
        app.view.set_cursor(&app.buffer, CharOffset(3)).unwrap();

        press(&mut app, KeyCode::Enter);

        assert_eq!(text(&app), "a\r\n\r\nb");
        assert_eq!(app.line_ending, "\r\n");
    }

    #[test]
    fn open_detects_crlf_line_ending() {
        let directory = TempDir::new("open-crlf");
        let path = directory.join("document.txt");
        fs::write(&path, "a\r\nb").unwrap();
        let mut app = app("");

        app.open_document(&path).unwrap();
        assert_eq!(app.line_ending, "\r\n");

        app.view.set_cursor(&app.buffer, CharOffset(1)).unwrap();
        press(&mut app, KeyCode::Enter);

        assert_eq!(text(&app), "a\r\n\r\nb");
    }

    #[test]
    fn ctrl_s_without_path_opens_save_as_prompt() {
        let mut app = app("");

        press_ctrl(&mut app, KeyCode::Char('s'));

        assert_eq!(app.mode(), Mode::Prompt(PromptKind::SaveAs));
        assert!(app.prompt().is_some());
    }

    #[test]
    fn prompt_save_as_writes_file_and_marks_saved() {
        let directory = TempDir::new("prompt-save");
        let path = directory.join("new.txt");
        let mut app = app("");

        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('i'));
        app.on_idle();

        press_ctrl(&mut app, KeyCode::Char('s'));
        for character in path.to_string_lossy().chars() {
            app.apply_action(Action::PromptChar(character));
        }
        app.apply_action(Action::PromptConfirm);

        assert_eq!(app.mode(), Mode::Editor);
        assert!(app.prompt().is_none());
        assert_eq!(fs::read(&path).unwrap(), b"hi");
        assert_eq!(app.path(), Some(path.as_path()));
        assert_eq!(app.document_name(), "new.txt");
        assert!(!app.buffer().is_modified());
    }

    #[test]
    fn ctrl_s_with_associated_path_saves_directly() {
        let directory = TempDir::new("ctrl-s");
        let path = directory.join("document.txt");
        let mut app = app("abc");

        app.save_document_to(&path).unwrap();
        app.view.set_cursor(&app.buffer, CharOffset(3)).unwrap();
        press(&mut app, KeyCode::Char('d'));
        app.on_idle();

        press_ctrl(&mut app, KeyCode::Char('s'));

        assert_eq!(fs::read(&path).unwrap(), b"abcd");
        assert!(!app.buffer().is_modified());
        assert_eq!(app.mode(), Mode::Editor);
    }

    #[test]
    fn prompt_edits_and_confirms_open() {
        let directory = TempDir::new("prompt-open");
        let path = directory.join("document.txt");
        fs::write(&path, "texto").unwrap();
        let mut app = app("antigo");

        press_ctrl(&mut app, KeyCode::Char('o'));
        for character in path.to_string_lossy().chars() {
            app.apply_action(Action::PromptChar(character));
        }
        app.apply_action(Action::PromptConfirm);

        assert_eq!(app.mode(), Mode::Editor);
        assert!(app.prompt().is_none());
        assert_eq!(text(&app), "texto");
    }

    #[test]
    fn prompt_cancel_keeps_document() {
        let mut app = app("antigo");

        press_ctrl(&mut app, KeyCode::Char('o'));
        app.apply_action(Action::PromptChar('x'));
        app.apply_action(Action::PromptCancel);

        assert_eq!(app.mode(), Mode::Editor);
        assert!(app.prompt().is_none());
        assert_eq!(text(&app), "antigo");
    }

    #[test]
    fn prompt_open_failure_keeps_prompt_and_document() {
        let directory = TempDir::new("prompt-open-failure");
        let path = directory.join("missing.txt");
        let mut app = app("antigo");

        press_ctrl(&mut app, KeyCode::Char('o'));
        for character in path.to_string_lossy().chars() {
            app.apply_action(Action::PromptChar(character));
        }
        app.apply_action(Action::PromptConfirm);

        assert_eq!(app.mode(), Mode::Prompt(PromptKind::Open));
        assert_eq!(
            app.prompt().map(Prompt::input),
            Some(path.to_string_lossy().as_ref())
        );
        assert_eq!(text(&app), "antigo");
        assert_eq!(
            app.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
    }

    #[test]
    fn prompt_supports_cursor_editing() {
        let mut app = app("");

        press_ctrl(&mut app, KeyCode::Char('o'));
        app.apply_action(Action::PromptChar('a'));
        app.apply_action(Action::PromptChar('b'));
        app.apply_action(Action::PromptChar('c'));
        app.apply_action(Action::PromptLeft);
        app.apply_action(Action::PromptBackspace);
        app.apply_action(Action::PromptEnd);
        app.apply_action(Action::PromptChar('d'));

        assert_eq!(app.prompt().map(Prompt::input), Some("acd"));
    }

    #[test]
    fn help_mode_opens_and_closes() {
        let mut app = app("");

        press_ctrl(&mut app, KeyCode::Char('h'));
        assert_eq!(app.mode(), Mode::Help);

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.mode(), Mode::Editor);
    }

    fn modified_app(text: &str) -> App {
        let mut application = app(text);
        let len = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(len))
            .unwrap();
        press(&mut application, KeyCode::Char('x'));
        application.on_idle();
        application
    }

    fn type_prompt(app: &mut App, text: &str) {
        for character in text.chars() {
            app.apply_action(Action::PromptChar(character));
        }
    }

    // ----- Saída -----

    #[test]
    fn quit_unmodified_does_not_show_confirmation() {
        let mut application = app("abc");

        press_ctrl(&mut application, KeyCode::Char('q'));

        assert!(!application.is_running());
        assert!(application.confirm().is_none());
    }

    #[test]
    fn quit_modified_opens_confirmation() {
        let mut application = modified_app("");

        press_ctrl(&mut application, KeyCode::Char('q'));

        assert!(application.is_running());
        assert_eq!(application.mode(), Mode::Confirm);
        let dialog = application.confirm().unwrap();
        assert_eq!(dialog.kind(), ConfirmKind::Quit);
        assert_eq!(dialog.selected(), ConfirmChoice::Save);
        assert!(dialog.save_available());
    }

    #[test]
    fn quit_save_writes_and_exits() {
        let directory = TempDir::new("quit-save");
        let path = directory.join("document.txt");
        let mut application = modified_app("abc");
        application.path = Some(path.clone());

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('s'));

        assert!(!application.is_running());
        assert_eq!(fs::read(&path).unwrap(), b"abcx");
    }

    #[test]
    fn quit_save_without_path_uses_save_as_and_exits() {
        let directory = TempDir::new("quit-save-as");
        let path = directory.join("document.txt");
        let mut application = modified_app("abc");

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('s'));

        assert_eq!(application.mode(), Mode::Prompt(PromptKind::SaveAs));
        type_prompt(&mut application, &path.to_string_lossy());
        application.apply_action(Action::PromptConfirm);

        assert!(!application.is_running());
        assert_eq!(fs::read(&path).unwrap(), b"abcx");
    }

    #[test]
    fn quit_discard_exits_without_saving() {
        let directory = TempDir::new("quit-discard");
        let path = directory.join("document.txt");
        let mut application = app("abc");
        application.save_document_to(&path).unwrap();

        let len = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(len))
            .unwrap();
        press(&mut application, KeyCode::Char('d'));
        application.on_idle();

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('d'));

        assert!(!application.is_running());
        assert_eq!(fs::read(&path).unwrap(), b"abc");
    }

    #[test]
    fn quit_cancel_keeps_document_and_running() {
        let mut application = modified_app("abc");
        let before = text(&application);

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Esc);

        assert!(application.is_running());
        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(text(&application), before);
        assert!(application.buffer().is_modified());
    }

    #[test]
    fn quit_save_failure_keeps_running_and_allows_retry() {
        let directory = TempDir::new("quit-save-retry");
        let bad_path = directory.join("missing").join("document.txt");
        let good_path = directory.join("document.txt");
        let mut application = modified_app("abc");
        application.path = Some(bad_path);

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('s'));

        assert!(application.is_running());
        assert_eq!(application.mode(), Mode::Confirm);
        assert!(application.confirm().unwrap().error().is_some());
        assert!(application.buffer().is_modified());
        assert!(!good_path.exists());

        application.path = Some(good_path.clone());
        press(&mut application, KeyCode::Char('s'));

        assert!(!application.is_running());
        assert_eq!(fs::read(&good_path).unwrap(), b"abcx");
    }

    #[test]
    fn quit_recovery_requires_explicit_discard() {
        let mut application = app("abc");
        application.force_recovery_for_tests();

        press_ctrl(&mut application, KeyCode::Char('q'));

        assert!(application.is_running());
        let dialog = application.confirm().unwrap();
        assert_eq!(dialog.kind(), ConfirmKind::Quit);
        assert!(!dialog.save_available());
        assert_eq!(dialog.selected(), ConfirmChoice::Cancel);

        press(&mut application, KeyCode::Char('s'));
        assert!(application.is_running());
        assert!(application.confirm().unwrap().error().is_some());

        press(&mut application, KeyCode::Char('d'));
        assert!(!application.is_running());
    }

    #[test]
    fn quit_with_active_session_commits_before_confirming() {
        let mut application = app("");

        press(&mut application, KeyCode::Char('a'));
        assert!(application.buffer().has_active_transaction());

        press_ctrl(&mut application, KeyCode::Char('q'));

        assert!(!application.buffer().has_active_transaction());
        assert!(application.buffer().can_undo());
        assert_eq!(application.buffer().revision(), 1);
        assert_eq!(application.mode(), Mode::Confirm);
    }

    // ----- Abertura -----

    #[test]
    fn open_with_modified_document_prompts_before_opening() {
        let directory = TempDir::new("open-modified");
        let target = directory.join("target.txt");
        fs::write(&target, "novo").unwrap();
        let mut application = modified_app("antigo");

        press_ctrl(&mut application, KeyCode::Char('o'));
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::Open));
        type_prompt(&mut application, &target.to_string_lossy());
        application.apply_action(Action::PromptConfirm);

        assert_eq!(application.mode(), Mode::Confirm);
        let dialog = application.confirm().unwrap();
        assert_eq!(dialog.kind(), ConfirmKind::Open);
        assert_eq!(dialog.target(), Some(target.as_path()));
        assert_eq!(text(&application), "antigox");
    }

    #[test]
    fn open_save_and_continue() {
        let directory = TempDir::new("open-save-continue");
        let current = directory.join("current.txt");
        let target = directory.join("target.txt");
        fs::write(&target, "novo").unwrap();
        let mut application = modified_app("antigo");
        application.path = Some(current.clone());

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &target.to_string_lossy());
        application.apply_action(Action::PromptConfirm);
        press(&mut application, KeyCode::Char('s'));

        assert_eq!(fs::read(&current).unwrap(), b"antigox");
        assert_eq!(text(&application), "novo");
        assert_eq!(application.path(), Some(target.as_path()));
        assert_eq!(application.mode(), Mode::Editor);
    }

    #[test]
    fn open_save_without_path_uses_save_as_then_opens() {
        let directory = TempDir::new("open-save-as-continue");
        let current = directory.join("current.txt");
        let target = directory.join("target.txt");
        fs::write(&target, "novo").unwrap();
        let mut application = modified_app("antigo");

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &target.to_string_lossy());
        application.apply_action(Action::PromptConfirm);
        press(&mut application, KeyCode::Char('s'));

        assert_eq!(application.mode(), Mode::Prompt(PromptKind::SaveAs));
        type_prompt(&mut application, &current.to_string_lossy());
        application.apply_action(Action::PromptConfirm);

        assert_eq!(fs::read(&current).unwrap(), b"antigox");
        assert_eq!(text(&application), "novo");
        assert_eq!(application.path(), Some(target.as_path()));
    }

    #[test]
    fn open_discard_and_continue() {
        let directory = TempDir::new("open-discard-continue");
        let target = directory.join("target.txt");
        fs::write(&target, "novo").unwrap();
        let mut application = modified_app("antigo");

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &target.to_string_lossy());
        application.apply_action(Action::PromptConfirm);
        press(&mut application, KeyCode::Char('d'));

        assert_eq!(text(&application), "novo");
        assert_eq!(application.path(), Some(target.as_path()));
        assert_eq!(application.mode(), Mode::Editor);
    }

    #[test]
    fn open_cancel_preserves_document_cursor_and_selection() {
        let directory = TempDir::new("open-cancel");
        let target = directory.join("target.txt");
        fs::write(&target, "novo").unwrap();
        let mut application = modified_app("antigo");
        application
            .view
            .set_selection(&application.buffer, CharOffset(1), CharOffset(3))
            .unwrap();
        let before = text(&application);
        let cursor = application.view().cursor();
        let selection = application.view().selection_range();

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &target.to_string_lossy());
        application.apply_action(Action::PromptConfirm);
        press(&mut application, KeyCode::Esc);

        assert!(application.is_running());
        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(text(&application), before);
        assert_eq!(application.view().cursor(), cursor);
        assert_eq!(application.view().selection_range(), selection);
    }

    #[test]
    fn open_save_failure_keeps_current_document_and_allows_retry() {
        let directory = TempDir::new("open-save-retry");
        let bad_path = directory.join("missing").join("current.txt");
        let good_path = directory.join("current.txt");
        let target = directory.join("target.txt");
        fs::write(&target, "novo").unwrap();
        let mut application = modified_app("antigo");
        application.path = Some(bad_path);

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &target.to_string_lossy());
        application.apply_action(Action::PromptConfirm);
        press(&mut application, KeyCode::Char('s'));

        assert_eq!(application.mode(), Mode::Confirm);
        assert!(application.confirm().unwrap().error().is_some());
        assert_eq!(text(&application), "antigox");

        application.path = Some(good_path.clone());
        press(&mut application, KeyCode::Char('s'));

        assert_eq!(fs::read(&good_path).unwrap(), b"antigox");
        assert_eq!(text(&application), "novo");
    }

    #[test]
    fn open_failure_after_discard_keeps_current_document() {
        let directory = TempDir::new("open-discard-failure");
        let missing = directory.join("missing.txt");
        let mut application = modified_app("antigo");

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &missing.to_string_lossy());
        application.apply_action(Action::PromptConfirm);
        press(&mut application, KeyCode::Char('d'));

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(text(&application), "antigox");
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
    }

    #[test]
    fn open_failure_after_save_keeps_saved_current_document() {
        let directory = TempDir::new("open-save-then-failure");
        let current = directory.join("current.txt");
        let missing = directory.join("missing.txt");
        let mut application = modified_app("antigo");
        application.path = Some(current.clone());

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &missing.to_string_lossy());
        application.apply_action(Action::PromptConfirm);
        press(&mut application, KeyCode::Char('s'));

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(text(&application), "antigox");
        assert_eq!(application.path(), Some(current.as_path()));
        assert!(!application.buffer().is_modified());
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
    }

    #[test]
    fn recovery_open_requires_explicit_discard() {
        let directory = TempDir::new("open-recovery");
        let target = directory.join("target.txt");
        fs::write(&target, "novo").unwrap();
        let mut application = app("abc");
        application.force_recovery_for_tests();

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &target.to_string_lossy());
        application.apply_action(Action::PromptConfirm);

        assert_eq!(application.mode(), Mode::Confirm);
        assert!(!application.confirm().unwrap().save_available());

        press(&mut application, KeyCode::Char('d'));

        assert_eq!(text(&application), "novo");
        assert_eq!(application.path(), Some(target.as_path()));
    }

    #[test]
    fn confirmation_ignores_editor_and_command_keys() {
        let mut application = modified_app("abc");
        let before = text(&application);

        press_ctrl(&mut application, KeyCode::Char('q'));
        assert_eq!(application.mode(), Mode::Confirm);

        press(&mut application, KeyCode::Char('X'));
        press(&mut application, KeyCode::Backspace);
        press(&mut application, KeyCode::Delete);
        press_ctrl(&mut application, KeyCode::Char('z'));
        press_ctrl(&mut application, KeyCode::Char('o'));

        assert_eq!(text(&application), before);
        assert_eq!(application.mode(), Mode::Confirm);
        assert!(application.prompt().is_none());
        assert!(application.is_running());
    }

    #[test]
    fn repeated_quit_request_does_not_duplicate_confirmation() {
        let mut application = modified_app("abc");

        press_ctrl(&mut application, KeyCode::Char('q'));
        press_ctrl(&mut application, KeyCode::Char('q'));

        assert_eq!(application.mode(), Mode::Confirm);
        assert!(application.confirm().is_some());
        assert!(application.is_running());
    }

    #[test]
    fn cancel_save_as_during_pending_quit_cancels_exit() {
        let mut application = modified_app("abc");

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('s'));
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::SaveAs));

        application.apply_action(Action::PromptCancel);

        assert!(application.is_running());
        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(text(&application), "abcx");
    }

    // ----- Prompts -----

    #[test]
    fn prompt_visible_start_short_path_is_zero() {
        let prompt = Prompt::new(PromptKind::Open, "abc".to_owned());

        assert_eq!(prompt.visible_start(20), 0);
    }

    #[test]
    fn prompt_visible_start_long_path_keeps_cursor_at_end_visible() {
        let prompt = Prompt::new(PromptKind::Open, "a".repeat(100));

        let start = prompt.visible_start(10);

        assert_eq!(start, 91);
        assert!(start > 0);
    }

    #[test]
    fn prompt_visible_start_cursor_at_start_is_zero() {
        let mut prompt = Prompt::new(PromptKind::Open, "a".repeat(100));
        prompt.move_home();

        assert_eq!(prompt.visible_start(10), 0);
    }

    #[test]
    fn prompt_visible_start_accounts_for_double_width_characters() {
        let prompt = Prompt::new(PromptKind::Open, "界".repeat(10));

        let start = prompt.visible_start(10);

        assert_eq!(start, 6);
    }

    #[test]
    fn prompt_visible_start_centers_cursor_in_the_middle() {
        let mut prompt = Prompt::new(PromptKind::Open, "a".repeat(100));
        for _ in 0..50 {
            prompt.move_left();
        }

        let start = prompt.visible_start(10);

        assert_eq!(start, 45);
    }

    #[test]
    fn prompt_horizontal_navigation_keeps_cursor_valid() {
        let mut application = app("");

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &"a".repeat(80));
        let length = application.prompt().unwrap().input().chars().count();

        application.apply_action(Action::PromptHome);
        assert_eq!(application.prompt().unwrap().cursor(), 0);
        assert_eq!(application.prompt().unwrap().visible_start(10), 0);

        application.apply_action(Action::PromptEnd);
        assert_eq!(application.prompt().unwrap().cursor(), length);
        assert!(application.prompt().unwrap().visible_start(10) > 0);

        application.apply_action(Action::PromptLeft);
        assert_eq!(application.prompt().unwrap().cursor(), length - 1);

        application.apply_action(Action::PromptRight);
        assert_eq!(application.prompt().unwrap().cursor(), length);
    }

    #[test]
    fn prompt_preserves_spaces_and_unicode() {
        let mut application = app("");
        let path = "/tmp/café com espaço/界.txt";

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, path);

        assert_eq!(application.prompt().unwrap().input(), path);
    }

    #[test]
    fn opens_file_with_unicode_and_spaces_in_path() {
        let directory = TempDir::new("unicode-path");
        let path = directory.join("café 界.txt");
        fs::write(&path, "ok").unwrap();
        let mut application = app("");

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &path.to_string_lossy());
        application.apply_action(Action::PromptConfirm);

        assert_eq!(text(&application), "ok");
        assert_eq!(application.path(), Some(path.as_path()));
    }

    // ----- Integridade -----

    #[test]
    fn open_with_active_session_commits_before_prompt() {
        let mut application = app("");

        press(&mut application, KeyCode::Char('a'));
        assert!(application.buffer().has_active_transaction());

        press_ctrl(&mut application, KeyCode::Char('o'));

        assert!(!application.buffer().has_active_transaction());
        assert_eq!(application.buffer().revision(), 1);
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::Open));
    }

    #[test]
    fn saving_without_changes_does_not_create_revision() {
        let directory = TempDir::new("save-clean");
        let path = directory.join("document.txt");
        let mut application = app("abc");
        application.save_document_to(&path).unwrap();

        let revision = application.buffer().revision();
        assert_eq!(revision, 0);
        assert!(!application.buffer().is_modified());

        application.save_document_to(&path).unwrap();

        assert_eq!(application.buffer().revision(), revision);
        assert!(!application.buffer().is_modified());
    }

    #[test]
    fn failed_save_does_not_change_saved_revision() {
        let directory = TempDir::new("save-failure-revision");
        let path = directory.join("missing").join("document.txt");
        let mut application = app("");

        press(&mut application, KeyCode::Char('a'));
        application.on_idle();
        let saved_revision = application.buffer().saved_revision();

        let result = application.save_document_to(&path);

        assert!(result.is_err());
        assert_eq!(application.buffer().saved_revision(), saved_revision);
        assert!(application.buffer().is_modified());
    }

    #[test]
    fn successful_open_reports_success_only_after_completion() {
        let directory = TempDir::new("open-success-message");
        let path = directory.join("document.txt");
        fs::write(&path, "conteúdo").unwrap();
        let mut application = app("");

        press_ctrl(&mut application, KeyCode::Char('o'));
        type_prompt(&mut application, &path.to_string_lossy());
        application.apply_action(Action::PromptConfirm);

        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Info)
        );
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("arquivo aberto")
        );
        assert_eq!(text(&application), "conteúdo");
    }

    #[test]
    fn successful_save_reports_success_only_after_completion() {
        let directory = TempDir::new("save-success-message");
        let path = directory.join("document.txt");
        let mut application = modified_app("abc");
        application.path = Some(path.clone());

        press_ctrl(&mut application, KeyCode::Char('s'));

        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Info)
        );
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("arquivo salvo")
        );
        assert_eq!(fs::read(&path).unwrap(), b"abcx");
        assert!(!application.buffer().is_modified());
    }

    // ----- Abas e ciclo de documentos -----

    fn isolated_paths(root: &Path) -> AppPaths {
        AppPaths {
            config_file: root.join("config.toml"),
            themes_dir: root.join("themes"),
            root: root.to_path_buf(),
            session_file: root.join("session.toml"),
            recent_file: root.join("recent.toml"),
            recovery_dir: root.join("recovery"),
        }
    }

    #[test]
    fn tabs_preserve_state_switch_and_close_documents() {
        let directory = TempDir::new("tabs-cycle");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();

        assert_eq!(application.tab_count(), 2);
        assert_eq!(application.active_tab(), 1);
        assert_eq!(application.tab_names(), vec!["a.txt", "b.txt"]);
        assert_eq!(text(&application), "B");

        assert!(application.previous_tab());
        assert_eq!(text(&application), "A");

        application
            .view
            .set_cursor(&application.buffer, CharOffset(1))
            .unwrap();
        press(&mut application, KeyCode::Char('x'));
        application.on_idle();

        assert!(application.next_tab());
        assert_eq!(text(&application), "B");
        assert!(!application.tabs()[1].modified);
        assert!(application.tabs()[0].modified);

        application.close_tab(1);
        assert_eq!(application.tab_count(), 1);
        assert_eq!(application.tab_names(), vec!["a.txt"]);

        application.close_current_tab();
        assert_eq!(application.mode(), Mode::Confirm);
        assert_eq!(application.confirm().unwrap().kind(), ConfirmKind::CloseTab);
        press(&mut application, KeyCode::Char('d'));

        assert_eq!(application.tab_count(), 1);
        assert_eq!(application.document_name(), "sem título");
        assert_eq!(text(&application), "");
    }

    #[test]
    fn reopening_a_file_activates_the_existing_tab() {
        let directory = TempDir::new("tabs-dedup");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        application.open_document_in_new_tab(&first).unwrap();

        assert_eq!(application.tab_count(), 2);
        assert_eq!(application.active_tab(), 0);
        assert_eq!(text(&application), "A");
    }

    #[test]
    fn mouse_click_on_tab_row_switches_tabs() {
        let directory = TempDir::new("tabs-mouse");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        assert_eq!(application.active_tab(), 1);

        application.update(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));

        assert_eq!(application.active_tab(), 0);
        assert_eq!(text(&application), "A");
    }

    // ----- Watcher, conflito e remoção -----

    #[test]
    fn external_modification_without_local_edits_reloads_document() {
        let directory = TempDir::new("watcher-reload");
        let path = directory.join("a.txt");
        fs::write(&path, "original").unwrap();
        let mut application = app("");
        application.open_document(&path).unwrap();

        fs::write(&path, "externo").unwrap();
        application.on_idle();

        assert_eq!(text(&application), "externo");
        assert!(!application.has_conflict());
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("recarregado")
        );
    }

    #[test]
    fn external_modification_conflicts_and_requires_second_save() {
        let directory = TempDir::new("watcher-conflict");
        let path = directory.join("a.txt");
        fs::write(&path, "original").unwrap();
        let mut application = app("");
        application.open_document(&path).unwrap();

        application
            .view
            .set_cursor(&application.buffer, CharOffset(8))
            .unwrap();
        press(&mut application, KeyCode::Char('!'));
        application.on_idle();
        fs::write(&path, "externo").unwrap();
        application.on_idle();

        assert!(application.has_conflict());

        press_ctrl(&mut application, KeyCode::Char('s'));
        assert_eq!(fs::read_to_string(&path).unwrap(), "externo");
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("externamente")
        );

        press_ctrl(&mut application, KeyCode::Char('s'));
        assert_eq!(fs::read_to_string(&path).unwrap(), "original!");
        assert!(!application.has_conflict());
        assert!(!application.buffer().is_modified());
    }

    #[test]
    fn removed_file_is_reported_and_can_be_recreated_by_saving() {
        let directory = TempDir::new("watcher-removed");
        let path = directory.join("a.txt");
        fs::write(&path, "conteudo").unwrap();
        let mut application = app("");
        application.open_document(&path).unwrap();

        fs::remove_file(&path).unwrap();
        application.on_idle();

        assert!(application.is_missing());
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("removido")
        );

        press_ctrl(&mut application, KeyCode::Char('s'));
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "conteudo");
        assert!(!application.is_missing());
    }

    // ----- Sessão e recuperação -----

    #[test]
    fn session_restores_workspace_tabs_and_unsaved_content() {
        let directory = TempDir::new("session-restore");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let first = project.join("main.rs");
        let second = project.join("lib.rs");
        fs::write(&first, "fn main() {}").unwrap();
        fs::write(&second, "pub fn lib() {}").unwrap();
        let paths = isolated_paths(&config);

        let mut application = App::with_paths(80, 24, paths.clone());
        application.open_workspace(&project).unwrap();
        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        let end = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(end))
            .unwrap();
        press(&mut application, KeyCode::Char('!'));
        application.on_idle();
        application.persist_before_quit();
        drop(application);

        let restored = App::with_paths(80, 24, paths.clone());
        assert_eq!(
            restored.workspace().map(|workspace| workspace.root()),
            Some(fs::canonicalize(&project).unwrap().as_path())
        );
        assert_eq!(restored.tab_count(), 2);
        assert_eq!(restored.active_tab(), 1);
        assert_eq!(text(&restored), "pub fn lib() {}!");
        assert!(restored.is_recovered());

        let mut restored = restored;
        restored.save_document_to(&second).unwrap();
        restored.persist_before_quit();
        drop(restored);

        let clean = App::with_paths(80, 24, paths);
        assert_eq!(clean.tab_count(), 2);
        assert_eq!(text(&clean), "pub fn lib() {}!");
        assert!(!clean.is_recovered());
        assert_eq!(fs::read_to_string(&second).unwrap(), "pub fn lib() {}!");
    }

    #[test]
    fn untitled_recovery_is_restored_without_a_session_path() {
        let directory = TempDir::new("untitled-recovery");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = isolated_paths(&config);

        let mut application = App::with_paths(80, 24, paths.clone());
        for character in "rascunho".chars() {
            press(&mut application, KeyCode::Char(character));
        }
        application.on_idle();
        application.persist_before_quit();
        drop(application);

        let restored = App::with_paths(80, 24, paths);
        assert_eq!(restored.tab_count(), 1);
        assert_eq!(text(&restored), "rascunho");
        assert!(restored.is_recovered());
        assert!(restored.path().is_none());
    }

    // ----- Explorador e Workspace -----

    #[test]
    fn explorer_creates_opens_and_deletes_files() {
        let directory = TempDir::new("explorer-app");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer().visible());
        assert!(application.explorer_visible_width().is_some());

        application.open_explorer_prompt(PromptKind::NewFile);
        type_prompt(&mut application, "novo.txt");
        application.apply_action(Action::PromptConfirm);

        let created = fs::canonicalize(project.join("novo.txt")).unwrap();
        assert!(created.is_file());

        let index = application
            .explorer()
            .entries()
            .iter()
            .position(|entry| entry.path == created)
            .unwrap();
        application.explorer.select(index);
        application.update(Event::Key(key(KeyCode::Enter)));
        assert_eq!(application.path(), Some(created.as_path()));
        assert!(!application.explorer_focused());

        application.explorer_focused = true;
        application.explorer.select(index);
        application.update(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::SHIFT,
        )));

        assert_eq!(application.mode(), Mode::Confirm);
        assert_eq!(application.confirm().unwrap().kind(), ConfirmKind::Delete);
        assert!(application.confirm().unwrap().destructive_only());
        press(&mut application, KeyCode::Char('d'));

        assert!(!created.exists());
        assert_eq!(application.document_name(), "sem título");
    }

    #[test]
    fn explorer_rename_remaps_open_document() {
        let directory = TempDir::new("explorer-rename");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let path = project.join("a.txt");
        fs::write(&path, "conteudo").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        application.open_document_in_new_tab(&path).unwrap();
        let index = application
            .explorer()
            .entries()
            .iter()
            .position(|entry| entry.path == path)
            .unwrap();
        application.explorer.select(index);
        application.open_explorer_prompt(PromptKind::Rename);

        let prompt = application.prompt.as_mut().unwrap();
        prompt.input = "b.txt".to_owned();
        prompt.cursor = prompt.input.chars().count();
        application.apply_action(Action::PromptConfirm);

        let renamed = fs::canonicalize(project.join("b.txt")).unwrap();
        assert!(!path.exists());
        assert!(renamed.exists());
        assert_eq!(application.path(), Some(renamed.as_path()));
        assert_eq!(application.document_name(), "b.txt");
        assert_eq!(text(&application), "conteudo");
    }

    #[test]
    fn deletion_is_blocked_for_modified_open_documents() {
        let directory = TempDir::new("explorer-protection");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let path = project.join("a.txt");
        fs::write(&path, "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        application.open_document_in_new_tab(&path).unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(1))
            .unwrap();
        press(&mut application, KeyCode::Char('x'));
        application.on_idle();

        let index = application
            .explorer()
            .entries()
            .iter()
            .position(|entry| entry.path == path)
            .unwrap();
        application.explorer.select(index);
        application.explorer_focused = true;
        application.update(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::SHIFT,
        )));

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.confirm().is_none());
        assert!(path.exists());
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("modificados")
        );
    }

    #[test]
    fn explorer_sidebar_is_hidden_without_workspace_or_in_narrow_terminals() {
        let directory = TempDir::new("explorer-layout");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut application = app("");

        assert!(application.explorer_visible_width().is_none());
        application.open_workspace(&project).unwrap();
        assert!(application.explorer_visible_width().is_some());

        application.update(Event::Resize {
            width: 30,
            height: 10,
        });
        assert!(application.explorer_visible_width().is_none());
    }

    #[test]
    fn explorer_sidebar_resizes_with_keyboard_and_mouse_drag() {
        let directory = TempDir::new("explorer-resize");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        let initial = application.explorer().width();

        application.update(Event::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(application.explorer().width(), initial + 4);

        application.update(Event::Key(KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::CONTROL,
        )));
        assert_eq!(application.explorer().width(), initial);

        application.update(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 24,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(application.explorer().width(), 25);
        assert!(application.explorer_visible_width().is_some());
    }

    // ----- Tela inicial -----

    #[test]
    fn start_screen_lists_recent_projects_and_opens_selection() {
        let directory = TempDir::new("start-screen");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let paths = isolated_paths(&config);

        let store = RecentStore::new(paths.recent_file.clone());
        let mut projects = RecentProjects::default();
        projects.record("projeto", &project);
        store.save(&projects).unwrap();

        let mut application = App::with_paths(80, 24, paths);
        application.show_start_screen();
        assert_eq!(
            application
                .start_screen()
                .map(|screen| screen.items().len()),
            Some(1)
        );

        application.update(Event::Key(key(KeyCode::Enter)));

        assert!(application.start_screen().is_none());
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(fs::canonicalize(&project).unwrap().as_path())
        );
        assert!(application.explorer_focused());
    }

    #[test]
    fn escape_closes_start_screen_without_opening_workspace() {
        let directory = TempDir::new("start-screen-esc");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = isolated_paths(&config);
        let mut application = App::with_paths(80, 24, paths);

        application.show_start_screen();
        application.update(Event::Key(key(KeyCode::Esc)));

        assert!(application.start_screen().is_none());
        assert!(application.workspace().is_none());
    }
}
