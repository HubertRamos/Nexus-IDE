use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nexus_core::buffer::Buffer;
use nexus_core::buffer::Transaction;
use nexus_core::buffer::edit::BufferError;
use nexus_core::buffer::edit::{AppliedEdit, Edit, TextRange};
use nexus_core::buffer::position::CharOffset;
use nexus_core::buffer::{LineRange, MoveDirection};
use nexus_syntax::{LanguageId, SyntaxState};
use ratatui::layout::Rect;

use crate::diagnostics::{
    ProblemFilter, ProblemIdentity, ProblemItem, problem_counts, problem_items,
};
use crate::document::{Document, DocumentId, DocumentManager, TabInfo, detect_line_ending};
use crate::document_sync::{DiagnosticFreshness, DocumentSyncBridge, StoredDiagnostics, SyncSink};
use crate::event::Event;
use crate::explorer::{Explorer, ExplorerActivation};
use crate::files::{self, FileError};
use crate::input::{Action, ConfirmAction};
use crate::navigation::{
    NAVIGATION_HISTORY_LIMIT, NavigationContextState, NavigationHistory, NavigationHistoryEntry,
    NavigationLocation, NavigationOperationCompletion, NavigationOperationRegistry,
    NavigationOperationStatus, NavigationOriginContext, NavigationStaleReason, outcome_status,
};
use crate::paths::AppPaths;
use crate::picker::{
    FilePickerError, NativeFilePicker, PickerKind, PickerRequest, PickerSelection,
};
use crate::recent::{RecentEntry, RecentProjects, RecentStore};
use crate::recovery::{RECOVERY_VERSION, RecoveryManager, RecoveryRecord, now_seconds};
use crate::session::{Session, SessionStore};
use crate::theme::{ColorMode, ThemeState};
use crate::view::{Motion, TAB_WIDTH, View, char_width, editor_content_size};
use crate::watcher::{DetectedChange, FileFingerprint, FileWatcher, fingerprint};
use crate::workspace::Workspace;
use nexus_lsp::completion::CompletionItem;
use nexus_lsp::protocol::{NavigationMethod, NavigationTarget, Position};
use nexus_lsp::service::{
    CompletionOperationId, NavigationCompletion, NavigationOperationId, NavigationOutcome,
};

/// Colunas mínimas reservadas ao editor quando a sidebar está visível.
const MIN_EDITOR_WIDTH: u16 = 20;

/// Ciclos ociosos (250 ms cada) antes de uma mensagem informativa desaparecer.
const STATUS_INFO_TICKS: u8 = 12;

/// Mensagens de erro ficam um pouco mais tempo, mas nunca permanentemente.
const STATUS_ERROR_TICKS: u8 = 24;

/// Janela de tempo usada para reconhecer um duplo clique no Explorer.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(450);

/// Modo de interação da interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Editor,
    Prompt(PromptKind),
    Help,
    Confirm,
    ContextMenu,
    NavigationChooser,
}

/// Ações oferecidas pelo menu de contexto do Explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    Open,
    NewFile,
    NewDirectory,
    Rename,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMenuItem {
    pub action: ContextAction,
    pub label: &'static str,
}

/// Menu de contexto ancorado no item selecionado do Explorer.
#[derive(Debug, Clone)]
pub struct ContextMenu {
    items: Vec<ContextMenuItem>,
    selected: usize,
    anchor: (u16, u16),
    title: String,
}

impl ContextMenu {
    pub fn items(&self) -> &[ContextMenuItem] {
        &self.items
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Retângulo do menu, sempre dentro da área da interface.
    pub fn rect(&self, width: u16, height: u16) -> Rect {
        let menu_width = 24.min(width.max(1));
        let menu_height = (self.items.len() as u16 + 2).min(height.max(1));
        let x = self.anchor.0.min(width.saturating_sub(menu_width));
        let y = self.anchor.1.min(height.saturating_sub(menu_height + 1));

        Rect::new(x, y, menu_width, menu_height)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }

        let maximum = self.items.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, maximum as isize) as usize;
    }
}

/// Uma definição ou referência apresentada no seletor de navegação.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationChoice {
    pub target: NavigationTarget,
    pub file_name: String,
    pub relative_path: String,
    pub line: u32,
    pub column: u32,
    pub context: Option<String>,
}

/// Sugestões de completion atualmente apresentadas no editor.
#[derive(Debug, Clone)]
pub struct CompletionPopup {
    items: Vec<CompletionItem>,
    selected: usize,
}

impl CompletionPopup {
    pub fn items(&self) -> &[CompletionItem] { &self.items }
    pub fn selected(&self) -> usize { self.selected }

    fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() { self.selected = 0; return; }
        self.selected = (self.selected as isize + delta)
            .clamp(0, self.items.len().saturating_sub(1) as isize) as usize;
    }
}

struct PendingCompletion {
    operation_id: CompletionOperationId,
    uri: String,
    document_id: DocumentId,
    revision: u64,
}

/// Lista modal de destinos para navegações com mais de um resultado.
#[derive(Debug, Clone)]
pub struct NavigationChooser {
    origin: NavigationOriginContext,
    choices: Vec<NavigationChoice>,
    selected: usize,
}

impl NavigationChooser {
    pub fn choices(&self) -> &[NavigationChoice] {
        &self.choices
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn is_references(&self) -> bool {
        matches!(self.origin.method, NavigationMethod::References { .. })
    }

    pub fn title(&self) -> &'static str {
        if self.is_references() {
            "Referências"
        } else {
            match self.origin.method {
                NavigationMethod::Declaration => "Declarações",
                NavigationMethod::TypeDefinition => "Definições de Tipo",
                _ => "Definições",
            }
        }
    }

    /// Primeiro item visível para manter a seleção dentro da janela.
    pub fn offset(&self, rows: usize) -> usize {
        if rows == 0 || self.selected < rows {
            0
        } else {
            self.selected + 1 - rows
        }
    }

    /// Retângulo do popup, sempre dentro da área da interface.
    pub fn rect(&self, width: u16, height: u16) -> Rect {
        if width == 0 || height == 0 {
            return Rect::new(0, 0, 0, 0);
        }

        let popup_width = (width.saturating_mul(4) / 5).max(30).min(width);
        let desired_height = self.choices.len().saturating_add(2) as u16;
        let maximum_height = (height.saturating_mul(3) / 4).max(3);
        let popup_height = desired_height.min(maximum_height).min(height);
        let x = (width - popup_width) / 2;
        let y = (height - popup_height) / 2;

        Rect::new(x, y, popup_width, popup_height)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.choices.is_empty() {
            return;
        }

        let maximum = self.choices.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, maximum as isize) as usize;
    }
}

/// Posição e largura de uma aba na barra superior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabSlot {
    pub index: usize,
    pub start: u16,
    pub width: u16,
    /// Coluna do botão de fechamento.
    pub close: u16,
}

impl TabSlot {
    pub fn contains(self, column: u16) -> bool {
        column >= self.start && column < self.start.saturating_add(self.width)
    }

    pub fn close_contains(self, column: u16) -> bool {
        // O glifo fica na última célula; a célula anterior também fecha para
        // dar uma área de acerto mais generosa ao mouse.
        column >= self.close && column < self.start.saturating_add(self.width)
    }
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
    OpenWorkspace,
    SaveAs,
    NewFile,
    NewDirectory,
    Rename,
}

impl PromptKind {
    pub fn title(self) -> &'static str {
        match self {
            PromptKind::Open => "Abrir arquivo",
            PromptKind::OpenWorkspace => "Abrir projeto",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryDirection {
    Back,
    Forward,
}

pub struct App {
    documents: DocumentManager,
    /// Há pelo menos uma aba/documento aberto. O editor vazio inicial e o
    /// estado após fechar a última aba não contam como documento aberto.
    open_documents: bool,
    workspace: Option<Workspace>,
    workspace_generation: u64,
    explorer: Explorer,
    file_watcher: Option<FileWatcher>,
    recovery_manager: RecoveryManager,
    session_store: SessionStore,
    recent_store: RecentStore,
    paths: AppPaths,
    buffer: Buffer,
    document_id: DocumentId,
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
    /// Seletor nativo compilado e habilitado (desativado nos testes).
    native_picker: bool,
    /// Ambiente gráfico disponível para abrir o seletor nativo.
    picker_interactive: bool,
    /// Pedido de seleção nativa pendente para o loop de eventos.
    picker_request: Option<PickerRequest>,
    /// Explorador recebe as teclas de navegação.
    explorer_focused: bool,
    problems_open: bool,
    problems_focused: bool,
    problems_filter: ProblemFilter,
    problems_selection: Option<ProblemIdentity>,
    problems_selection_index: usize,
    last_problem_click: Option<(Instant, ProblemIdentity)>,
    /// Aba sob o ponteiro, usada para revelar o botão de fechamento.
    hovered_tab: Option<usize>,
    /// Último clique no Explorer, para detectar duplo clique.
    last_explorer_click: Option<(Instant, usize)>,
    /// Menu de contexto aberto para um item do Explorer.
    context_menu: Option<ContextMenu>,
    /// Arrasto ativo da borda da sidebar.
    sidebar_drag: bool,
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
    /// Tempo (em ciclos ociosos) desde a última mensagem operacional.
    status_ticks: u8,
    session: Option<EditKind>,
    theme: ThemeState,
    syntax: Option<SyntaxState>,
    /// Ponte para a sincronização LSP. Não inicia servidores e não depende de
    /// Ratatui; o sink é fornecido pela camada de integração.
    document_sync: DocumentSyncBridge,
    diagnostics: Vec<StoredDiagnostics>,
    completion_popup: Option<CompletionPopup>,
    pending_completion: Option<PendingCompletion>,
    completion_trigger: Option<(u8, Option<String>)>,
    navigation_operations: NavigationOperationRegistry,
    /// Operação de definição iniciada pela interface e ainda não aplicada.
    active_navigation: Option<NavigationOperationId>,
    /// Destinos aguardando escolha explícita do usuário.
    navigation_chooser: Option<NavigationChooser>,
    /// Navegações efetivamente aplicadas, com cursor de voltar/avançar.
    navigation_history: NavigationHistory,
    /// Evita abrir um documento externo ao workspace no LSP ao navegar.
    suppress_next_sync_open: bool,
}

struct NavigationDocumentSnapshot {
    uri: Option<String>,
    path: Option<PathBuf>,
    revision: u64,
    position: Option<Position>,
    editing: bool,
    active: bool,
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
        app.native_picker = true;
        app.picker_interactive = NativeFilePicker::interactive_available();

        if let Some(paths) = AppPaths::from_environment() {
            app.paths = paths.clone();
            app.explorer = Explorer::new(paths.root.clone());
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
        app.open_documents = true;
        app.path = Some(path.to_path_buf());
        app.document_name = document_name(path);
        app.syntax = LanguageId::from_path(path).map(|language| SyntaxState::new(language, text));
        app
    }

    fn with_theme(width: u16, height: u16, text: &str, theme: ThemeState) -> Self {
        let buffer = Buffer::from_text(text);
        let line_count = buffer.len_lines().unwrap_or(1);
        let open_documents = !text.is_empty();
        let header_height = if open_documents { 2 } else { 1 };
        let (cols, rows) = editor_content_size(width, height, header_height, line_count);

        let mut view = View::new();
        view.set_content_size(cols, rows);

        let documents = DocumentManager::new();
        let paths = AppPaths::default();
        let explorer = Explorer::new(paths.root.clone());
        let mut app = Self {
            documents,
            open_documents: !text.is_empty(),
            workspace: None,
            workspace_generation: 0,
            explorer,
            file_watcher: None,
            recovery_manager: RecoveryManager::new(paths.recovery_dir.clone()),
            session_store: SessionStore::new(paths.session_file.clone()),
            recent_store: RecentStore::new(paths.recent_file.clone()),
            paths,
            buffer,
            document_id: DocumentId::new(),
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
            native_picker: false,
            picker_interactive: false,
            picker_request: None,
            explorer_focused: false,
            problems_open: false,
            problems_focused: false,
            problems_filter: ProblemFilter::All,
            problems_selection: None,
            problems_selection_index: 0,
            last_problem_click: None,
            hovered_tab: None,
            last_explorer_click: None,
            context_menu: None,
            sidebar_drag: false,
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
            status_ticks: 0,
            session: None,
            theme,
            syntax: None,
            document_sync: DocumentSyncBridge::new(),
            diagnostics: Vec::new(),
            completion_popup: None,
            pending_completion: None,
            completion_trigger: None,
            navigation_operations: NavigationOperationRegistry::default(),
            active_navigation: None,
            navigation_chooser: None,
            navigation_history: NavigationHistory::new(NAVIGATION_HISTORY_LIMIT),
            suppress_next_sync_open: false,
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

    /// Snapshot atual de diagnósticos recebidos para documentos abertos.
    #[allow(dead_code)] // ponto de leitura usado pela próxima fase visual
    pub fn diagnostics(&self) -> &[StoredDiagnostics] {
        &self.diagnostics
    }

    pub fn completion_popup(&self) -> Option<&CompletionPopup> {
        self.completion_popup.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn set_diagnostics_for_tests(&mut self, diagnostics: Vec<StoredDiagnostics>) {
        self.diagnostics = diagnostics;
    }

    /// Estado de cursor/rolagem do documento ativo.
    pub fn view(&self) -> &View {
        &self.view
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[allow(dead_code)] // usado pelos testes de comportamento da TUI
    pub fn document_name(&self) -> &str {
        &self.document_name
    }

    #[allow(dead_code)] // usado pelos testes de comportamento da TUI
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

    pub fn has_open_documents(&self) -> bool {
        self.open_documents
    }

    pub fn hovered_tab(&self) -> Option<usize> {
        self.hovered_tab
    }

    pub fn context_menu(&self) -> Option<&ContextMenu> {
        self.context_menu.as_ref()
    }

    pub fn navigation_chooser(&self) -> Option<&NavigationChooser> {
        self.navigation_chooser.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn navigation_history(&self) -> &NavigationHistory {
        &self.navigation_history
    }

    pub fn can_navigate_back(&self) -> bool {
        self.navigation_history.can_go_back()
    }

    pub fn can_navigate_forward(&self) -> bool {
        self.navigation_history.can_go_forward()
    }

    pub fn navigation_back_steps(&self) -> usize {
        self.navigation_history.back_count()
    }

    pub fn navigation_forward_steps(&self) -> usize {
        self.navigation_history.forward_count()
    }

    pub fn theme(&self) -> &ThemeState {
        &self.theme
    }

    pub fn syntax(&self) -> Option<&SyntaxState> {
        self.syntax.as_ref()
    }

    /// Instala o destino dos eventos de sincronização LSP.
    ///
    /// A TUI apenas traduz eventos de buffer; iniciar e compartilhar
    /// servidores continua sendo responsabilidade do sink.
    #[allow(dead_code)]
    pub fn set_sync_sink(&mut self, sink: Box<dyn SyncSink>) {
        self.document_sync.set_sink(sink);
        let root = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf());
        self.document_sync.set_workspace(root.as_deref());
        // Um sink pode ser instalado depois que documentos já estão abertos
        // (por exemplo, ao retomar uma sessão). Reenvia o estado atual para
        // não deixar buffers sem `didOpen`.
        self.sync_all_open();
    }

    /// Agenda uma solicitação de completion para depois da sincronização da edição.
    fn schedule_completion(&mut self, trigger_kind: u8, trigger_character: Option<String>) {
        self.completion_popup = None;
        self.completion_trigger = Some((trigger_kind, trigger_character));
    }

    fn clear_completion(&mut self) {
        self.completion_popup = None;
        self.pending_completion = None;
        self.completion_trigger = None;
    }

    fn request_completion(&mut self, trigger_kind: u8, trigger_character: Option<String>) {
        let Some(path) = self.path.clone() else { return; };
        let Ok(text) = self.buffer.text() else { return; };
        let Ok(position) = nexus_lsp::utf16::position_from_offset(&text, self.view.cursor().0) else {
            return;
        };
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let Some(operation_id) = self.document_sync.request_completion(
            &path, position, trigger_kind, trigger_character,
        ) else { return; };
        self.pending_completion = Some(PendingCompletion {
            operation_id,
            uri,
            document_id: self.document_id,
            revision: self.buffer.revision(),
        });
    }

    fn poll_completion_results(&mut self) {
        for result in self.document_sync.drain_completion_results() {
            let Some(pending) = self.pending_completion.as_ref() else { continue; };
            if result.operation_id != pending.operation_id
                || result.uri != pending.uri
                || pending.document_id != self.document_id
                || pending.revision != self.buffer.revision()
                || self.path.as_deref().map(nexus_lsp::uri::path_to_uri).as_deref()
                    != Some(pending.uri.as_str())
            {
                continue;
            }
            self.pending_completion = None;
            if let Ok(Some(response)) = result.result {
                let items = response.items().to_vec();
                if !items.is_empty() {
                    self.completion_popup = Some(CompletionPopup { items, selected: 0 });
                    self.needs_redraw = true;
                }
            }
        }
    }

    fn handle_completion_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        if self.completion_popup.is_none()
            || key.kind == ratatui::crossterm::event::KeyEventKind::Release
            || !key.modifiers.is_empty()
        {
            return false;
        }
        use ratatui::crossterm::event::KeyCode;
        match key.code {
            KeyCode::Up => {
                if let Some(popup) = self.completion_popup.as_mut() { popup.move_selection(-1); }
                self.needs_redraw = true;
                true
            }
            KeyCode::Down => {
                if let Some(popup) = self.completion_popup.as_mut() { popup.move_selection(1); }
                self.needs_redraw = true;
                true
            }
            KeyCode::Enter | KeyCode::Tab => { self.accept_completion(); true }
            KeyCode::Esc => { self.clear_completion(); self.needs_redraw = true; true }
            _ => { self.completion_popup = None; false }
        }
    }

    fn accept_completion(&mut self) {
        let Some(popup) = self.completion_popup.take() else { return; };
        let Some(item) = popup.items.get(popup.selected) else { return; };
        let insertion = if item.insert_text_format == Some(2) {
            item.label.clone()
        } else {
            item.insert_text.clone().unwrap_or_else(|| item.label.clone())
        };
        let cursor = self.view.cursor();
        let line_start = match self.buffer.line_start(self.buffer.line_of_char(cursor).unwrap_or(0)) {
            Ok(start) => start,
            Err(_) => return,
        };
        let mut start = cursor.0;
        while start > line_start.0 {
            let previous = match self.buffer.char_at(CharOffset(start - 1)) {
                Ok(Some(character)) => character,
                _ => break,
            };
            if previous.is_alphanumeric() || previous == '_' { start -= 1; } else { break; }
        }
        let range = TextRange::new(CharOffset(start), cursor);
        self.commit_session();
        match self.buffer.apply_transaction(vec![Edit::new(range, insertion.clone())]) {
            Ok(Some(transaction)) => {
                self.apply_syntax_transaction(&transaction);
                self.sync_change_active(&transaction);
                self.view.clear_selection();
                self.set_cursor_after_edit(CharOffset(start + insertion.chars().count()));
            }
            Ok(None) => {}
            Err(error) => self.report_buffer_error("não foi possível aplicar a sugestão", &error),
        }
        self.pending_completion = None;
        self.completion_trigger = None;
        self.needs_redraw = true;
    }

    /// Coleta conclusões assíncronas de navegação para o consumidor da camada
    /// de aplicação, sem acoplar o protocolo ao renderer.
    #[allow(dead_code)]
    pub fn take_navigation_results(&mut self) -> Vec<NavigationOperationCompletion> {
        self.poll_navigation_results();
        self.navigation_operations.take_completed()
    }

    /// Captura o documento ativo e enfileira navegação sem aguardar o servidor.
    #[allow(dead_code)]
    pub fn request_navigation(
        &mut self,
        method: NavigationMethod,
    ) -> Option<NavigationOperationId> {
        self.commit_session();
        let path = self.path.clone()?;
        let language_id = crate::document_sync::language_id_for_path(&path)?;
        let text = self.buffer.text().ok()?;
        let position = nexus_lsp::utf16::position_from_offset(&text, self.view.cursor().0).ok()?;
        let workspace = self.document_sync.workspace_for(&path);
        let session_id = self
            .document_sync
            .current_session_id(&workspace, language_id);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let operation_id = self.document_sync.request_navigation(
            workspace.clone(),
            language_id,
            method,
            uri.clone(),
            position,
        )?;
        let context = NavigationOriginContext {
            operation_id,
            document_id: self.document_id,
            method,
            workspace,
            workspace_generation: self.workspace_generation,
            session_id,
            language_id: language_id.to_owned(),
            uri,
            position,
            document_revision: self.buffer.revision(),
            location: self.capture_navigation_location(),
        };
        if !self.navigation_operations.register(context) {
            self.document_sync.cancel_navigation(operation_id);
            return None;
        }
        Some(operation_id)
    }

    #[allow(dead_code)]
    pub fn navigation_context(
        &self,
        operation_id: NavigationOperationId,
    ) -> Option<&NavigationOriginContext> {
        self.navigation_operations.context(operation_id)
    }

    #[allow(dead_code)]
    pub fn cancel_navigation(&mut self, operation_id: NavigationOperationId) -> bool {
        if !self.navigation_operations.cancel(operation_id) {
            return false;
        }
        if self.active_navigation == Some(operation_id) {
            self.active_navigation = None;
        }
        self.document_sync.cancel_navigation(operation_id);
        true
    }

    fn poll_navigation_results(&mut self) {
        for completion in self.document_sync.drain_navigation_results() {
            self.accept_navigation_completion(completion);
        }

        // A conclusão da navegação interativa é aplicada imediatamente.
        // Outras operações continuam disponíveis via `take_navigation_results`.
        if let Some(operation_id) = self.active_navigation
            && let Some(completion) = self.navigation_operations.take_completed_for(operation_id)
        {
            self.active_navigation = None;
            self.apply_navigation_completion(completion);
        }
    }

    fn accept_navigation_completion(&mut self, completion: NavigationCompletion) {
        let operation_id = completion.context.operation_id;
        let Some(origin) = self.navigation_operations.context(operation_id).cloned() else {
            // Canceladas, consumidas ou expiradas na TUI: uma resposta tardia
            // não pode recriar o registro.
            return;
        };

        let service_context = completion.context;
        if service_context.operation_id != origin.operation_id
            || service_context.method != origin.method
            || service_context.workspace != origin.workspace
            || service_context.language_id != origin.language_id
            || service_context.uri != origin.uri
            || service_context.position != origin.position
        {
            self.navigation_operations.complete(
                operation_id,
                Some(service_context),
                NavigationContextState::ContextMismatch,
                outcome_status(completion.outcome),
            );
            return;
        }

        let Some(document) = self.navigation_document_snapshot(origin.document_id) else {
            self.navigation_operations.complete(
                operation_id,
                Some(service_context),
                NavigationContextState::OriginDocumentMissing,
                outcome_status(completion.outcome),
            );
            return;
        };

        let current_session = self
            .document_sync
            .current_session_id(&origin.workspace, &origin.language_id);
        let session_replaced = origin
            .session_id
            .is_some_and(|session| service_context.session_id != Some(session))
            || current_session.is_some_and(|session| service_context.session_id != Some(session))
            || (current_session.is_none()
                && matches!(&completion.outcome, NavigationOutcome::Result(_)));
        let mut stale = Vec::new();
        if origin.workspace_generation != self.workspace_generation
            || document
                .path
                .as_deref()
                .is_some_and(|path| self.document_sync.workspace_for(path) != origin.workspace)
        {
            stale.push(NavigationStaleReason::WorkspaceChanged);
        }
        if !document.active {
            stale.push(NavigationStaleReason::ActiveDocumentChanged);
        }
        if document.uri.as_deref() != Some(origin.uri.as_str()) {
            stale.push(NavigationStaleReason::DocumentUriChanged);
        }
        if document.revision != origin.document_revision || document.editing {
            stale.push(NavigationStaleReason::DocumentEdited);
        }
        if document.position != Some(origin.position) {
            stale.push(NavigationStaleReason::CursorMoved);
        }
        let context_state = if session_replaced {
            NavigationContextState::SessionReplaced
        } else if stale.is_empty() {
            NavigationContextState::Current
        } else {
            NavigationContextState::Stale(stale)
        };
        let status = match completion.outcome {
            NavigationOutcome::Result(result) => NavigationOperationStatus::Response { result },
            outcome => outcome_status(outcome),
        };
        self.navigation_operations.complete(
            operation_id,
            Some(service_context),
            context_state,
            status,
        );
    }

    /// Inicia uma navegação interativa sem aguardar o servidor no laço principal.
    fn begin_editor_navigation(&mut self, method: NavigationMethod) {
        if !self.open_documents || self.path.is_none() {
            self.set_info(no_navigation_document_message(method));
            return;
        }

        // Uma nova solicitação da interface substitui a anterior. Operações
        // independentes continuam intocadas.
        if let Some(previous) = self.active_navigation.take() {
            self.cancel_navigation(previous);
            let _ = self.navigation_operations.take_completed_for(previous);
        }

        let Some(operation_id) = self.request_navigation(method) else {
            self.set_error(request_navigation_failure_message(method));
            return;
        };

        self.active_navigation = Some(operation_id);
        self.set_info(navigation_pending_message(method));
    }

    fn go_to_definition(&mut self) {
        self.begin_editor_navigation(NavigationMethod::Definition);
    }

    fn go_to_declaration(&mut self) {
        self.begin_editor_navigation(NavigationMethod::Declaration);
    }

    fn go_to_type_definition(&mut self) {
        self.begin_editor_navigation(NavigationMethod::TypeDefinition);
    }

    fn find_references(&mut self) {
        self.begin_editor_navigation(NavigationMethod::References {
            include_declaration: true,
        });
    }

    fn apply_navigation_completion(&mut self, completion: NavigationOperationCompletion) {
        let NavigationOperationCompletion {
            origin,
            context_state,
            status,
            ..
        } = completion;

        let result = navigation_result_label(origin.method);
        match context_state {
            NavigationContextState::Current => {}
            NavigationContextState::Stale(_) => {
                self.set_info(format!(
                    "{result} descartada: o documento mudou durante a busca"
                ));
                return;
            }
            NavigationContextState::SessionReplaced | NavigationContextState::ContextMismatch => {
                self.set_error(format!(
                    "{result} descartada: o contexto do servidor LSP mudou"
                ));
                return;
            }
            NavigationContextState::OriginDocumentMissing => {
                self.set_info(format!(
                    "{result} descartada: o documento de origem foi fechado"
                ));
                return;
            }
        }

        let references = is_references_method(origin.method);
        match status {
            NavigationOperationStatus::Response { result } => match result {
                nexus_lsp::protocol::NavigationResult::Destinations(targets) => {
                    let targets = if references {
                        normalize_reference_targets(targets)
                    } else {
                        normalize_navigation_targets(targets)
                    };
                    match targets.len() {
                        0 => self.set_info(no_navigation_results_message(origin.method)),
                        1 if !references => {
                            let target = targets.into_iter().next().expect("um destino");
                            self.apply_navigation_target(&origin, target);
                        }
                        _ => self.open_navigation_chooser(origin, targets),
                    }
                }
                nexus_lsp::protocol::NavigationResult::Empty => {
                    self.set_info(no_navigation_results_message(origin.method));
                }
                nexus_lsp::protocol::NavigationResult::Unsupported => {
                    self.set_info(unsupported_navigation_message(origin.method));
                }
            },
            status => self.report_navigation_status(&status, origin.method),
        }
    }

    fn report_navigation_status(
        &mut self,
        status: &NavigationOperationStatus,
        method: NavigationMethod,
    ) {
        let action = navigation_action_label(method);
        match status {
            NavigationOperationStatus::Unsupported => {
                self.set_info(unsupported_navigation_message(method));
            }
            NavigationOperationStatus::Cancelled => {
                self.set_info(navigation_cancelled_message(method));
            }
            NavigationOperationStatus::TimedOut { duration } => {
                self.set_error(format!(
                    "tempo esgotado ao {action} ({} ms)",
                    duration.as_millis()
                ));
            }
            NavigationOperationStatus::SessionEnded => {
                self.set_error(format!("servidor LSP indisponível para {action}"));
            }
            NavigationOperationStatus::TransportFailure { .. } => {
                self.set_error(format!(
                    "falha de comunicação com o servidor LSP ao {action}"
                ));
            }
            NavigationOperationStatus::ServerFailure { .. } => {
                self.set_error(server_navigation_failure_message(method));
            }
            NavigationOperationStatus::ProtocolFailure { .. } => {
                self.set_error(format!("resposta inválida do servidor LSP ao {action}"));
            }
            NavigationOperationStatus::Failed { .. } => {
                self.set_error(format!("não foi possível {action}"));
            }
            NavigationOperationStatus::Response { .. } => {}
        }
    }

    fn open_navigation_chooser(
        &mut self,
        origin: NavigationOriginContext,
        targets: Vec<NavigationTarget>,
    ) {
        let count = targets.len();
        let plural = navigation_result_plural(origin.method);
        let mut texts = BTreeMap::new();
        let choices: Vec<_> = targets
            .into_iter()
            .map(|target| self.navigation_choice(target, &mut texts))
            .collect();

        self.navigation_chooser = Some(NavigationChooser {
            origin,
            choices,
            selected: 0,
        });
        self.mode = Mode::NavigationChooser;
        self.set_info(format!(
            "{count} {plural} encontradas; escolha um destino e confirme"
        ));
    }

    fn navigation_choice(
        &self,
        target: NavigationTarget,
        texts: &mut BTreeMap<PathBuf, Option<String>>,
    ) -> NavigationChoice {
        let path = nexus_lsp::uri::uri_to_path(&target.uri);
        let (file_name, relative_path) = match path.as_deref() {
            Some(path) => (
                crate::document::document_name(path),
                self.workspace_relative_label(path),
            ),
            None => (target.uri.clone(), target.uri.clone()),
        };
        let context = path.as_deref().and_then(|path| {
            self.navigation_line_context_cached(path, target.selection_range.start.line, texts)
        });

        NavigationChoice {
            file_name,
            relative_path,
            line: target.selection_range.start.line,
            column: target.selection_range.start.character,
            context,
            target,
        }
    }

    fn workspace_relative_label(&self, path: &Path) -> String {
        let canonical = canonical_or_owned(path);
        self.workspace
            .as_ref()
            .and_then(|workspace| canonical.strip_prefix(workspace.root()).ok())
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_else(|| canonical.to_string_lossy().into_owned())
    }

    fn navigation_line_context_cached(
        &self,
        path: &Path,
        line: u32,
        texts: &mut BTreeMap<PathBuf, Option<String>>,
    ) -> Option<String> {
        let text = self.navigation_text_for_path_cached(path, texts)?;
        let content = text.lines().nth(line as usize)?.trim();
        if content.is_empty() {
            return None;
        }

        let mut excerpt: String = content.chars().take(80).collect();
        if content.chars().count() > 80 {
            excerpt.push('…');
        }
        Some(excerpt)
    }

    /// Lê o conteúdo uma única vez por arquivo ao montar listas grandes.
    fn navigation_text_for_path_cached(
        &self,
        path: &Path,
        texts: &mut BTreeMap<PathBuf, Option<String>>,
    ) -> Option<String> {
        let key = canonical_or_owned(path);
        texts
            .entry(key)
            .or_insert_with(|| self.navigation_text_for_path(path))
            .clone()
    }

    fn navigation_text_for_path(&self, path: &Path) -> Option<String> {
        let target = canonical_or_owned(path);

        if self
            .path
            .as_deref()
            .is_some_and(|current| canonical_or_owned(current) == target)
        {
            return self.buffer.text().ok();
        }

        if let Some(document) = self.documents.inactive().iter().find(|document| {
            document
                .path
                .as_deref()
                .is_some_and(|candidate| canonical_or_owned(candidate) == target)
        }) {
            return document.buffer.text().ok();
        }

        files::read_document(path).ok()
    }

    /// Abre ou reutiliza a aba do destino e posiciona o cursor na seleção.
    fn apply_navigation_target(
        &mut self,
        origin: &NavigationOriginContext,
        target: NavigationTarget,
    ) -> bool {
        let result_label = navigation_result_label(origin.method);
        let Some(path) = nexus_lsp::uri::uri_to_path(&target.uri) else {
            self.set_error(format!(
                "a {result_label} aponta para uma URI de arquivo inválida"
            ));
            return false;
        };

        let existing = self.navigation_tab_index(&path);
        if let Some(index) = existing {
            if index != self.documents.active_index() && !self.switch_tab(index) {
                self.set_error(format!("não foi possível ativar a aba da {result_label}"));
                return false;
            }
        } else {
            // Um arquivo externo ao workspace é aberto apenas como documento,
            // sem criar um novo workspace LSP nem iniciar outro servidor.
            let external = self
                .workspace
                .as_ref()
                .is_some_and(|workspace| !canonical_or_owned(&path).starts_with(workspace.root()));
            self.suppress_next_sync_open = external;
            let result = self.open_document_in_new_tab(&path);
            self.suppress_next_sync_open = false;
            if let Err(error) = result {
                self.set_error(format!("não foi possível abrir a {result_label}: {error}"));
                return false;
            }
        }

        let Some(offset) = self.navigation_cursor_offset(&target) else {
            self.set_error(format!(
                "a {result_label} aponta para uma posição fora do documento"
            ));
            return false;
        };

        self.commit_session();
        self.view.clear_selection();
        self.sync_viewport();
        if let Err(error) = self.view.set_cursor(&self.buffer, offset) {
            self.report_buffer_error(
                &format!("não foi possível posicionar o cursor na {result_label}"),
                &error,
            );
            return false;
        }
        self.ensure_visible();
        self.needs_redraw = true;

        let label = self.workspace_relative_label(&path);
        self.set_info(format!(
            "{result_label}: {}:{}:{}",
            label,
            target.selection_range.start.line + 1,
            target.selection_range.start.character + 1
        ));

        let target_location = self.capture_navigation_location();
        self.navigation_history.record(NavigationHistoryEntry {
            method: origin.method,
            origin: origin.location.clone(),
            target: target_location,
        });

        true
    }

    fn navigation_tab_index(&mut self, path: &Path) -> Option<usize> {
        let active = self.take_active_document();
        let index = self.documents.index_of_path(&active, path);
        self.install_active_document(active);
        index
    }

    fn navigate_history(&mut self, direction: HistoryDirection) -> bool {
        let destination = match direction {
            HistoryDirection::Back => self.navigation_history.back_target().cloned(),
            HistoryDirection::Forward => self.navigation_history.forward_target().cloned(),
        };
        let Some(location) = destination else {
            self.set_info(match direction {
                HistoryDirection::Back => "não há posição anterior no histórico de navegação",
                HistoryDirection::Forward => "não há próxima posição no histórico de navegação",
            });
            return false;
        };

        if !self.restore_navigation_location(&location) {
            return false;
        }

        let committed = match direction {
            HistoryDirection::Back => self.navigation_history.commit_back(),
            HistoryDirection::Forward => self.navigation_history.commit_forward(),
        };
        debug_assert!(committed, "o destino foi obtido antes de restaurar");

        self.set_info(match direction {
            HistoryDirection::Back => "histórico: posição anterior",
            HistoryDirection::Forward => "histórico: próxima posição",
        });
        true
    }

    fn restore_navigation_location(&mut self, location: &NavigationLocation) -> bool {
        if let Err(message) = self.activate_history_document(location) {
            self.set_error(message);
            return false;
        }

        if !self.buffer.is_consistent() {
            self.set_error(
                "não foi possível restaurar a localização: documento em recuperação pendente",
            );
            return false;
        }

        self.commit_session();
        self.sync_viewport();

        if let Err(error) = self.view.restore_state(
            &self.buffer,
            location.cursor,
            location.anchor,
            location.top_line,
            location.left_col,
        ) {
            self.report_buffer_error(
                "não foi possível restaurar a localização de navegação",
                &error,
            );
            return false;
        }

        if let Err(error) = self.view.ensure_visible(&self.buffer) {
            self.report_buffer_error("não foi possível ajustar o viewport da navegação", &error);
            return false;
        }

        self.overwrite_pending = false;
        self.refresh_watcher();
        self.needs_redraw = true;
        true
    }

    /// Reutiliza a aba pelo `DocumentId`, depois pelo caminho e, por último,
    /// reabre o arquivo sem trocar o workspace atual.
    fn activate_history_document(&mut self, location: &NavigationLocation) -> Result<(), String> {
        if self.document_id == location.document_id {
            return Ok(());
        }

        if let Some(index) = self.history_inactive_tab_index(location.document_id) {
            return self.activate_history_tab(index);
        }

        let path = location
            .path
            .clone()
            .or_else(|| nexus_lsp::uri::uri_to_path(&location.uri));
        let Some(path) = path else {
            return Err(
                "não foi possível restaurar a localização: o documento não está mais aberto"
                    .to_owned(),
            );
        };

        if let Some(index) = self.navigation_tab_index(&path) {
            return self.activate_history_tab(index);
        }

        let external = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| !canonical_or_owned(&path).starts_with(workspace.root()));
        self.suppress_next_sync_open = external;
        let result = self.open_document_in_new_tab(&path);
        self.suppress_next_sync_open = false;

        result.map_err(|error| format!("não foi possível restaurar a localização: {error}"))
    }

    fn activate_history_tab(&mut self, index: usize) -> Result<(), String> {
        if index == self.documents.active_index() {
            return Ok(());
        }

        if !self.navigation_tab_is_consistent(index) {
            return Err(
                "não foi possível restaurar a localização: documento em recuperação pendente"
                    .to_owned(),
            );
        }

        if self.switch_tab(index) {
            Ok(())
        } else {
            Err("não foi possível ativar a aba da localização de navegação".to_owned())
        }
    }

    fn history_inactive_tab_index(&self, document_id: DocumentId) -> Option<usize> {
        self.documents
            .inactive()
            .iter()
            .position(|document| document.id == document_id)
            .map(|physical| {
                if physical < self.documents.active_index() {
                    physical
                } else {
                    physical + 1
                }
            })
    }

    fn navigation_tab_is_consistent(&self, index: usize) -> bool {
        if index == self.documents.active_index() {
            return self.buffer.is_consistent();
        }

        let physical = if index < self.documents.active_index() {
            index
        } else {
            index.saturating_sub(1)
        };
        self.documents
            .inactive()
            .get(physical)
            .is_some_and(|document| document.buffer.is_consistent())
    }

    /// Prefere `targetSelectionRange`; usa `targetRange` apenas como fallback.
    fn navigation_cursor_offset(&self, target: &NavigationTarget) -> Option<CharOffset> {
        let text = self.buffer.text().ok()?;
        for position in [target.selection_range.start, target.target_range.start] {
            if let Ok(offset) = nexus_lsp::utf16::offset_from_position(&text, position) {
                return Some(CharOffset(offset));
            }
        }
        None
    }

    /// Captura documento, cursor, seleção, viewport e contexto de workspace.
    fn capture_navigation_location(&self) -> NavigationLocation {
        let cursor = self.view.cursor();
        let anchor = self.view.selection_range().map(|range| {
            if range.start == cursor {
                range.end
            } else {
                range.start
            }
        });

        NavigationLocation {
            document_id: self.document_id,
            uri: self
                .path
                .as_deref()
                .map(nexus_lsp::uri::path_to_uri)
                .unwrap_or_default(),
            path: self.path.clone(),
            cursor,
            anchor,
            top_line: self.view.top_line(),
            left_col: self.view.left_col(),
            workspace: self
                .workspace
                .as_ref()
                .map(|workspace| workspace.root().to_string_lossy().into_owned()),
            workspace_generation: self.workspace_generation,
            revision: self.buffer.revision(),
        }
    }

    fn move_navigation_choice(&mut self, delta: isize) {
        if let Some(chooser) = self.navigation_chooser.as_mut() {
            chooser.move_selection(delta);
            self.needs_redraw = true;
        }
    }

    fn activate_navigation_choice(&mut self) {
        let Some(chooser) = self.navigation_chooser.take() else {
            return;
        };
        self.mode = Mode::Editor;
        self.needs_redraw = true;

        let Some(choice) = chooser.choices.get(chooser.selected) else {
            self.set_error(no_navigation_selection_message(chooser.origin.method));
            return;
        };
        self.apply_navigation_target(&chooser.origin, choice.target.clone());
    }

    fn cancel_navigation_chooser(&mut self) {
        if self.navigation_chooser.take().is_some() {
            self.mode = Mode::Editor;
            self.set_info("navegação cancelada");
        }
    }

    fn navigation_document_snapshot(
        &self,
        document_id: DocumentId,
    ) -> Option<NavigationDocumentSnapshot> {
        if self.document_id == document_id {
            let uri = self.path.as_deref().map(nexus_lsp::uri::path_to_uri);
            let text = self.buffer.text().ok()?;
            let position = nexus_lsp::utf16::position_from_offset(&text, self.view.cursor().0).ok();
            return Some(NavigationDocumentSnapshot {
                uri,
                revision: self.buffer.revision(),
                position,
                editing: self.buffer.has_active_transaction(),
                active: true,
                path: self.path.clone(),
            });
        }

        let document = self
            .documents
            .inactive()
            .iter()
            .find(|document| document.id == document_id)?;
        let uri = document.path.as_deref().map(nexus_lsp::uri::path_to_uri);
        let text = document.buffer.text().ok()?;
        let position = nexus_lsp::utf16::position_from_offset(&text, document.view.cursor().0).ok();
        Some(NavigationDocumentSnapshot {
            uri,
            revision: document.buffer.revision(),
            position,
            editing: document.buffer.has_active_transaction(),
            active: false,
            path: document.path.clone(),
        })
    }

    fn invalidate_navigation_for_uri(&mut self, path: &Path) {
        let uri = nexus_lsp::uri::path_to_uri(path);
        for operation_id in self.navigation_operations.invalidate_uri(&uri) {
            self.document_sync.cancel_navigation(operation_id);
        }
    }

    /// Envia `didOpen` para todas as abas abertas.
    ///
    /// A operação é idempotente na ponte: reabrir o mesmo caminho com a mesma
    /// linguagem não duplica eventos.
    fn sync_all_open(&mut self) {
        let mut documents = Vec::new();

        if let Some(path) = self.path.clone() {
            documents.push((path, self.buffer.text().unwrap_or_default()));
        }
        for document in self.documents.inactive() {
            if let Some(path) = document.path.clone() {
                documents.push((path, document.buffer.text().unwrap_or_default()));
            }
        }

        for (path, text) in documents {
            self.document_sync.open(&path, &text);
        }
    }

    /// Fecha no LSP todos os documentos rastreados e encerra o serviço.
    ///
    /// Chamado ao encerrar o editor para que os servidores recebam `didClose`
    /// antes do `shutdown` da thread de serviço. É idempotente.
    pub fn shutdown_lsp(&mut self) {
        for operation_id in self.navigation_operations.cancel_all() {
            self.document_sync.cancel_navigation(operation_id);
        }
        self.document_sync.shutdown();
    }

    fn sync_open_active(&mut self) {
        if self.suppress_next_sync_open {
            self.suppress_next_sync_open = false;
            return;
        }

        let Some(path) = self.path.clone() else {
            return;
        };
        let text = self.buffer.text().unwrap_or_default();
        self.document_sync.open(&path, &text);
    }

    fn sync_change_active(&mut self, transaction: &Transaction) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let text = self.buffer.text().unwrap_or_default();
        self.document_sync.change(&path, transaction, &text);
        self.mark_diagnostics_stale(&path);
    }

    fn sync_resync_active(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let text = self.buffer.text().unwrap_or_default();
        self.document_sync.resync(&path, &text);
        self.mark_diagnostics_stale(&path);
    }

    fn sync_save_active(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let text = self.buffer.text().unwrap_or_default();
        self.document_sync.save(&path, &text);
    }

    fn sync_close_path(&mut self, path: Option<&Path>) {
        if let Some(path) = path {
            self.invalidate_navigation_for_uri(path);
            self.document_sync.close(path);
            let uri = nexus_lsp::uri::path_to_uri(path);
            self.diagnostics
                .retain(|diagnostic| diagnostic.key.uri != uri);
        }
    }

    fn mark_diagnostics_stale(&mut self, path: &Path) {
        let uri = nexus_lsp::uri::path_to_uri(path);
        for diagnostic in self
            .diagnostics
            .iter_mut()
            .filter(|diagnostic| diagnostic.key.uri == uri)
        {
            diagnostic.freshness = DiagnosticFreshness::PotentiallyStale;
        }
    }

    fn clear_diagnostics_for_path(&mut self, path: &Path) {
        let uri = nexus_lsp::uri::path_to_uri(path);
        self.diagnostics
            .retain(|diagnostic| diagnostic.key.uri != uri);
    }

    /// Move o documento ativo para uma representação avulsa.
    ///
    /// O `DocumentManager` guarda apenas as abas inativas; esta conversão
    /// permite usar as operações dele sem duplicar a lógica de abas no `App`.
    fn take_active_document(&mut self) -> Document {
        Document {
            id: self.document_id,
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
        self.document_id = document.id;
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

    #[allow(dead_code)] // usado pelos testes de comportamento da TUI
    pub fn tab_count(&self) -> usize {
        self.documents.len()
    }

    /// Distribui as abas abertas na largura disponível.
    ///
    /// A mesma geometria é usada pela renderização e pelo mouse, evitando
    /// áreas de clique desalinhadas. Cada aba reserva uma célula para o botão
    /// de fechamento, mesmo quando ele está oculto.
    pub fn tab_slots(&self, available: u16) -> Vec<TabSlot> {
        if !self.open_documents || available == 0 {
            return Vec::new();
        }

        let mut slots = Vec::new();
        let mut start = 0u16;

        for tab in self.tabs() {
            if start >= available {
                break;
            }

            let name_width = tab
                .name
                .chars()
                .map(|character| char_width(character, 0))
                .sum::<usize>()
                .min(18) as u16;
            let desired = (name_width + 4).clamp(7, 22);
            let width = desired.min(available - start);

            if width < 5 {
                break;
            }

            slots.push(TabSlot {
                index: tab.index,
                start,
                width,
                close: start + width - 2,
            });
            start += width;
        }

        slots
    }

    /// Índice da linha do Explorer correspondente a uma linha da sidebar.
    ///
    /// A lista é rolada para manter o item selecionado visível; o cálculo é
    /// compartilhado pela renderização e pelo mouse.
    pub fn explorer_row_offset(&self) -> usize {
        let rows = self.explorer_rows();
        self.explorer
            .selected()
            .saturating_sub(rows.saturating_sub(1))
    }

    fn explorer_rows(&self) -> usize {
        let body = self
            .height
            .saturating_sub(self.header_height())
            .saturating_sub(1);
        body.saturating_sub(2) as usize
    }

    #[allow(dead_code)] // usado pelos testes de comportamento da TUI
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

    pub fn problems_focused(&self) -> bool {
        self.problems_focused
    }
    pub fn problems_filter(&self) -> ProblemFilter {
        self.problems_filter
    }
    pub fn problem_items(&self) -> Vec<ProblemItem> {
        problem_items(&self.diagnostics, self.problems_filter)
    }
    pub fn problem_counts(&self) -> crate::diagnostics::ProblemCounts {
        problem_counts(&self.diagnostics)
    }
    pub fn selected_problem(&self) -> Option<ProblemIdentity> {
        self.problems_selection.clone()
    }
    pub fn problems_selection_offset(&self) -> usize {
        let inner_height = self.problems_panel_height().saturating_sub(1) as usize;
        let help_rows = usize::from(inner_height >= 4);
        let rows = inner_height.saturating_sub(help_rows);
        self.problems_selection_index
            .saturating_sub(rows.saturating_sub(1))
    }
    pub fn problems_panel_height(&self) -> u16 {
        let body = self
            .height
            .saturating_sub(self.header_height())
            .saturating_sub(1);
        if !self.problems_open || body < 3 {
            0
        } else {
            6.min(body.saturating_sub(1))
        }
    }

    fn normalize_problem_selection(&mut self) {
        let items = self.problem_items();
        if let Some(selected) = &self.problems_selection
            && let Some(index) = items.iter().position(|item| &item.identity == selected)
        {
            self.problems_selection_index = index;
            return;
        }
        if items.is_empty() {
            self.problems_selection = None;
            self.problems_selection_index = 0;
        } else {
            self.problems_selection_index = self.problems_selection_index.min(items.len() - 1);
            self.problems_selection = Some(items[self.problems_selection_index].identity.clone());
        }
    }

    fn set_problems_filter(&mut self, filter: ProblemFilter) {
        self.problems_filter = filter;
        self.normalize_problem_selection();
        self.needs_redraw = true;
    }

    fn move_problem_selection(&mut self, delta: isize) {
        let items = self.problem_items();
        if items.is_empty() {
            self.problems_selection = None;
            return;
        }
        let index = (self.problems_selection_index as isize + delta)
            .clamp(0, items.len() as isize - 1) as usize;
        self.problems_selection_index = index;
        self.problems_selection = Some(items[index].identity.clone());
        self.needs_redraw = true;
    }

    fn toggle_problems(&mut self, focus: bool) {
        if self.problems_open {
            self.problems_open = false;
            self.problems_focused = false;
        } else {
            self.problems_open = true;
            self.problems_focused = focus;
            if focus {
                self.explorer_focused = false;
            }
            self.normalize_problem_selection();
        }
        self.sync_viewport();
        self.ensure_visible();
        self.needs_redraw = true;
    }

    fn focus_problems(&mut self) {
        self.explorer_focused = false;
        if !self.problems_open {
            self.toggle_problems(true);
        } else {
            self.problems_focused = true;
            self.needs_redraw = true;
        }
    }

    fn activate_problem(&mut self) {
        self.normalize_problem_selection();
        let Some(item) = self
            .problem_items()
            .into_iter()
            .find(|item| Some(&item.identity) == self.problems_selection.as_ref())
        else {
            return;
        };
        let opened = self
            .tabs()
            .iter()
            .any(|tab| tab.path.as_deref() == Some(item.path.as_path()));
        if !opened {
            let root = Path::new(&item.identity.key.workspace);
            let canonical_root = std::fs::canonicalize(root);
            let canonical_file = std::fs::canonicalize(&item.path);
            let (Ok(canonical_root), Ok(canonical_file)) = (canonical_root, canonical_file) else {
                self.set_info("arquivo do diagnóstico não está disponível");
                return;
            };
            if !canonical_root.is_dir()
                || !canonical_file.is_file()
                || !canonical_file.starts_with(&canonical_root)
            {
                self.set_info("arquivo do diagnóstico não está disponível");
                return;
            }
            if self
                .workspace
                .as_ref()
                .is_none_or(|workspace| workspace.root() != canonical_root)
                && let Err(error) = self.open_workspace(&canonical_root)
            {
                self.set_info(format!("não foi possível abrir o workspace: {error}"));
                return;
            }
        }
        if let Err(error) = self.open_document_in_new_tab(&item.path) {
            self.set_info(format!("não foi possível abrir {}", item.file_label));
            let _ = error;
            return;
        }
        let position = nexus_lsp::protocol::Position {
            line: item.identity.start_line,
            character: item.identity.start_character,
        };
        if let Ok(text) = self.buffer.text()
            && let Ok(offset) = nexus_lsp::utf16::offset_from_position(&text, position)
        {
            let _ = self.view.set_cursor(&self.buffer, CharOffset(offset));
            self.view.clear_selection();
            self.sync_viewport();
            self.ensure_visible();
        }
        self.explorer_focused = false;
        self.problems_focused = false;
        self.needs_redraw = true;
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
    #[allow(dead_code)] // usado pelos testes de comportamento da TUI
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

        if let Some((trigger_kind, trigger_character)) = self.completion_trigger.take() {
            self.request_completion(trigger_kind, trigger_character);
        }

        self.age_status();
        self.check_external_changes();
        self.persist_recovery();
        self.report_sync_notices();
    }

    /// Exibe falhas assíncronas do LSP sem bloquear o laço de eventos.
    fn report_sync_notices(&mut self) {
        if let Some(notice) = self.document_sync.drain_notices().into_iter().next() {
            self.set_error(notice);
        }
        self.poll_navigation_results();
        self.poll_completion_results();
        self.diagnostics = self.document_sync.drain_diagnostics();
        self.normalize_problem_selection();
    }

    /// Mensagens operacionais expiram sozinhas; a interface não deve manter
    /// confirmações antigas ocupando a barra inferior.
    fn age_status(&mut self) {
        if self.status.is_none() {
            self.status_ticks = 0;
            return;
        }

        self.status_ticks = self.status_ticks.saturating_add(1);
        let limit = match self.status.as_ref().map(|message| message.kind) {
            Some(MessageKind::Error) => STATUS_ERROR_TICKS,
            Some(MessageKind::Info) => STATUS_INFO_TICKS,
            None => 0,
        };

        if self.status_ticks >= limit {
            self.clear_status();
        }
    }

    /// Descarta a mensagem atual; chamado em toda interação relevante.
    fn clear_status(&mut self) {
        self.status_ticks = 0;
        if self.status.take().is_some() {
            self.needs_redraw = true;
        }
    }

    /// Aplica um evento já recebido ao estado da aplicação.
    pub fn update(&mut self, event: Event) {
        match event {
            Event::Key(key) => {
                if key.kind != ratatui::crossterm::event::KeyEventKind::Release {
                    self.clear_status();
                }

                // Ctrl+B, Ctrl+O, Ctrl+Shift+O, Ctrl+Q e Ctrl+Shift+Q são
                // globais enquanto não há um diálogo modal capturando a entrada.
                if self.accepts_global_shortcuts()
                    && let Some(action) = crate::input::global_action(key)
                {
                    match action {
                        Action::Quit => {
                            self.request_quit();
                            return;
                        }
                        Action::CloseTab
                            if self.start_screen.is_none()
                                && matches!(self.mode, Mode::Editor | Mode::Help) =>
                        {
                            self.close_current_tab();
                            return;
                        }
                        Action::ToggleExplorer => {
                            self.toggle_explorer();
                            return;
                        }
                        Action::ToggleProblems => {
                            self.toggle_problems(true);
                            return;
                        }
                        Action::FocusProblems => {
                            self.focus_problems();
                            return;
                        }
                        Action::NavigateBack
                            if self.start_screen.is_none()
                                && matches!(self.mode, Mode::Editor | Mode::Help) =>
                        {
                            self.navigate_history(HistoryDirection::Back);
                            return;
                        }
                        Action::NavigateForward
                            if self.start_screen.is_none()
                                && matches!(self.mode, Mode::Editor | Mode::Help) =>
                        {
                            self.navigate_history(HistoryDirection::Forward);
                            return;
                        }
                        Action::OpenFilePicker | Action::OpenWorkspacePicker
                            if matches!(self.mode, Mode::Editor | Mode::Help) =>
                        {
                            self.begin_picker(if action == Action::OpenFilePicker {
                                PickerKind::Files
                            } else {
                                PickerKind::Directory
                            });
                            return;
                        }
                        _ => {}
                    }
                }

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

                if self.mode == Mode::ContextMenu {
                    self.handle_context_menu_key(key);
                    return;
                }

                if self.mode == Mode::NavigationChooser {
                    if let Some(action) = crate::input::navigation_choice_action(key) {
                        self.apply_navigation_choice_action(action);
                    }
                    return;
                }

                if self.problems_focused && self.mode == Mode::Editor {
                    if let Some(action) = crate::input::problems_action(key) {
                        use crate::input::ProblemsAction;
                        match action {
                            ProblemsAction::Up => self.move_problem_selection(-1),
                            ProblemsAction::Down => self.move_problem_selection(1),
                            ProblemsAction::PageUp => self.move_problem_selection(-5),
                            ProblemsAction::PageDown => self.move_problem_selection(5),
                            ProblemsAction::Activate => self.activate_problem(),
                            ProblemsAction::Close => self.toggle_problems(false),
                            ProblemsAction::FocusEditor => {
                                self.explorer_focused = false;
                                self.problems_focused = false;
                                self.needs_redraw = true;
                            }
                            ProblemsAction::Filter(filter) => self.set_problems_filter(filter),
                        }
                        return;
                    }
                    // O foco do painel não deve permitir que teclas de filtro
                    // ou navegação alterem o documento.
                    return;
                }

                if self.mode == Mode::Editor
                    && key.kind != ratatui::crossterm::event::KeyEventKind::Release
                    && key.modifiers.contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
                    && key.code == ratatui::crossterm::event::KeyCode::Char(' ')
                {
                    self.commit_session();
                    self.request_completion(1, None);
                    return;
                }

                if self.mode == Mode::Editor && self.handle_completion_key(key) {
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
                    Mode::Confirm | Mode::ContextMenu | Mode::NavigationChooser => None,
                };

                if let Some(action) = action {
                    self.apply_action(action);
                }
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize { width, height } => {
                self.width = width;
                self.height = height;
                if self.explorer.visible() && self.explorer_available_width().is_none() {
                    // A sidebar deixou de caber: ela some junto com o foco
                    // para não capturar teclas invisivelmente.
                    self.apply_explorer_state(false);
                }
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
            Mode::Confirm | Mode::ContextMenu | Mode::NavigationChooser => {}
        }
    }

    fn apply_help_action(&mut self, action: Action) {
        match action {
            Action::ToggleProblems => self.toggle_problems(false),
            Action::FocusProblems => self.focus_problems(),
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
        if matches!(action, Action::Save | Action::SaveAsPrompt) && !self.open_documents {
            self.set_info("nenhum documento aberto");
            return;
        }

        if matches!(
            action,
            Action::InsertChar(_) | Action::InsertTab | Action::InsertNewline
        ) {
            self.ensure_document_open();
        }

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
            Action::InsertTab => self.insert_tab(),
            Action::OutdentSelection => self.outdent_selection(),
            Action::DuplicateLines => self.duplicate_lines(),
            Action::MoveLineUp => self.move_lines(MoveDirection::Up),
            Action::MoveLineDown => self.move_lines(MoveDirection::Down),
            Action::InsertNewline => self.insert_newline(),
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
            Action::OpenFilePicker => {
                self.begin_picker(PickerKind::Files);
            }
            Action::OpenWorkspacePicker => {
                self.begin_picker(PickerKind::Directory);
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
            Action::GoToDefinition => self.go_to_definition(),
            Action::GoToDeclaration => self.go_to_declaration(),
            Action::GoToTypeDefinition => self.go_to_type_definition(),
            Action::FindReferences => self.find_references(),
            Action::NavigateBack => {
                self.navigate_history(HistoryDirection::Back);
            }
            Action::NavigateForward => {
                self.navigate_history(HistoryDirection::Forward);
            }
            Action::ToggleHelp => {
                self.mode = Mode::Help;
                self.needs_redraw = true;
            }
            Action::ToggleExplorer => {
                self.toggle_explorer();
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
            | Action::ToggleProblems
            | Action::FocusProblems
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

    fn apply_navigation_choice_action(&mut self, action: crate::input::NavigationChoiceAction) {
        use crate::input::NavigationChoiceAction;
        match action {
            NavigationChoiceAction::Up => self.move_navigation_choice(-1),
            NavigationChoiceAction::Down => self.move_navigation_choice(1),
            NavigationChoiceAction::PageUp => self.move_navigation_choice(-5),
            NavigationChoiceAction::PageDown => self.move_navigation_choice(5),
            NavigationChoiceAction::Activate => self.activate_navigation_choice(),
            NavigationChoiceAction::Cancel => self.cancel_navigation_chooser(),
        }
    }

    fn move_cursor(&mut self, motion: Motion, select: bool) {
        self.clear_completion();
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
        if character.is_alphanumeric() || character == '_' {
            self.schedule_completion(1, None);
        } else if character == '.' {
            self.schedule_completion(2, Some(character.to_string()));
        } else {
            self.clear_completion();
        }
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
                        self.sync_change_active(transaction);
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

    fn insert_newline(&mut self) {
        let text = match self.newline_with_indentation() {
            Ok(text) => text,
            Err(error) => {
                self.report_buffer_error("não foi possível calcular a indentação", &error);
                return;
            }
        };

        self.insert_text(&text, EditKind::Insert);
    }

    /// Nova linha com a indentação da linha atual.
    ///
    /// Quando o ponto de edição está dentro da indentação existente, a
    /// indentação não é repetida: o restante da linha já carrega os espaços ou
    /// a tabulação. Linhas sem conteúdo não recebem espaços extras.
    fn newline_with_indentation(&self) -> Result<String, BufferError> {
        let at = self
            .view
            .selection_range()
            .map(|range| range.start)
            .unwrap_or_else(|| self.view.cursor());
        let line = self.buffer.line_of_char(at)?;
        let start = self.buffer.line_start(line)?;
        let text = self.buffer.line_text(line)?;
        let indent: String = text
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .collect();
        let indent_len = indent.chars().count();

        let mut newline = self.line_ending.clone();

        if !indent.is_empty() && at.0 >= start.0 + indent_len {
            newline.push_str(&indent);
        }

        Ok(newline)
    }

    fn insert_tab(&mut self) {
        if self.view.has_selection() {
            self.indent_selection();
        } else {
            self.insert_text("\t", EditKind::Insert);
        }
    }

    fn selected_or_current_lines(&self) -> Result<LineRange, BufferError> {
        match self.view.selection_range() {
            Some(range) => self.buffer.selection_line_range(range),
            None => self.buffer.line_range_at(self.view.cursor()),
        }
    }

    fn indent_selection(&mut self) {
        if !self.buffer.is_consistent() {
            self.report_recovery();
            return;
        }

        let lines = match self.selected_or_current_lines() {
            Ok(lines) => lines,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar as linhas", &error);
                return;
            }
        };

        self.commit_session();

        match self.buffer.indent_lines(lines, "\t") {
            Ok(Some(transaction)) => self.apply_line_transaction(&transaction),
            Ok(None) => {}
            Err(error) => {
                self.report_buffer_error("não foi possível indentar as linhas", &error);
            }
        }
    }

    fn outdent_selection(&mut self) {
        if !self.buffer.is_consistent() {
            self.report_recovery();
            return;
        }

        let lines = match self.selected_or_current_lines() {
            Ok(lines) => lines,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar as linhas", &error);
                return;
            }
        };

        self.commit_session();

        match self.buffer.outdent_lines(lines, TAB_WIDTH) {
            Ok(Some(transaction)) => self.apply_line_transaction(&transaction),
            Ok(None) => {}
            Err(error) => {
                self.report_buffer_error("não foi possível desindentar as linhas", &error);
            }
        }
    }

    fn duplicate_lines(&mut self) {
        if !self.buffer.is_consistent() {
            self.report_recovery();
            return;
        }

        let lines = match self.selected_or_current_lines() {
            Ok(lines) => lines,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar as linhas", &error);
                return;
            }
        };

        self.commit_session();

        match self.buffer.duplicate_lines(lines) {
            Ok(Some(transaction)) => {
                self.apply_syntax_transaction(&transaction);
                self.sync_change_active(&transaction);
                let inserted = transaction
                    .edits()
                    .iter()
                    .map(|edit| edit.inserted_text.chars().count())
                    .sum::<usize>();
                self.shift_view_after_insertion(inserted);
            }
            Ok(None) => {}
            Err(error) => {
                self.report_buffer_error("não foi possível duplicar as linhas", &error);
            }
        }
    }

    fn move_lines(&mut self, direction: MoveDirection) {
        if !self.buffer.is_consistent() {
            self.report_recovery();
            return;
        }

        let lines = match self.selected_or_current_lines() {
            Ok(lines) => lines,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar as linhas", &error);
                return;
            }
        };

        let cursor = self.view.cursor();
        let selection = self.view.selection_range();
        let anchor = selection.map(|range| {
            if cursor == range.start {
                range.end
            } else {
                range.start
            }
        });

        let cursor_position = match self.line_column(cursor) {
            Ok(position) => position,
            Err(error) => {
                self.report_buffer_error("não foi possível localizar o cursor", &error);
                return;
            }
        };
        let anchor_position = match anchor {
            Some(offset) => match self.line_column(offset) {
                Ok(position) => Some(position),
                Err(error) => {
                    self.report_buffer_error("não foi possível localizar a seleção", &error);
                    return;
                }
            },
            None => None,
        };

        self.commit_session();

        match self.buffer.move_lines(lines, direction) {
            Ok(Some(outcome)) => {
                self.apply_syntax_transaction(&outcome.transaction);
                self.sync_change_active(&outcome.transaction);

                let new_cursor = match self.offset_for_line_column(
                    outcome.map_line(cursor_position.0),
                    cursor_position.1,
                ) {
                    Ok(offset) => offset,
                    Err(error) => {
                        self.report_buffer_error("não foi possível ajustar o cursor", &error);
                        return;
                    }
                };
                let new_anchor = match anchor_position {
                    Some((line, column)) => {
                        match self
                            .offset_for_line_column(outcome.map_line(line), column)
                        {
                            Ok(offset) => Some(offset),
                            Err(error) => {
                                self.report_buffer_error(
                                    "não foi possível ajustar a seleção",
                                    &error,
                                );
                                return;
                            }
                        }
                    }
                    None => None,
                };

                self.sync_viewport();
                self.needs_redraw = true;

                if let Some(anchor) = new_anchor {
                    if let Err(error) =
                        self.view.set_selection(&self.buffer, anchor, new_cursor)
                    {
                        self.report_buffer_error(
                            "não foi possível restaurar a seleção",
                            &error,
                        );
                    }
                } else {
                    self.view.clear_selection();
                    if let Err(error) = self.view.set_cursor(&self.buffer, new_cursor) {
                        self.report_buffer_error("não foi possível ajustar o cursor", &error);
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.report_buffer_error("não foi possível mover as linhas", &error);
            }
        }
    }

    fn apply_line_transaction(&mut self, transaction: &Transaction) {
        self.apply_syntax_transaction(transaction);
        self.sync_change_active(transaction);

        let cursor = self.view.cursor();
        let selection = self.view.selection_range();
        let anchor = selection.map(|range| {
            if cursor == range.start {
                range.end
            } else {
                range.start
            }
        });

        let new_cursor = map_offset_through_transaction(cursor, transaction);
        let new_anchor = anchor.map(|offset| map_offset_through_transaction(offset, transaction));

        self.sync_viewport();
        self.needs_redraw = true;

        if let Some(anchor) = new_anchor {
            if let Err(error) = self.view.set_selection(&self.buffer, anchor, new_cursor) {
                self.report_buffer_error("não foi possível restaurar a seleção", &error);
            }
        } else {
            self.view.clear_selection();
            if let Err(error) = self.view.set_cursor(&self.buffer, new_cursor) {
                self.report_buffer_error("não foi possível ajustar o cursor", &error);
            }
        }
    }

    fn shift_view_after_insertion(&mut self, inserted: usize) {
        let cursor = self.view.cursor();
        let selection = self.view.selection_range();
        let anchor = selection.map(|range| {
            if cursor == range.start {
                range.end
            } else {
                range.start
            }
        });

        let new_cursor = CharOffset(cursor.0 + inserted);
        let new_anchor = anchor.map(|offset| CharOffset(offset.0 + inserted));

        self.sync_viewport();
        self.needs_redraw = true;

        if let Some(anchor) = new_anchor {
            if let Err(error) = self.view.set_selection(&self.buffer, anchor, new_cursor) {
                self.report_buffer_error("não foi possível restaurar a seleção", &error);
            }
        } else {
            self.view.clear_selection();
            if let Err(error) = self.view.set_cursor(&self.buffer, new_cursor) {
                self.report_buffer_error("não foi possível ajustar o cursor", &error);
            }
        }
    }

    fn line_column(&self, offset: CharOffset) -> Result<(usize, usize), BufferError> {
        let line = self.buffer.line_of_char(offset)?;
        let start = self.buffer.line_start(line)?;
        Ok((line, offset.0 - start.0))
    }

    fn offset_for_line_column(
        &self,
        line: usize,
        column: usize,
    ) -> Result<CharOffset, BufferError> {
        let start = self.buffer.line_start(line)?;
        let end = self.buffer.line_end(line)?;
        Ok(CharOffset((start.0 + column).min(end.0)))
    }

    fn backspace(&mut self) {
        self.schedule_completion(1, None);
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
        self.clear_completion();
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
                    self.sync_change_active(transaction);
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
        self.sync_resync_active();
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
            Ok(transaction) => {
                if let Some(transaction) = &transaction {
                    self.sync_change_active(transaction);
                }
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

        let rows_height = self.height.saturating_sub(self.problems_panel_height());
        let (cols, rows) =
            editor_content_size(self.width, rows_height, self.header_height(), line_count);
        self.view.set_content_size(cols, rows);
    }

    fn ensure_visible(&mut self) {
        if let Err(error) = self.view.ensure_visible(&self.buffer) {
            self.report_buffer_error("não foi possível ajustar o viewport", &error);
        }
    }

    fn open_prompt(&mut self, kind: PromptKind) {
        let initial = match kind {
            PromptKind::Open | PromptKind::OpenWorkspace => String::new(),
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

    /// Inicia o seletor nativo ou recorre ao prompt manual de caminho.
    ///
    /// A escolha é feita antes de qualquer suspensão do terminal. Sem ambiente
    /// gráfico, o usuário continua no Nexus com o mecanismo de entrada manual
    /// já existente, sem deixar o terminal desconfigurado.
    fn begin_picker(&mut self, kind: PickerKind) {
        if self.confirm.is_some() {
            return;
        }

        // A ajuda é uma sobreposição: abrir o seletor devolve o controle ao
        // editor, evitando que o diálogo termine com a ajuda ainda ativa.
        if self.mode == Mode::Help {
            self.mode = Mode::Editor;
        }

        self.commit_session();

        if self.native_picker && self.picker_interactive {
            self.picker_request = Some(PickerRequest::new(kind, self.picker_start_directory()));
            self.needs_redraw = true;
            return;
        }

        let prompt = match kind {
            PickerKind::Files => PromptKind::Open,
            PickerKind::Directory => PromptKind::OpenWorkspace,
        };

        // O prompt precisa ser visível; a tela inicial não pode cobri-lo.
        self.start_screen = None;
        self.open_prompt(prompt);

        if self.native_picker {
            self.set_info("seletor gráfico indisponível; informe o caminho manualmente");
        }
    }

    /// Retira o pedido de seleção nativa pendente, se houver.
    ///
    /// O loop de eventos usa este método para suspender o terminal, executar o
    /// diálogo e devolver o resultado à aplicação.
    pub fn take_picker_request(&mut self) -> Option<PickerRequest> {
        self.picker_request.take()
    }

    /// Conclui um pedido de seleção.
    ///
    /// `None` representa cancelamento: nenhum documento, workspace ou mensagem
    /// é alterado. Um resultado válido é aplicado ao estado atual.
    pub fn complete_picker(&mut self, selection: Option<PickerSelection>) {
        self.needs_redraw = true;

        if let Some(selection) = selection {
            self.apply_picker_selection(selection);
        }
    }

    /// Informa que o seletor nativo falhou e oferece o prompt manual.
    pub fn report_picker_error(&mut self, kind: PickerKind, error: &FilePickerError) {
        let prompt = match kind {
            PickerKind::Files => PromptKind::Open,
            PickerKind::Directory => PromptKind::OpenWorkspace,
        };

        self.start_screen = None;
        self.open_prompt(prompt);
        self.set_error(format!("{error}; informe o caminho manualmente"));
    }

    fn apply_picker_selection(&mut self, selection: PickerSelection) {
        self.start_screen = None;

        match selection {
            PickerSelection::Files(paths) => {
                let mut first_error = None;

                for path in paths {
                    if let Err(error) = self.open_document_in_new_tab(&path)
                        && first_error.is_none()
                    {
                        first_error = Some(error.to_string());
                    }
                }

                if let Some(error) = first_error {
                    self.set_error(error);
                }
            }
            PickerSelection::Directory(path) => {
                if let Err(error) = self.open_workspace(&path) {
                    self.set_error(error.to_string());
                }
            }
        }

        self.needs_redraw = true;
    }

    /// Diretório inicial do diálogo, quando há um contexto útil.
    fn picker_start_directory(&self) -> Option<PathBuf> {
        if let Some(parent) = self.path.as_deref().and_then(Path::parent) {
            return Some(parent.to_path_buf());
        }

        self.workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf())
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
            PromptKind::Open | PromptKind::OpenWorkspace | PromptKind::SaveAs => {
                return self.open_prompt(kind);
            }
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
            PromptKind::OpenWorkspace => {
                if prompt.input().is_empty() {
                    self.prompt = Some(prompt);
                    self.set_error(FileError::EmptyPath.to_string());
                    return;
                }

                let path = PathBuf::from(prompt.input());
                match self.open_workspace(&path) {
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
            PromptKind::Open | PromptKind::OpenWorkspace | PromptKind::SaveAs => unreachable!(),
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
        let previous_recovery = self.recovery_id.clone();
        let previous_path = self.path.clone();
        self.install_active_document(document);
        if let Some(id) = previous_recovery {
            let _ = self.recovery_manager.remove(&id);
        }
        // A aba anterior foi descartada por esta abertura. Se o caminho não
        // continua visível em outra aba, o servidor precisa receber `didClose`.
        if let Some(previous) = previous_path.as_deref()
            && self.path.as_deref().is_none_or(|current| {
                canonical_for_compare(current) != canonical_for_compare(previous)
            })
            && !self.documents.inactive().iter().any(|document| {
                document.path.as_deref().is_some_and(|candidate| {
                    canonical_for_compare(candidate) == canonical_for_compare(previous)
                })
            })
        {
            self.sync_close_path(Some(previous));
        }
        self.open_documents = true;
        self.recovery_id = self.path.as_deref().map(RecoveryManager::id_for_path);
        self.conflict = false;
        self.overwrite_pending = false;
        self.view.reset();
        self.sync_viewport();
        self.ensure_visible();
        self.refresh_watcher();
        self.sync_open_active();
        self.set_info("arquivo aberto");
        self.persist_session();

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
        self.open_documents = true;
        self.recovery_id = self.path.as_deref().map(RecoveryManager::id_for_path);
        self.conflict = false;
        self.overwrite_pending = false;
        self.sync_viewport();
        self.ensure_visible();
        self.refresh_watcher();
        self.sync_open_active();
        self.set_info("arquivo aberto");
        self.persist_session();

        Ok(())
    }

    fn is_pristine_untitled(&self) -> bool {
        self.path.is_none()
            && !self.recovered
            && !self.buffer.is_modified()
            && self.documents.len() == 1
            && self.buffer.is_empty().unwrap_or(false)
    }

    /// O primeiro texto digitado no estado vazio cria um documento real.
    ///
    /// Sem isso, o buffer existiria mas a interface continuaria mostrando o
    /// estado vazio e a aba não apareceria.
    fn ensure_document_open(&mut self) {
        if self.open_documents {
            return;
        }

        self.open_documents = true;
        self.untitled_sequence += 1;
        self.recovery_id = Some(RecoveryManager::new_untitled_id(self.untitled_sequence));
        self.needs_redraw = true;
    }

    /// Salva o documento no caminho informado.
    ///
    /// A sessão de edição é concluída antes da leitura do texto. O estado de
    /// salvamento só é atualizado depois que a escrita é confirmada.
    fn save_document_to(&mut self, path: &Path) -> Result<(), DocumentError> {
        self.commit_session();
        let previous_path = self.path.clone();

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
        match previous_path {
            Some(previous) if previous != canonical => {
                self.document_sync.rename(&previous, &canonical, &text);
                self.clear_diagnostics_for_path(&previous);
            }
            Some(_) => self.sync_save_active(),
            None => self.document_sync.open(&canonical, &text),
        }
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
        } else {
            self.close_tab_now(index);
        }
    }

    fn close_tab_now(&mut self, index: usize) {
        if index == self.documents.active_index() {
            let closed_recovery = self.recovery_id.clone();
            let closed_path = self.path.clone();
            let last =
                self.with_active_document(|document, manager| manager.close_active(document));
            self.sync_close_path(closed_path.as_deref());

            if let Some(id) = closed_recovery {
                let _ = self.recovery_manager.remove(&id);
            }

            if !last {
                // O manager já instalou um documento vazio.
                self.untitled_sequence += 1;
                self.recovery_id = Some(RecoveryManager::new_untitled_id(self.untitled_sequence));
                self.open_documents = false;
            } else {
                self.open_documents = true;
                if self.recovery_id.is_none() {
                    self.recovery_id = self.path.as_deref().map(RecoveryManager::id_for_path);
                }
            }

            self.overwrite_pending = false;
            self.sync_viewport();
            self.ensure_visible();
            self.refresh_watcher();
            self.needs_redraw = true;
            self.set_info("aba fechada");
        } else if let Some(document) = self.documents.take_inactive(index) {
            self.sync_close_path(document.path.as_deref());
            if let Some(id) = document.recovery_id {
                let _ = self.recovery_manager.remove(&id);
            }
            self.needs_redraw = true;
            self.set_info("aba fechada");
        }

        self.persist_session();
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

        let open_paths: Vec<_> = self
            .path
            .iter()
            .chain(
                self.documents
                    .inactive()
                    .iter()
                    .filter_map(|document| document.path.as_ref()),
            )
            .cloned()
            .collect();
        for path in &open_paths {
            self.invalidate_navigation_for_uri(path);
        }
        self.with_active_document(|document, manager| manager.reset(document));
        self.document_sync.close_all();
        self.diagnostics.clear();
        self.untitled_sequence += 1;
        self.recovery_id = Some(RecoveryManager::new_untitled_id(self.untitled_sequence));
        self.open_documents = false;
        self.conflict = false;
        self.overwrite_pending = false;
        self.sync_viewport();
        self.ensure_visible();
        self.refresh_watcher();
        self.needs_redraw = true;
        self.set_info("todas as abas foram fechadas");
        self.persist_session();
        true
    }

    /// Abre um diretório como Workspace e atualiza explorador e recentes.
    pub fn open_workspace(&mut self, path: &Path) -> Result<(), crate::workspace::WorkspaceError> {
        let workspace = Workspace::open(path)?;
        let root = workspace.root().to_path_buf();
        if self.workspace.as_ref().map(|current| current.root()) != Some(root.as_path()) {
            self.workspace_generation = self.workspace_generation.saturating_add(1);
        }
        self.explorer.set_root(&root);
        self.workspace = Some(workspace);
        self.document_sync.set_workspace(Some(&root));
        self.apply_explorer_state(true);
        self.record_recent_workspace();
        self.set_info("Workspace aberto");
        Ok(())
    }

    /// Fecha o Workspace ativo sem encerrar os documentos abertos.
    ///
    /// A ponte reatribui cada documento à sua chave efetiva (o diretório pai
    /// quando não há raiz ativa), enviando `didClose` ao contexto antigo e
    /// reabrindo apenas o que mudou de servidor.
    #[allow(dead_code)]
    pub fn close_workspace(&mut self) {
        if self.workspace.take().is_none() {
            return;
        }

        self.workspace_generation = self.workspace_generation.saturating_add(1);

        self.document_sync.set_workspace(None);
        self.apply_explorer_state(false);
        self.persist_session();
        self.set_info("Workspace fechado");
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
    /// O Explorer é dono de todo input de navegação e digitação enquanto está
    /// focado. Retorna `false` apenas para eventos que devem continuar no
    /// editor/atalhos: teclas com `Ctrl` (atalhos globais e de edição) e a
    /// tecla de ajuda. Qualquer outro evento é consumido, mesmo sem ação
    /// definida, para que uma letra não caia no editor e crie texto.
    fn handle_explorer_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

        if key.kind == KeyEventKind::Release {
            return true;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
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
            KeyCode::F(10) => {
                self.open_context_menu();
                return true;
            }
            KeyCode::Esc => {
                self.explorer_focused = false;
                self.set_info("foco no editor");
                return true;
            }
            // F1 abre a ajuda; F3 é o atalho alternativo para o seletor de
            // projeto. Ambos continuam válidos com o Explorer focado.
            KeyCode::F(1) | KeyCode::F(3) | KeyCode::F(12) => return false,
            // Letras, Tab, Backspace e demais teclas sem função no Explorer
            // são ignoradas aqui, sem chegar ao editor.
            _ => return true,
        }

        self.needs_redraw = true;
        true
    }

    /// Abre o menu de contexto para o item selecionado do Explorer.
    fn open_context_menu(&mut self) {
        if self.workspace.is_none() {
            self.set_error("nenhum Workspace aberto");
            return;
        }

        let Some(entry) = self.explorer.selected_entry() else {
            self.set_error("selecione um item no explorador");
            return;
        };

        let title = entry.name.clone();
        let items = vec![
            ContextMenuItem {
                action: ContextAction::Open,
                label: "Abrir",
            },
            ContextMenuItem {
                action: ContextAction::NewFile,
                label: "Novo arquivo",
            },
            ContextMenuItem {
                action: ContextAction::NewDirectory,
                label: "Nova pasta",
            },
            ContextMenuItem {
                action: ContextAction::Rename,
                label: "Renomear",
            },
            ContextMenuItem {
                action: ContextAction::Delete,
                label: "Excluir",
            },
        ];

        let body_top = self.header_height();
        let sidebar = self.explorer_visible_width().unwrap_or(0);
        let row = body_top.saturating_add(1).saturating_add(
            self.explorer
                .selected()
                .saturating_sub(self.explorer_row_offset()) as u16,
        );

        self.context_menu = Some(ContextMenu {
            items,
            selected: 0,
            anchor: (sidebar, row),
            title,
        });
        self.explorer_focused = true;
        self.mode = Mode::ContextMenu;
        self.needs_redraw = true;
    }

    fn close_context_menu(&mut self) {
        self.context_menu = None;
        self.mode = Mode::Editor;
        self.needs_redraw = true;
    }

    /// Teclas do menu de contexto. O menu é modal e não deixa `Ctrl+Q`
    /// fechar uma aba por engano.
    fn handle_context_menu_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyEventKind};

        if key.kind == KeyEventKind::Release {
            return;
        }

        match key.code {
            KeyCode::Up => {
                if let Some(menu) = self.context_menu.as_mut() {
                    menu.move_selection(-1);
                }
                self.needs_redraw = true;
            }
            KeyCode::Down => {
                if let Some(menu) = self.context_menu.as_mut() {
                    menu.move_selection(1);
                }
                self.needs_redraw = true;
            }
            KeyCode::Enter => self.activate_context_menu(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => self.close_context_menu(),
            _ => {}
        }
    }

    fn activate_context_menu(&mut self) {
        let Some(menu) = self.context_menu.take() else {
            return;
        };

        let action = menu
            .items()
            .get(menu.selected())
            .map(|item| item.action)
            .unwrap_or(ContextAction::Open);

        self.mode = Mode::Editor;
        self.needs_redraw = true;

        match action {
            ContextAction::Open => match self.explorer.activate_selected() {
                Some(ExplorerActivation::OpenFile(path)) => {
                    match self.open_document_in_new_tab(&path) {
                        Ok(()) => self.explorer_focused = false,
                        Err(error) => self.set_error(error.to_string()),
                    }
                }
                Some(ExplorerActivation::ToggleDirectory(_)) => {
                    self.explorer_focused = true;
                }
                None => self.set_error("nenhum item selecionado no explorador"),
            },
            ContextAction::NewFile => self.open_explorer_prompt(PromptKind::NewFile),
            ContextAction::NewDirectory => self.open_explorer_prompt(PromptKind::NewDirectory),
            ContextAction::Rename => self.open_explorer_prompt(PromptKind::Rename),
            ContextAction::Delete => self.request_delete_selected(false),
        }
    }

    /// Largura da barra lateral quando ela cabe no layout atual.
    pub fn explorer_visible_width(&self) -> Option<u16> {
        if !self.explorer.visible() {
            return None;
        }

        self.explorer_available_width()
    }

    /// Largura que a sidebar pode ocupar sem comprometer o editor.
    ///
    /// O limite antigo escondia o explorador em terminais com menos de 40
    /// colunas. Agora ele é adaptativo: a sidebar usa a largura preferida
    /// quando há espaço e encolhe até `MIN_WIDTH`, reservando sempre pelo
    /// menos [`MIN_EDITOR_WIDTH`] colunas para o editor.
    fn explorer_available_width(&self) -> Option<u16> {
        self.workspace.as_ref()?;

        let minimum = crate::explorer::MIN_WIDTH + MIN_EDITOR_WIDTH;
        if self.width < minimum {
            return None;
        }

        let maximum = self.width - MIN_EDITOR_WIDTH;
        Some(
            self.explorer
                .width()
                .min(maximum)
                .max(crate::explorer::MIN_WIDTH),
        )
    }

    /// Modos em que os atalhos globais podem ser aplicados sem conflito.
    ///
    /// A confirmação de alterações e o menu de contexto são operações modais
    /// críticas: seus atalhos continuam isolados. Nos demais modos, os
    /// atalhos globais não disputam teclas com a edição ou com os prompts.
    fn accepts_global_shortcuts(&self) -> bool {
        !matches!(
            self.mode,
            Mode::Confirm | Mode::ContextMenu | Mode::NavigationChooser
        )
    }

    /// Alterna a sidebar preservando a coerência entre visibilidade e foco.
    fn toggle_explorer(&mut self) {
        if self.explorer.visible() {
            self.apply_explorer_state(false);
            self.needs_redraw = true;
            return;
        }

        if self.workspace.is_none() {
            self.explorer_focused = false;
            self.set_info("abra um projeto com Ctrl+Shift+O (ou F3) para usar o Explorer");
            self.needs_redraw = true;
            return;
        }

        if self.explorer_available_width().is_none() {
            self.explorer_focused = false;
            self.set_info("terminal estreito: amplie a janela para abrir o Explorer");
            self.needs_redraw = true;
            return;
        }

        self.apply_explorer_state(true);
        self.needs_redraw = true;
    }

    /// Aplica visibilidade e foco como um único estado consistente.
    ///
    /// Uma sidebar só fica visível quando existe Workspace e espaço no layout;
    /// sempre que visível, o foco acompanha para que a navegação por teclado
    /// continue acessível.
    fn apply_explorer_state(&mut self, visible: bool) {
        let visible = visible && self.explorer_available_width().is_some();
        self.explorer.set_visible(visible);
        self.explorer_focused = visible;
    }

    /// Altura do cabeçalho: duas linhas quando há abas abertas.
    pub fn header_height(&self) -> u16 {
        if self.open_documents { 2 } else { 1 }
    }

    /// Trata clique e rolagem do mouse sem depender de posições mágicas da UI.
    fn handle_mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        if self.start_screen.is_some() {
            return;
        }

        if matches!(
            mouse.kind,
            MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.clear_status();
        }

        if self.mode == Mode::ContextMenu {
            self.handle_context_menu_mouse(mouse);
            return;
        }

        if self.mode != Mode::Editor {
            return;
        }

        let header = self.header_height();
        let tabs_row = self.open_documents.then(|| header.saturating_sub(1));
        let over_tabs = tabs_row == Some(mouse.row);
        let slots = if over_tabs {
            self.tab_slots(self.width)
        } else {
            Vec::new()
        };
        let tab_slot = slots
            .iter()
            .copied()
            .find(|slot| slot.contains(mouse.column));

        let hovered = tab_slot.map(|slot| slot.index);
        if self.hovered_tab != hovered {
            self.hovered_tab = hovered;
            self.needs_redraw = true;
        }

        let body_top = header;
        let body_bottom = self.height.saturating_sub(1);
        let in_body = mouse.row >= body_top && mouse.row < body_bottom;
        if self.problems_panel_height() > 0
            && mouse.row >= body_bottom.saturating_sub(self.problems_panel_height())
        {
            let panel_top = body_bottom.saturating_sub(self.problems_panel_height());
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.problems_focused = true;
                    let border_rows = u16::from(self.problems_panel_height() >= 3);
                    let row = mouse
                        .row
                        .saturating_sub(panel_top)
                        .saturating_sub(border_rows) as usize
                        + self.problems_selection_offset();
                    let items = self.problem_items();
                    if let Some(item) = items.get(row) {
                        self.problems_selection_index = row;
                        self.problems_selection = Some(item.identity.clone());
                        let now = Instant::now();
                        let activate = self.last_problem_click.as_ref().is_some_and(
                            |(previous_at, previous)| {
                                previous == &item.identity
                                    && now.duration_since(*previous_at) <= DOUBLE_CLICK_WINDOW
                            },
                        );
                        self.last_problem_click = Some((now, item.identity.clone()));
                        if activate {
                            self.activate_problem();
                        }
                    }
                    self.needs_redraw = true;
                }
                MouseEventKind::ScrollUp => self.move_problem_selection(-1),
                MouseEventKind::ScrollDown => self.move_problem_selection(1),
                _ => {}
            }
            return;
        }
        let sidebar = self.explorer_visible_width();
        let over_sidebar = sidebar.is_some_and(|width| mouse.column < width);
        let near_sidebar_border =
            sidebar.is_some_and(|width| mouse.column.abs_diff(width.saturating_sub(1)) <= 1);
        let explorer_index = if in_body && over_sidebar {
            self.explorer_index_at(mouse.row)
        } else {
            None
        };

        match mouse.kind {
            MouseEventKind::Moved => {}
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let forward = mouse.kind == MouseEventKind::ScrollDown;
                if in_body && over_sidebar {
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
            MouseEventKind::Down(MouseButton::Left) if over_tabs => {
                if let Some(slot) = tab_slot {
                    if slot.close_contains(mouse.column) {
                        self.close_tab(slot.index);
                    } else {
                        self.switch_tab(slot.index);
                    }
                }
                self.needs_redraw = true;
            }
            MouseEventKind::Down(MouseButton::Middle) if over_tabs => {
                if let Some(slot) = tab_slot {
                    self.close_tab(slot.index);
                }
                self.needs_redraw = true;
            }
            MouseEventKind::Down(MouseButton::Left) if in_body && near_sidebar_border => {
                // A borda tem uma área de acerto generosa; o arrasto só começa
                // nela para não redimensionar a sidebar ao interagir no editor.
                self.sidebar_drag = true;
                self.last_explorer_click = None;
            }
            MouseEventKind::Down(MouseButton::Right) if explorer_index.is_some() => {
                if let Some(index) = explorer_index {
                    self.explorer.select(index);
                    self.explorer_focused = true;
                    self.last_explorer_click = None;
                    self.open_context_menu();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if explorer_index.is_some() => {
                if let Some(index) = explorer_index {
                    self.problems_focused = false;
                    self.handle_explorer_click(index);
                }
            }
            MouseEventKind::Down(MouseButton::Middle) if explorer_index.is_some() => {
                if let Some(index) = explorer_index {
                    self.problems_focused = false;
                    self.explorer.select(index);
                    self.explorer_focused = true;
                    self.last_explorer_click = None;
                    self.activate_explorer_selection();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_body => {
                // Clique no editor devolve o foco para a edição e interrompe
                // uma sequência de duplo clique no Explorer.
                self.last_explorer_click = None;
                self.problems_focused = false;
                if self.explorer_focused {
                    self.explorer_focused = false;
                    self.needs_redraw = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.sidebar_drag && sidebar.is_some() => {
                // Arrastar a borda da sidebar redimensiona o explorador.
                let width = mouse.column.saturating_add(1);
                self.explorer.set_width(width.min(self.width / 2));
                self.needs_redraw = true;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.sidebar_drag = false;
            }
            _ => {}
        }
    }

    /// Índice do Explorer correspondente a uma linha absoluta do terminal.
    fn explorer_index_at(&self, row: u16) -> Option<usize> {
        let inner_top = self.header_height().saturating_add(1);
        if row < inner_top {
            return None;
        }

        let local = (row - inner_top) as usize;
        if local >= self.explorer_rows() {
            return None;
        }

        let index = self.explorer_row_offset() + local;
        (index < self.explorer.entries().len()).then_some(index)
    }

    /// Um clique simples seleciona; o segundo clique na mesma linha ativa.
    fn handle_explorer_click(&mut self, index: usize) {
        let now = Instant::now();
        let double = self.last_explorer_click.is_some_and(|(instant, previous)| {
            previous == index && now.duration_since(instant) <= DOUBLE_CLICK_WINDOW
        });

        self.explorer.select(index);
        self.explorer_focused = true;
        self.needs_redraw = true;

        if double {
            self.last_explorer_click = None;
            self.activate_explorer_selection();
        } else {
            self.last_explorer_click = Some((now, index));
        }
    }

    fn activate_explorer_selection(&mut self) {
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
            None => self.set_error("nenhum item selecionado no explorador"),
        }
        self.needs_redraw = true;
    }

    fn handle_context_menu_mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }

        let Some(menu) = self.context_menu.as_ref() else {
            return;
        };

        let rect = menu.rect(self.width, self.height);
        if !rect.contains(ratatui::layout::Position::new(mouse.column, mouse.row)) {
            self.close_context_menu();
            return;
        }

        let index = mouse.row.saturating_sub(rect.y).saturating_sub(1) as usize;
        if index < menu.items().len() {
            if let Some(menu) = self.context_menu.as_mut() {
                menu.selected = index;
            }
            self.activate_context_menu();
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

        let removed_paths: Vec<PathBuf> = std::iter::once(self.path.clone())
            .chain(
                self.documents
                    .inactive()
                    .iter()
                    .map(|document| document.path.clone()),
            )
            .flatten()
            .filter(|candidate| path_within(candidate, path))
            .collect();

        match self.explorer.delete(path, permanent) {
            Ok(()) => {
                let removed = self.with_active_document(|document, manager| {
                    manager.remove_paths_under(document, path)
                });
                for removed_path in &removed_paths {
                    self.sync_close_path(Some(removed_path));
                }
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

    /// Renomeações preservam o histórico e atualizam os caminhos já gravados.
    fn remap_navigation_history(&mut self, old: &Path, new: &Path) {
        for entry in self.navigation_history.entries_mut() {
            remap_navigation_location(&mut entry.origin, old, new);
            remap_navigation_location(&mut entry.target, old, new);
        }
    }

    fn remap_open_paths(&mut self, old: &Path, new: &Path) {
        let old = canonical_for_compare(old);
        let new = canonical_for_compare(new);
        self.remap_navigation_history(&old, &new);
        let mut renames = Vec::new();
        let remap = |path: &Path,
                     name: &mut String,
                     syntax: &mut Option<SyntaxState>,
                     disk: &mut Option<FileFingerprint>,
                     recovery_id: &mut Option<String>,
                     text: &str| {
            let candidate = canonical_for_compare(path);
            if candidate.starts_with(&old) {
                let suffix = candidate.strip_prefix(&old).unwrap_or(Path::new(""));
                let updated = if suffix.as_os_str().is_empty() {
                    new.clone()
                } else {
                    new.join(suffix)
                };
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
            renames.push((path, updated.clone(), text.clone()));
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
                renames.push((path, updated.clone(), text));
                document.path = Some(updated);
            }
        }

        // A renomeação troca a identidade do documento: fecha o caminho antigo
        // e abre o novo no mesmo servidor (ou em outro, se o workspace mudou).
        for (old, new, text) in renames {
            self.invalidate_navigation_for_uri(&old);
            self.document_sync.rename(&old, &new, &text);
            self.clear_diagnostics_for_path(&old);
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
        if !self.persistence || !self.running {
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

    /// Persiste apenas o Workspace e o estado do Explorer.
    ///
    /// Documentos e abas não pertencem mais à sessão: cada inicialização
    /// começa com o editor vazio. Conteúdo não salvo continua protegido pelo
    /// Recovery Manager.
    fn persist_session(&mut self) {
        if !self.persistence {
            return;
        }

        self.commit_session();

        let mut session = Session::default();
        session.workspace = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf());
        session.explorer = self.explorer.session();

        if let Err(error) = self.session_store.save(&session) {
            self.set_error(error.to_string());
        }
    }

    /// Salva a sessão e remove cópias de recuperação antes de encerrar.
    ///
    /// O encerramento normal só acontece depois que alterações foram salvas ou
    /// descartadas. Limpar as cópias garante que a próxima inicialização comece
    /// vazia; uma terminação anormal continua preservando o trabalho por meio
    /// das cópias gravadas durante a execução.
    fn persist_before_quit(&mut self) {
        if !self.persistence {
            return;
        }

        if let Some(id) = self.recovery_id.take() {
            let _ = self.recovery_manager.remove(&id);
        }

        for document in self.documents.inactive_mut() {
            if let Some(id) = document.recovery_id.take() {
                let _ = self.recovery_manager.remove(&id);
            }
        }

        self.persist_session();
    }

    /// Restaura o Workspace, o Explorer e cópias de recuperação.
    ///
    /// Documentos que estavam apenas abertos não são restaurados: a sessão não
    /// guarda mais abas. Cópias de recuperação continuam sendo abertas porque
    /// representam trabalho não salvo que não pode ser perdido.
    fn restore_session(&mut self) -> Vec<String> {
        if !self.persistence {
            return Vec::new();
        }

        let mut warnings = Vec::new();

        match self.session_store.load() {
            Ok(Some(session)) => {
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
                // Recompõe visibilidade e foco juntos: uma sidebar restaurada
                // aberta precisa continuar navegável por teclado; sem Workspace
                // ou sem espaço no layout, ela permanece fechada.
                self.apply_explorer_state(session.explorer.visible);

                // Reescreve a sessão sem documentos, limpando arquivos antigos
                // que ainda guardavam abas.
                self.persist_session();
            }
            Ok(None) => {}
            Err(error) => warnings.push(error.to_string()),
        }

        // A restauração pode ter aberto um Workspace que não existia quando o
        // sink foi instalado. Atualiza a raiz antes de reabrir os documentos
        // recuperados para que o `ServerPool` use a chave correta.
        let root = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.root().to_path_buf());
        self.document_sync.set_workspace(root.as_deref());

        let records = self.recovery_manager.load_all();
        if records.is_empty() {
            self.open_documents = false;
            return warnings;
        }

        let untitled = records
            .iter()
            .filter(|record| record.path.is_none())
            .count() as u64;
        let mut documents = Vec::with_capacity(records.len());

        for record in records {
            let mut document =
                Document::from_unsaved_text(record.name.clone(), record.path.clone(), &record.text);
            document.recovery_id = Some(record.id.clone());
            documents.push(document);
        }

        // O registro mais recente fica ativo, preservando a intenção do usuário
        // sem reabrir documentos que estavam apenas limpos.
        let active_index = documents.len() - 1;
        let active = documents.remove(active_index);

        self.documents = DocumentManager::new();
        for document in documents {
            self.documents.push_inactive(document);
        }
        self.documents.set_active_index(active_index);
        self.install_active_document(active);
        self.open_documents = true;
        self.untitled_sequence = untitled;
        self.overwrite_pending = false;
        self.sync_viewport();
        self.ensure_visible();
        self.sync_all_open();
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
        let previous_active_path = self.path.clone();

        self.with_active_document(|document, _| {
            message = evaluate_external_change(document, root.as_deref());
        });

        if let (Some(old), Some(new)) = (previous_active_path, self.path.clone())
            && old != new
        {
            let text = self.buffer.text().unwrap_or_default();
            self.handle_external_rename(&old, &new, &text);
        }

        if let Some((kind, text)) = message {
            self.report_message(kind, text);
        }

        let mut messages = Vec::new();
        let mut renames = Vec::new();
        for document in self.documents.inactive_mut() {
            let old_path = document.path.clone();
            if let Some(message) = evaluate_external_change(document, root.as_deref()) {
                messages.push(message);
            }
            if let (Some(old), Some(new)) = (old_path, document.path.clone())
                && old != new
            {
                renames.push((old, new, document.buffer.text().unwrap_or_default()));
            }
        }

        for (old, new, text) in renames {
            self.handle_external_rename(&old, &new, &text);
        }

        for (kind, text) in messages {
            self.report_message(kind, text);
        }

        self.refresh_watcher();
    }

    fn handle_external_rename(&mut self, old: &Path, new: &Path, text: &str) {
        self.invalidate_navigation_for_uri(old);
        self.document_sync.rename(old, new, text);
        self.clear_diagnostics_for_path(old);
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
                self.sync_resync_active();
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
        self.status_ticks = 0;
        self.needs_redraw = true;
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            kind: MessageKind::Error,
            text: text.into(),
        });
        self.status_ticks = 0;
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

    /// Abre o seletor de definições diretamente para testes de renderização.
    #[cfg(test)]
    pub(crate) fn open_navigation_chooser_for_tests(&mut self, targets: Vec<NavigationTarget>) {
        self.open_navigation_chooser_with_method_for_tests(NavigationMethod::Definition, targets);
    }

    #[cfg(test)]
    pub(crate) fn open_references_chooser_for_tests(&mut self, targets: Vec<NavigationTarget>) {
        self.open_navigation_chooser_with_method_for_tests(
            NavigationMethod::References {
                include_declaration: true,
            },
            targets,
        );
    }

    #[cfg(test)]
    pub(crate) fn open_declarations_chooser_for_tests(&mut self, targets: Vec<NavigationTarget>) {
        self.open_navigation_chooser_with_method_for_tests(NavigationMethod::Declaration, targets);
    }

    #[cfg(test)]
    pub(crate) fn open_type_definitions_chooser_for_tests(
        &mut self,
        targets: Vec<NavigationTarget>,
    ) {
        self.open_navigation_chooser_with_method_for_tests(
            NavigationMethod::TypeDefinition,
            targets,
        );
    }

    #[cfg(test)]
    fn open_navigation_chooser_with_method_for_tests(
        &mut self,
        method: NavigationMethod,
        targets: Vec<NavigationTarget>,
    ) {
        let origin = NavigationOriginContext {
            operation_id: NavigationOperationId(0),
            document_id: self.document_id,
            method,
            workspace: String::new(),
            language_id: "rust".to_owned(),
            workspace_generation: self.workspace_generation,
            session_id: None,
            uri: self
                .path
                .as_deref()
                .map(nexus_lsp::uri::path_to_uri)
                .unwrap_or_default(),
            position: Position {
                line: 0,
                character: 0,
            },
            document_revision: self.buffer.revision(),
            location: self.capture_navigation_location(),
        };
        self.open_navigation_chooser(origin, targets);
    }

    /// Abre um documento substituindo a aba ativa, como `PendingAction::Open`.
    #[cfg(test)]
    pub(crate) fn open_document_for_tests(
        &mut self,
        path: &Path,
    ) -> Result<(), crate::document::DocumentError> {
        self.open_document(path)
    }

    /// Executa a exclusão já confirmada de um caminho.
    #[cfg(test)]
    pub(crate) fn perform_delete_for_tests(&mut self, path: &Path, permanent: bool) {
        self.perform_delete(path, permanent);
    }

    /// Remapeia abas abertas após uma renomeação já efetivada no disco.
    #[cfg(test)]
    pub(crate) fn remap_open_paths_for_tests(&mut self, old: &Path, new: &Path) {
        self.remap_open_paths(old, new);
    }
}

/// Mapeia um deslocamento através das edições de uma transação.
///
/// As edições são percorridas da maior posição para a menor, de modo que cada
/// deslocamento permaneça comparável às posições originais das edições ainda
/// não processadas. Inserções no ponto exato do cursor o deslocam para depois
/// do texto inserido; remoções que cobrem o cursor o fixam no início do
/// intervalo removido.
fn map_offset_through_transaction(offset: CharOffset, transaction: &Transaction) -> CharOffset {
    let mut mapped = offset.0;
    let mut edits: Vec<_> = transaction.edits().iter().collect();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start.0));

    for edit in edits {
        let start = edit.range.start.0;
        let deleted = edit.deleted_text.chars().count();
        let inserted = edit.inserted_text.chars().count();

        if mapped < start {
            continue;
        }

        if mapped < start + deleted {
            mapped = start + inserted;
        } else {
            mapped = mapped - deleted + inserted;
        }
    }

    CharOffset(mapped)
}

/// Remove apenas destinos exatamente iguais, preservando a ordem original.
fn is_references_method(method: NavigationMethod) -> bool {
    matches!(method, NavigationMethod::References { .. })
}

fn is_declaration_method(method: NavigationMethod) -> bool {
    matches!(method, NavigationMethod::Declaration)
}

fn is_type_definition_method(method: NavigationMethod) -> bool {
    matches!(method, NavigationMethod::TypeDefinition)
}

fn navigation_result_label(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "referência"
    } else if is_declaration_method(method) {
        "declaração"
    } else if is_type_definition_method(method) {
        "definição de tipo"
    } else {
        "definição"
    }
}

fn navigation_result_plural(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "referências"
    } else if is_declaration_method(method) {
        "declarações"
    } else if is_type_definition_method(method) {
        "definições de tipo"
    } else {
        "definições"
    }
}

fn navigation_action_label(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "localizar referências"
    } else if is_declaration_method(method) {
        "buscar declaração"
    } else if is_type_definition_method(method) {
        "buscar definição de tipo"
    } else {
        "buscar definição"
    }
}

fn no_navigation_results_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "nenhuma referência encontrada"
    } else if is_declaration_method(method) {
        "nenhuma declaração encontrada"
    } else if is_type_definition_method(method) {
        "nenhuma definição de tipo encontrada"
    } else {
        "nenhuma definição encontrada"
    }
}

fn no_navigation_document_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "nenhum documento aberto para localizar referências"
    } else if is_declaration_method(method) {
        "nenhum documento aberto para ir à declaração"
    } else if is_type_definition_method(method) {
        "nenhum documento aberto para ir à definição de tipo"
    } else {
        "nenhum documento aberto para ir à definição"
    }
}

fn request_navigation_failure_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "não foi possível solicitar as referências para este documento"
    } else if is_declaration_method(method) {
        "não foi possível solicitar a declaração para este documento"
    } else if is_type_definition_method(method) {
        "não foi possível solicitar a definição de tipo para este documento"
    } else {
        "não foi possível solicitar a definição para este documento"
    }
}

fn navigation_cancelled_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "busca por referências cancelada"
    } else if is_declaration_method(method) {
        "busca por declaração cancelada"
    } else if is_type_definition_method(method) {
        "busca por definição de tipo cancelada"
    } else {
        "busca por definição cancelada"
    }
}

fn navigation_pending_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "procurando referências..."
    } else if is_declaration_method(method) {
        "procurando declaração..."
    } else if is_type_definition_method(method) {
        "procurando definição de tipo..."
    } else {
        "procurando definição..."
    }
}

fn no_navigation_selection_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "nenhuma referência selecionada"
    } else if is_declaration_method(method) {
        "nenhum destino de declaração selecionado"
    } else if is_type_definition_method(method) {
        "nenhum destino de definição de tipo selecionado"
    } else {
        "nenhum destino de definição selecionado"
    }
}

fn unsupported_navigation_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "o servidor LSP não suporta localizar referências"
    } else if is_declaration_method(method) {
        "o servidor LSP não suporta ir para declaração"
    } else if is_type_definition_method(method) {
        "o servidor LSP não suporta ir à definição de tipo"
    } else {
        "o servidor LSP não suporta ir para definição"
    }
}

fn server_navigation_failure_message(method: NavigationMethod) -> &'static str {
    if is_references_method(method) {
        "o servidor LSP não conseguiu localizar as referências"
    } else if is_declaration_method(method) {
        "o servidor LSP não conseguiu resolver a declaração"
    } else if is_type_definition_method(method) {
        "o servidor LSP não conseguiu resolver a definição de tipo"
    } else {
        "o servidor LSP não conseguiu resolver a definição"
    }
}

/// Referências podem chegar fora de ordem e repetidas. Ordena por arquivo e
/// posição, preservando ocorrências distintas na mesma linha.
fn normalize_reference_targets(mut targets: Vec<NavigationTarget>) -> Vec<NavigationTarget> {
    targets.retain(|target| {
        !target.uri.trim().is_empty() && target.selection_range.start <= target.selection_range.end
    });
    targets.sort_by(|left, right| {
        left.uri
            .cmp(&right.uri)
            .then(left.selection_range.start.cmp(&right.selection_range.start))
            .then(left.selection_range.end.cmp(&right.selection_range.end))
            .then(left.target_range.start.cmp(&right.target_range.start))
            .then(left.target_range.end.cmp(&right.target_range.end))
    });
    targets.dedup_by(|left, right| {
        left.uri == right.uri && left.selection_range == right.selection_range
    });
    targets
}

fn normalize_navigation_targets(targets: Vec<NavigationTarget>) -> Vec<NavigationTarget> {
    let mut unique = Vec::with_capacity(targets.len());
    for target in targets {
        if !unique.contains(&target) {
            unique.push(target);
        }
    }
    unique
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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

fn canonical_for_compare(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    crate::workspace::absolute_path(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_within(candidate: &Path, target: &Path) -> bool {
    canonical_for_compare(candidate).starts_with(canonical_for_compare(target))
}

fn remap_navigation_location(location: &mut NavigationLocation, old: &Path, new: &Path) {
    let Some(path) = location.path.clone() else {
        return;
    };

    if !path.starts_with(old) {
        return;
    }

    let suffix = path.strip_prefix(old).unwrap_or(Path::new(""));
    let updated = if suffix.as_os_str().is_empty() {
        new.to_path_buf()
    } else {
        new.join(suffix)
    };
    location.uri = nexus_lsp::uri::path_to_uri(&updated);
    location.path = Some(updated);
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
    use std::sync::{Arc, Mutex};

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

    fn press_ctrl_shift(app: &mut App, code: KeyCode) {
        app.update(Event::Key(KeyEvent::new(
            code,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    fn press_alt(app: &mut App, code: KeyCode) {
        app.update(Event::Key(KeyEvent::new(code, KeyModifiers::ALT)));
    }

    fn press_quit(app: &mut App) {
        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
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
            KeyCode::F(11),
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

        // Enter reaplica a indentação existente na linha.
        assert_eq!(text(&app), " \t\n \tx");
        assert_eq!(app.view().cursor(), CharOffset(6));
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
    fn tab_indents_selected_lines_as_one_undo_step() {
        let mut app = app("alpha\nbeta\ngamma");
        app.view
            .set_selection(&app.buffer, CharOffset(0), CharOffset(10))
            .unwrap();

        press(&mut app, KeyCode::Tab);

        assert_eq!(text(&app), "\talpha\n\tbeta\ngamma");
        assert_eq!(
            app.view.selection_range(),
            Some(TextRange::new(CharOffset(1), CharOffset(12)))
        );

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "alpha\nbeta\ngamma");

        press_ctrl(&mut app, KeyCode::Char('y'));
        assert_eq!(text(&app), "\talpha\n\tbeta\ngamma");
    }

    #[test]
    fn tab_without_selection_keeps_inserting_a_literal_tab() {
        let mut app = app("");

        press(&mut app, KeyCode::Tab);

        assert_eq!(text(&app), "\t");
        assert_eq!(app.view().cursor(), CharOffset(1));
    }

    #[test]
    fn shift_tab_outdents_selection_and_current_line() {
        let mut selected = app("\talpha\n    beta\nnone");
        selected
            .view
            .set_selection(&selected.buffer, CharOffset(0), CharOffset(15))
            .unwrap();

        press_shift(&mut selected, KeyCode::Tab);

        assert_eq!(text(&selected), "alpha\nbeta\nnone");
        assert_eq!(
            selected.view.selection_range(),
            Some(TextRange::new(CharOffset(0), CharOffset(10)))
        );

        let mut current = app("    alpha\nbeta");
        current
            .view
            .set_cursor(&current.buffer, CharOffset(2))
            .unwrap();
        press(&mut current, KeyCode::BackTab);

        assert_eq!(text(&current), "alpha\nbeta");
        assert_eq!(current.view().cursor(), CharOffset(0));
    }

    #[test]
    fn enter_auto_indents_and_preserves_crlf() {
        let mut unix = app("    code");
        unix.view
            .set_cursor(&unix.buffer, CharOffset(8))
            .unwrap();

        press(&mut unix, KeyCode::Enter);
        press(&mut unix, KeyCode::Char('x'));

        assert_eq!(text(&unix), "    code\n    x");

        let mut crlf = app("    a\r\n    b\r\n");
        crlf.view
            .set_cursor(&crlf.buffer, CharOffset(5))
            .unwrap();

        press(&mut crlf, KeyCode::Enter);
        press(&mut crlf, KeyCode::Char('x'));

        assert_eq!(text(&crlf), "    a\r\n    x\r\n    b\r\n");
    }

    #[test]
    fn enter_on_empty_line_or_empty_document_does_not_add_spaces() {
        let mut empty_line = app("\n");
        press(&mut empty_line, KeyCode::Enter);
        assert_eq!(text(&empty_line), "\n\n");

        let mut empty_document = app("");
        press(&mut empty_document, KeyCode::Enter);
        assert_eq!(text(&empty_document), "\n");
        assert_eq!(empty_document.view().cursor(), CharOffset(1));
    }

    #[test]
    fn enter_with_selection_uses_the_indentation_of_the_selection_start() {
        let mut app = app("    alpha\n    beta");
        app.view
            .set_selection(&app.buffer, CharOffset(4), CharOffset(9))
            .unwrap();

        press(&mut app, KeyCode::Enter);

        assert_eq!(text(&app), "    \n    \n    beta");
        assert_eq!(app.view().cursor(), CharOffset(9));
        assert!(!app.view().has_selection());
    }

    #[test]
    fn ctrl_d_duplicates_the_current_line_and_keeps_the_cursor_in_the_copy() {
        let mut app = app("alpha\nbeta");
        app.view
            .set_cursor(&app.buffer, CharOffset(2))
            .unwrap();

        press_ctrl(&mut app, KeyCode::Char('d'));

        assert_eq!(text(&app), "alpha\nalpha\nbeta");
        assert_eq!(app.view().cursor(), CharOffset(8));

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "alpha\nbeta");
    }

    #[test]
    fn ctrl_shift_d_duplicates_the_selected_block_and_preserves_the_selection() {
        let mut app = app("one\ntwo\nthree");
        app.view
            .set_selection(&app.buffer, CharOffset(0), CharOffset(7))
            .unwrap();

        press_ctrl_shift(&mut app, KeyCode::Char('D'));

        assert_eq!(text(&app), "one\ntwo\none\ntwo\nthree");
        assert_eq!(
            app.view.selection_range(),
            Some(TextRange::new(CharOffset(8), CharOffset(15)))
        );
        assert_eq!(
            app.buffer
                .slice(app.view.selection_range().unwrap())
                .unwrap(),
            "one\ntwo"
        );

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "one\ntwo\nthree");
    }

    #[test]
    fn alt_arrows_move_the_line_and_adjust_the_cursor() {
        let mut app = app("one\ntwo\nthree");
        app.view
            .set_cursor(&app.buffer, CharOffset(5))
            .unwrap();

        press_alt(&mut app, KeyCode::Up);

        assert_eq!(text(&app), "two\none\nthree");
        assert_eq!(app.view().cursor(), CharOffset(1));

        press_alt(&mut app, KeyCode::Down);

        assert_eq!(text(&app), "one\ntwo\nthree");
        assert_eq!(app.view().cursor(), CharOffset(5));
    }

    #[test]
    fn alt_arrows_move_the_selected_block_and_preserve_the_selection() {
        let mut app = app("one\ntwo\nthree");
        app.view
            .set_selection(&app.buffer, CharOffset(0), CharOffset(7))
            .unwrap();

        press_alt(&mut app, KeyCode::Down);

        assert_eq!(text(&app), "three\none\ntwo");
        assert_eq!(
            app.view.selection_range(),
            Some(TextRange::new(CharOffset(6), CharOffset(13)))
        );
        assert_eq!(
            app.buffer
                .slice(app.view.selection_range().unwrap())
                .unwrap(),
            "one\ntwo"
        );
    }

    #[test]
    fn alt_arrows_do_not_cross_document_boundaries() {
        let mut app = app("alpha\nbeta");
        app.view
            .set_cursor(&app.buffer, CharOffset(0))
            .unwrap();

        press_alt(&mut app, KeyCode::Up);
        assert_eq!(text(&app), "alpha\nbeta");
        assert!(!app.buffer().can_undo());

        app.view
            .set_cursor(&app.buffer, CharOffset(8))
            .unwrap();
        press_alt(&mut app, KeyCode::Down);
        assert_eq!(text(&app), "alpha\nbeta");
        assert!(!app.buffer().can_undo());
    }

    #[test]
    fn tab_indents_a_reversed_selection_and_preserves_its_direction() {
        let mut app = app("alpha\nbeta\ngamma");
        app.view
            .set_selection(&app.buffer, CharOffset(10), CharOffset(0))
            .unwrap();

        press(&mut app, KeyCode::Tab);

        assert_eq!(text(&app), "\talpha\n\tbeta\ngamma");
        assert_eq!(app.view().cursor(), CharOffset(1));
        assert_eq!(
            app.view.selection_range(),
            Some(TextRange::new(CharOffset(1), CharOffset(12)))
        );
    }

    #[test]
    fn duplicate_preserves_a_reversed_block_selection_in_the_copy() {
        let mut app = app("one\ntwo\nthree");
        app.view
            .set_selection(&app.buffer, CharOffset(7), CharOffset(0))
            .unwrap();

        press_ctrl_shift(&mut app, KeyCode::Char('D'));

        assert_eq!(text(&app), "one\ntwo\none\ntwo\nthree");
        assert_eq!(app.view().cursor(), CharOffset(8));
        assert_eq!(
            app.view.selection_range(),
            Some(TextRange::new(CharOffset(8), CharOffset(15)))
        );
        assert_eq!(
            app.buffer
                .slice(app.view.selection_range().unwrap())
                .unwrap(),
            "one\ntwo"
        );
    }

    #[test]
    fn moving_a_reversed_block_selection_preserves_its_direction() {
        let mut app = app("one\ntwo\nthree");
        app.view
            .set_selection(&app.buffer, CharOffset(7), CharOffset(0))
            .unwrap();

        press_alt(&mut app, KeyCode::Down);

        assert_eq!(text(&app), "three\none\ntwo");
        assert_eq!(app.view().cursor(), CharOffset(6));
        assert_eq!(
            app.view.selection_range(),
            Some(TextRange::new(CharOffset(6), CharOffset(13)))
        );
        assert_eq!(
            app.buffer
                .slice(app.view.selection_range().unwrap())
                .unwrap(),
            "one\ntwo"
        );
    }

    #[test]
    fn block_shortcuts_are_consumed_while_the_explorer_is_focused() {
        let directory = TempDir::new("block-shortcuts-explorer");
        fs::write(directory.join("file.txt"), "conteúdo").unwrap();
        let mut app = app("one\ntwo");
        app.open_workspace(&directory.path).unwrap();
        assert!(app.explorer_focused());

        press_alt(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Tab);
        press_shift(&mut app, KeyCode::Tab);

        assert_eq!(text(&app), "one\ntwo");
        assert!(!app.buffer().can_undo());
    }

    #[test]
    fn line_operations_are_individually_undoable_and_keep_the_dirty_state() {
        let mut app = app("a\nb");
        assert!(!app.buffer().is_modified());

        press_ctrl(&mut app, KeyCode::Char('d'));
        assert_eq!(text(&app), "a\na\nb");
        assert!(app.buffer().is_modified());

        press_alt(&mut app, KeyCode::Down);
        assert_eq!(text(&app), "a\nb\na");
        assert!(app.buffer().is_modified());

        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "a\na\nb");
        press_ctrl(&mut app, KeyCode::Char('z'));
        assert_eq!(text(&app), "a\nb");
        assert!(!app.buffer().is_modified());

        press_ctrl(&mut app, KeyCode::Char('y'));
        assert_eq!(text(&app), "a\na\nb");
        press_ctrl(&mut app, KeyCode::Char('y'));
        assert_eq!(text(&app), "a\nb\na");
        assert!(app.buffer().is_modified());
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

        press(&mut app, KeyCode::Char('a'));
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

        press_quit(&mut application);

        assert!(!application.is_running());
        assert!(application.confirm().is_none());
    }

    #[test]
    fn quit_modified_opens_confirmation() {
        let mut application = modified_app("");

        press_quit(&mut application);

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

        press_quit(&mut application);
        press(&mut application, KeyCode::Char('s'));

        assert!(!application.is_running());
        assert_eq!(fs::read(&path).unwrap(), b"abcx");
    }

    #[test]
    fn quit_save_without_path_uses_save_as_and_exits() {
        let directory = TempDir::new("quit-save-as");
        let path = directory.join("document.txt");
        let mut application = modified_app("abc");

        press_quit(&mut application);
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

        press_quit(&mut application);
        press(&mut application, KeyCode::Char('d'));

        assert!(!application.is_running());
        assert_eq!(fs::read(&path).unwrap(), b"abc");
    }

    #[test]
    fn quit_cancel_keeps_document_and_running() {
        let mut application = modified_app("abc");
        let before = text(&application);

        press_quit(&mut application);
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

        press_quit(&mut application);
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

        press_quit(&mut application);

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

        press_quit(&mut application);

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

        press_quit(&mut application);
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

        press_quit(&mut application);
        press_quit(&mut application);

        assert_eq!(application.mode(), Mode::Confirm);
        assert!(application.confirm().is_some());
        assert!(application.is_running());
    }

    #[test]
    fn cancel_save_as_during_pending_quit_cancels_exit() {
        let mut application = modified_app("abc");

        press_quit(&mut application);
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

    #[test]
    fn ctrl_q_closes_active_tab_and_selects_neighbor() {
        let directory = TempDir::new("tabs-ctrl-q");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        let third = directory.join("c.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        fs::write(&third, "C").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        application.open_document_in_new_tab(&third).unwrap();
        assert!(application.switch_tab(1));
        assert_eq!(text(&application), "B");

        press_ctrl(&mut application, KeyCode::Char('q'));

        assert_eq!(application.tab_names(), vec!["a.txt", "c.txt"]);
        assert_eq!(application.active_tab(), 1);
        assert_eq!(text(&application), "C");
        assert!(application.is_running());

        press_ctrl(&mut application, KeyCode::Char('q'));
        assert_eq!(application.tab_names(), vec!["a.txt"]);
        assert_eq!(application.active_tab(), 0);
        assert_eq!(text(&application), "A");

        press_ctrl(&mut application, KeyCode::Char('q'));
        assert!(!application.has_open_documents());
        assert_eq!(application.tab_count(), 1);
        assert_eq!(application.document_name(), "sem título");
        assert_eq!(text(&application), "");
        assert_eq!(application.header_height(), 1);
        assert!(application.tab_slots(80).is_empty());
        assert!(application.is_running());
    }

    #[test]
    fn middle_click_closes_tab_under_pointer() {
        let directory = TempDir::new("tabs-middle-click");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        assert_eq!(application.active_tab(), 1);
        let slot = application.tab_slots(80)[0];

        application.update(mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            slot.start + 1,
            1,
        ));

        assert_eq!(application.tab_names(), vec!["b.txt"]);
        assert_eq!(application.active_tab(), 0);
        assert_eq!(text(&application), "B");
    }

    #[test]
    fn close_glyph_click_closes_active_tab() {
        let directory = TempDir::new("tabs-close-glyph");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        let slot = application.tab_slots(80)[1];

        application.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            slot.close,
            1,
        ));

        assert_eq!(application.tab_names(), vec!["a.txt"]);
        assert_eq!(application.active_tab(), 0);
        assert_eq!(text(&application), "A");
        assert_eq!(
            application
                .status_message()
                .map(|message| message.text.as_str()),
            Some("aba fechada")
        );
    }

    #[test]
    fn unsaved_tab_close_offers_save_discard_and_cancel() {
        let directory = TempDir::new("tabs-unsaved-close");
        let path = directory.join("document.txt");
        fs::write(&path, "abc").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&path).unwrap();
        let end = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(end))
            .unwrap();
        press(&mut application, KeyCode::Char('x'));
        application.on_idle();

        press_ctrl(&mut application, KeyCode::Char('q'));
        assert_eq!(application.mode(), Mode::Confirm);
        assert_eq!(
            application.confirm().map(ConfirmDialog::kind),
            Some(ConfirmKind::CloseTab)
        );

        press(&mut application, KeyCode::Esc);
        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.has_open_documents());
        assert!(application.buffer().is_modified());
        assert_eq!(text(&application), "abcx");
        assert_eq!(fs::read(&path).unwrap(), b"abc");

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('s'));
        assert!(!application.has_open_documents());
        assert_eq!(fs::read(&path).unwrap(), b"abcx");

        application.open_document_in_new_tab(&path).unwrap();
        let end = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(end))
            .unwrap();
        press(&mut application, KeyCode::Char('y'));
        application.on_idle();
        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('d'));

        assert!(!application.has_open_documents());
        assert_eq!(fs::read(&path).unwrap(), b"abcx");
    }

    #[test]
    fn middle_click_on_modified_inactive_tab_opens_confirmation() {
        let directory = TempDir::new("tabs-middle-modified");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        application.switch_tab(0);
        let end = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(end))
            .unwrap();
        press(&mut application, KeyCode::Char('x'));
        application.on_idle();
        application.switch_tab(1);

        let slot = application.tab_slots(80)[0];
        application.update(mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            slot.start + 1,
            1,
        ));

        assert_eq!(application.mode(), Mode::Confirm);
        assert_eq!(
            application.confirm().map(ConfirmDialog::kind),
            Some(ConfirmKind::CloseTab)
        );
        assert_eq!(application.active_tab(), 0);
        assert_eq!(text(&application), "Ax");

        press(&mut application, KeyCode::Char('d'));
        assert_eq!(application.tab_names(), vec!["b.txt"]);
        assert_eq!(text(&application), "B");
        assert_eq!(fs::read(&first).unwrap(), b"A");
    }

    #[test]
    fn hovering_a_tab_requests_redraw_and_tracks_pointer() {
        let directory = TempDir::new("tabs-hover");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "A").unwrap();
        fs::write(&second, "B").unwrap();
        let mut application = app("");

        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        application.mark_rendered();
        let slot = application.tab_slots(80)[0];

        application.update(mouse_event(MouseEventKind::Moved, slot.start + 1, 1));
        assert_eq!(application.hovered_tab(), Some(0));
        assert!(application.needs_redraw());

        application.mark_rendered();
        application.update(mouse_event(MouseEventKind::Moved, 5, 5));
        assert_eq!(application.hovered_tab(), None);
        assert!(application.needs_redraw());
    }

    #[test]
    fn explorer_keyboard_navigates_expands_collapses_and_opens_files() {
        let directory = TempDir::new("explorer-keyboard");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::create_dir(project.join("src")).unwrap();
        fs::write(project.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(project.join("notes.txt"), "notas").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());
        assert_eq!(application.explorer().selected(), 0);
        assert_eq!(
            application
                .explorer()
                .entries()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["src", "notes.txt"]
        );

        press(&mut application, KeyCode::Down);
        assert_eq!(application.explorer().selected(), 1);
        press(&mut application, KeyCode::Up);
        assert_eq!(application.explorer().selected(), 0);

        press(&mut application, KeyCode::Right);
        assert!(application.explorer().entries()[0].expanded);
        assert_eq!(application.explorer().entries().len(), 3);
        assert_eq!(application.explorer().entries()[1].name, "main.rs");

        press(&mut application, KeyCode::Down);
        assert_eq!(application.explorer().entries()[1].name, "main.rs");
        assert_eq!(application.explorer().selected(), 1);

        press(&mut application, KeyCode::Left);
        assert_eq!(application.explorer().selected(), 0);
        assert!(application.explorer().entries()[0].expanded);

        press(&mut application, KeyCode::Left);
        assert!(!application.explorer().entries()[0].expanded);
        assert_eq!(application.explorer().entries().len(), 2);

        press(&mut application, KeyCode::Esc);
        assert!(!application.explorer_focused());

        application.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 2));
        assert!(application.explorer_focused());
        press(&mut application, KeyCode::Down);
        assert_eq!(application.explorer().entries()[1].name, "notes.txt");
        press(&mut application, KeyCode::Enter);

        assert!(!application.explorer_focused());
        assert_eq!(application.tab_names(), vec!["notes.txt"]);
        assert_eq!(text(&application), "notas");
    }

    #[test]
    fn explorer_focus_ignores_all_letters_without_creating_documents() {
        let directory = TempDir::new("explorer-letters-empty");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());

        for letter in 'a'..='z' {
            press(&mut application, KeyCode::Char(letter));
        }
        for letter in 'A'..='Z' {
            press_shift(&mut application, KeyCode::Char(letter));
        }

        assert!(application.explorer_focused());
        assert_eq!(application.mode(), Mode::Editor);
        assert!(!application.has_open_documents());
        assert_eq!(application.tab_count(), 1);
        assert_eq!(application.document_name(), "sem título");
        assert_eq!(text(&application), "");
    }

    #[test]
    fn explorer_focus_ignores_letters_without_modifying_open_document() {
        let directory = TempDir::new("explorer-letters-document");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let path = project.join("a.txt");
        fs::write(&path, "abc").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        application.open_document_in_new_tab(&path).unwrap();
        assert_eq!(text(&application), "abc");
        assert!(application.explorer_focused());

        for letter in 'a'..='z' {
            press(&mut application, KeyCode::Char(letter));
        }
        for letter in 'A'..='Z' {
            press_shift(&mut application, KeyCode::Char(letter));
        }

        assert!(application.explorer_focused());
        assert_eq!(application.tab_names(), vec!["a.txt"]);
        assert_eq!(text(&application), "abc");
        assert!(!application.buffer().is_modified());
    }

    #[test]
    fn explorer_focus_preserves_global_shortcuts() {
        let directory = TempDir::new("explorer-global-shortcuts");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let path = project.join("a.txt");
        fs::write(&path, "A").unwrap();
        let mut application = app("");
        enable_native_picker(&mut application);

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(!application.explorer_focused());
        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('o'));
        assert_eq!(
            application
                .take_picker_request()
                .map(|request| request.kind),
            Some(PickerKind::Files)
        );
        application.complete_picker(None);
        assert!(application.explorer_focused());

        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));
        assert_eq!(
            application
                .take_picker_request()
                .map(|request| request.kind),
            Some(PickerKind::Directory)
        );
        application.complete_picker(None);
        assert!(application.explorer_focused());

        application.open_document_in_new_tab(&path).unwrap();
        press_ctrl(&mut application, KeyCode::Char('b'));
        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('q'));
        assert!(!application.has_open_documents());
        assert!(application.is_running());

        press_quit(&mut application);
        assert!(!application.is_running());
    }

    #[test]
    fn explorer_enter_toggles_folders_and_opens_files() {
        let directory = TempDir::new("explorer-enter");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::create_dir(project.join("src")).unwrap();
        fs::write(project.join("src").join("main.rs"), "fn main() {}").unwrap();
        fs::write(project.join("notes.txt"), "notas").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());
        assert_eq!(application.explorer().entries()[0].name, "src");

        press(&mut application, KeyCode::Enter);
        assert!(application.explorer().entries()[0].expanded);
        assert_eq!(application.explorer().entries().len(), 3);

        press(&mut application, KeyCode::Enter);
        assert!(!application.explorer().entries()[0].expanded);
        assert_eq!(application.explorer().entries().len(), 2);

        press(&mut application, KeyCode::Down);
        assert_eq!(application.explorer().entries()[1].name, "notes.txt");
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.tab_names(), vec!["notes.txt"]);
        assert_eq!(text(&application), "notas");
        assert!(!application.explorer_focused());
    }

    #[test]
    fn explorer_f2_opens_rename_prompt_and_letters_stay_in_prompt() {
        let directory = TempDir::new("explorer-f2");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());
        press(&mut application, KeyCode::F(2));

        assert_eq!(application.mode(), Mode::Prompt(PromptKind::Rename));
        assert_eq!(application.prompt().unwrap().input(), "a.txt");
        assert!(!application.has_open_documents());

        press(&mut application, KeyCode::Char('x'));
        assert_eq!(application.prompt().unwrap().input(), "a.txtx");
        assert!(!application.has_open_documents());
        assert_eq!(text(&application), "");

        let length = application.prompt().unwrap().input().chars().count();
        for _ in 0..length {
            application.apply_action(Action::PromptBackspace);
        }
        type_prompt(&mut application, "b.txt");
        application.apply_action(Action::PromptConfirm);

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.explorer_focused());
        assert!(!project.join("a.txt").exists());
        assert!(project.join("b.txt").exists());
        assert!(!application.has_open_documents());
        assert_eq!(text(&application), "");
    }

    #[test]
    fn explorer_delete_opens_existing_confirmation_and_isolates_letters() {
        let directory = TempDir::new("explorer-delete");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());
        press(&mut application, KeyCode::Delete);

        assert_eq!(application.mode(), Mode::Confirm);
        assert_eq!(
            application.confirm().map(ConfirmDialog::kind),
            Some(ConfirmKind::Delete)
        );

        // Atalhos globais não atravessam o diálogo modal.
        press_ctrl(&mut application, KeyCode::Char('q'));
        assert_eq!(application.mode(), Mode::Confirm);
        assert!(project.join("a.txt").exists());

        // Letras sem função no diálogo não chegam ao editor.
        for letter in ['a', 'x', 'q'] {
            press(&mut application, KeyCode::Char(letter));
        }
        assert_eq!(application.mode(), Mode::Confirm);
        assert!(!application.has_open_documents());
        assert_eq!(text(&application), "");

        press(&mut application, KeyCode::Esc);
        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.explorer_focused());
        assert!(project.join("a.txt").exists());
        assert!(!application.has_open_documents());
    }

    #[test]
    fn esc_returns_focus_and_editor_typing_resumes() {
        let directory = TempDir::new("explorer-focus-transition");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());

        press(&mut application, KeyCode::Esc);
        assert!(!application.explorer_focused());
        press(&mut application, KeyCode::Char('a'));
        assert!(application.has_open_documents());
        assert_eq!(text(&application), "a");
        press(&mut application, KeyCode::Char('b'));
        assert_eq!(text(&application), "ab");

        press_ctrl(&mut application, KeyCode::Char('b'));
        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(application.explorer_focused());
        press(&mut application, KeyCode::Char('c'));
        assert_eq!(text(&application), "ab");

        press(&mut application, KeyCode::Esc);
        press(&mut application, KeyCode::Char('c'));
        assert_eq!(text(&application), "abc");
    }

    #[test]
    fn explorer_mouse_click_selects_and_editor_click_returns_focus() {
        let directory = TempDir::new("explorer-mouse-select");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        fs::write(project.join("b.txt"), "B").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());
        assert_eq!(application.explorer().selected(), 0);

        application.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 3));
        assert!(application.explorer_focused());
        assert_eq!(application.explorer().selected(), 1);
        assert!(!application.has_open_documents());

        application.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 60, 3));
        assert!(!application.explorer_focused());
        assert_eq!(application.explorer().selected(), 1);
        assert!(!application.has_open_documents());

        // Um clique no editor interrompe a sequência: o próximo clique simples
        // no Explorer apenas seleciona de novo.
        application.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 3));
        assert!(application.explorer_focused());
        assert!(!application.has_open_documents());
    }

    #[test]
    fn explorer_double_click_opens_files_and_toggles_folders() {
        let directory = TempDir::new("explorer-double-click");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        for _ in 0..2 {
            application.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 2));
        }

        assert_eq!(application.tab_names(), vec!["a.txt"]);
        assert!(!application.explorer_focused());

        let folders = directory.join("pastas");
        fs::create_dir(&folders).unwrap();
        fs::create_dir(folders.join("src")).unwrap();
        fs::write(folders.join("src").join("main.rs"), "fn main() {}").unwrap();
        let mut folders_app = app("");
        folders_app.open_workspace(&folders).unwrap();

        for _ in 0..2 {
            folders_app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 2));
        }
        assert!(folders_app.explorer().entries()[0].expanded);
        assert_eq!(folders_app.explorer().entries().len(), 2);

        for _ in 0..2 {
            folders_app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 2));
        }
        assert!(!folders_app.explorer().entries()[0].expanded);
        assert_eq!(folders_app.explorer().entries().len(), 1);
    }

    #[test]
    fn explorer_middle_click_opens_file() {
        let directory = TempDir::new("explorer-middle-click");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        application.update(mouse_event(MouseEventKind::Down(MouseButton::Middle), 2, 2));

        assert_eq!(application.tab_names(), vec!["a.txt"]);
        assert!(!application.explorer_focused());
    }

    #[test]
    fn context_menu_opens_file_and_isolates_global_shortcuts() {
        let directory = TempDir::new("context-menu-open");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        application.update(mouse_event(MouseEventKind::Down(MouseButton::Right), 2, 2));

        assert_eq!(application.mode(), Mode::ContextMenu);
        let menu = application.context_menu().unwrap();
        assert_eq!(menu.title(), "a.txt");
        assert_eq!(
            menu.items()
                .iter()
                .map(|item| item.label)
                .collect::<Vec<_>>(),
            vec!["Abrir", "Novo arquivo", "Nova pasta", "Renomear", "Excluir"]
        );

        press_ctrl(&mut application, KeyCode::Char('q'));
        assert!(!application.has_open_documents());
        assert!(application.is_running());
        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.context_menu().is_none());

        application.update(mouse_event(MouseEventKind::Down(MouseButton::Right), 2, 2));
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.tab_names(), vec!["a.txt"]);
        assert_eq!(text(&application), "A");
        assert!(!application.explorer_focused());
    }

    #[test]
    fn context_menu_dispatches_file_and_folder_creation() {
        let directory = TempDir::new("context-menu-create");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        application.update(mouse_event(MouseEventKind::Down(MouseButton::Right), 2, 2));
        press(&mut application, KeyCode::Down);
        press(&mut application, KeyCode::Enter);
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::NewFile));
        type_prompt(&mut application, "novo.txt");
        application.apply_action(Action::PromptConfirm);
        assert!(project.join("novo.txt").exists());

        let row = 2 + application
            .explorer()
            .entries()
            .iter()
            .position(|entry| entry.name == "novo.txt")
            .unwrap() as u16;
        application.update(mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            2,
            row,
        ));
        press(&mut application, KeyCode::Down);
        press(&mut application, KeyCode::Down);
        press(&mut application, KeyCode::Enter);
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::NewDirectory));
        type_prompt(&mut application, "pasta");
        application.apply_action(Action::PromptConfirm);
        assert!(project.join("pasta").is_dir());
    }

    #[test]
    fn context_menu_dispatches_rename_and_delete() {
        let directory = TempDir::new("context-menu-edit");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("a.txt"), "A").unwrap();
        fs::create_dir(project.join("pasta")).unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        let row = 2 + application
            .explorer()
            .entries()
            .iter()
            .position(|entry| entry.name == "pasta")
            .unwrap() as u16;
        application.update(mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            2,
            row,
        ));
        for _ in 0..3 {
            press(&mut application, KeyCode::Down);
        }
        press(&mut application, KeyCode::Enter);
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::Rename));
        for _ in 0.."pasta".chars().count() {
            application.apply_action(Action::PromptBackspace);
        }
        type_prompt(&mut application, "renomeada");
        application.apply_action(Action::PromptConfirm);
        assert!(!project.join("pasta").exists());
        assert!(project.join("renomeada").is_dir());

        let row = 2 + application
            .explorer()
            .entries()
            .iter()
            .position(|entry| entry.name == "a.txt")
            .unwrap() as u16;
        application.update(mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            2,
            row,
        ));
        for _ in 0..4 {
            press(&mut application, KeyCode::Down);
        }
        press(&mut application, KeyCode::Enter);
        assert_eq!(application.mode(), Mode::Confirm);
        assert_eq!(
            application.confirm().map(ConfirmDialog::kind),
            Some(ConfirmKind::Delete)
        );
        press(&mut application, KeyCode::Esc);
        assert!(project.join("a.txt").exists());
    }

    #[test]
    fn status_messages_age_out_and_clear_on_interaction() {
        let mut application = app("");

        application.set_info("informação");
        assert!(application.status_message().is_some());
        application.mark_rendered();
        press(&mut application, KeyCode::Char('a'));
        assert!(application.status_message().is_none());
        assert!(application.needs_redraw());

        application.set_info("informação");
        for _ in 0..STATUS_INFO_TICKS {
            application.on_idle();
        }
        assert!(application.status_message().is_none());

        application.set_error("erro");
        for _ in 0..STATUS_INFO_TICKS {
            application.on_idle();
        }
        assert!(application.status_message().is_some());
        for _ in 0..(STATUS_ERROR_TICKS - STATUS_INFO_TICKS) {
            application.on_idle();
        }
        assert!(application.status_message().is_none());
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
    fn session_restores_workspace_but_not_clean_documents() {
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
        // A sessão restaura o Workspace, mas não reabre documentos limpos.
        assert!(!restored.has_open_documents());
        assert_eq!(restored.tab_count(), 1);
        assert_eq!(text(&restored), "");

        let session = SessionStore::new(paths.session_file.clone())
            .load()
            .unwrap()
            .unwrap();
        assert!(session.documents.is_empty());
        assert_eq!(session.workspace.as_deref(), Some(project.as_path()));
    }

    #[test]
    fn abnormal_exit_restores_unsaved_recovery_but_not_clean_tabs() {
        let directory = TempDir::new("session-recovery-crash");
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
        // O fluxo real de abertura pelo Explorer devolve o foco ao editor.
        press(&mut application, KeyCode::Esc);
        let end = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(end))
            .unwrap();
        press(&mut application, KeyCode::Char('!'));
        application.on_idle();
        // Simula uma terminação anormal: a recuperação é gravada, mas a saída
        // normal (que limpa as cópias) não acontece.
        drop(application);

        let restored = App::with_paths(80, 24, paths);
        assert!(restored.workspace().is_some());
        assert!(restored.has_open_documents());
        assert_eq!(restored.tab_count(), 1);
        assert_eq!(text(&restored), "pub fn lib() {}!");
        assert!(restored.is_recovered());
        assert_eq!(restored.path(), Some(second.as_path()));
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
        drop(application);

        let restored = App::with_paths(80, 24, paths);
        assert!(restored.has_open_documents());
        assert_eq!(restored.tab_count(), 1);
        assert_eq!(text(&restored), "rascunho");
        assert!(restored.is_recovered());
        assert!(restored.path().is_none());
    }

    #[test]
    fn normal_quit_does_not_restore_unsaved_documents() {
        let directory = TempDir::new("normal-quit-empty");
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
        assert!(!restored.has_open_documents());
        assert_eq!(text(&restored), "");
    }

    #[test]
    fn closing_modified_tab_removes_it_from_recovery_and_session() {
        let directory = TempDir::new("closed-tab-session");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let path = project.join("document.txt");
        fs::write(&path, "abc").unwrap();
        let paths = isolated_paths(&config);

        let mut application = App::with_paths(80, 24, paths.clone());
        application.open_workspace(&project).unwrap();
        application.open_document_in_new_tab(&path).unwrap();
        // O fluxo real de abertura pelo Explorer devolve o foco ao editor.
        press(&mut application, KeyCode::Esc);
        let end = application.buffer().len_chars().unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(end))
            .unwrap();
        press(&mut application, KeyCode::Char('x'));
        application.on_idle();
        assert!(
            fs::read_dir(&paths.recovery_dir)
                .map(|entries| entries.flatten().count())
                .unwrap_or(0)
                > 0
        );

        press_ctrl(&mut application, KeyCode::Char('q'));
        press(&mut application, KeyCode::Char('d'));
        assert!(!application.has_open_documents());
        assert_eq!(
            fs::read_dir(&paths.recovery_dir)
                .map(|entries| entries.flatten().count())
                .unwrap_or(0),
            0
        );

        // Mesmo sem uma saída normal, a aba descartada não volta.
        drop(application);
        let restored = App::with_paths(80, 24, paths.clone());
        assert!(!restored.has_open_documents());
        assert_eq!(text(&restored), "");
        assert!(restored.workspace().is_some());

        let session = SessionStore::new(paths.session_file)
            .load()
            .unwrap()
            .unwrap();
        assert!(session.documents.is_empty());
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
        // O fluxo real de abertura pelo Explorer devolve o foco ao editor.
        press(&mut application, KeyCode::Esc);
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
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());
    }

    #[test]
    fn ctrl_b_closes_and_reopens_explorer_with_workspace() {
        let directory = TempDir::new("ctrl-b-toggle");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer().visible());
        assert!(application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(application.explorer().visible());
        assert!(application.explorer_focused());
        assert!(application.explorer_visible_width().is_some());
    }

    #[test]
    fn f6_opens_and_closes_problems_panel_and_shift_f6_focuses_it() {
        let mut application = app("abc");
        assert_eq!(application.problems_panel_height(), 0);
        press(&mut application, KeyCode::F(6));
        assert_eq!(application.problems_panel_height(), 6);
        assert!(application.problems_focused());
        press(&mut application, KeyCode::F(6));
        assert_eq!(application.problems_panel_height(), 0);
        press_shift(&mut application, KeyCode::F(6));
        assert_eq!(application.problems_panel_height(), 6);
        assert!(application.problems_focused());
        press(&mut application, KeyCode::Tab);
        assert!(!application.problems_focused());
        press(&mut application, KeyCode::F(6));
        assert_eq!(application.problems_panel_height(), 0);
    }

    #[test]
    fn small_terminal_keeps_editor_space_when_problems_panel_is_open() {
        let mut application = app("abc");
        press(&mut application, KeyCode::F(6));
        application.update(Event::Resize {
            width: 80,
            height: 5,
        });
        assert_eq!(application.problems_panel_height(), 0);
        assert!(application.view.content_size().1 > 0);
    }

    #[test]
    fn problem_panel_filters_and_opens_an_unopened_workspace_document_at_diagnostic() {
        use crate::diagnostics::ProblemFilter;
        use nexus_lsp::protocol::{Diagnostic, DiagnosticSeverity, Position, Range};
        use std::collections::BTreeMap;

        let directory = TempDir::new("problems-navigation");
        let project = directory.join("project");
        fs::create_dir_all(&project).unwrap();
        let source = project.join("src.rs");
        fs::write(&source, "let value = 1;\n").unwrap();
        let mut application = app("other");
        application.open_workspace(&project).unwrap();
        let uri = nexus_lsp::uri::path_to_uri(&source);
        let workspace = application
            .workspace()
            .unwrap()
            .root()
            .to_string_lossy()
            .into_owned();
        application.diagnostics = vec![StoredDiagnostics {
            key: crate::document_sync::DiagnosticsKey {
                workspace,
                language_id: "rust".into(),
                session_id: 1,
                uri: uri.clone(),
            },
            current_document_version: Some(1),
            publication: nexus_lsp::protocol::PublishDiagnosticsParams {
                uri,
                version: Some(1),
                extra: BTreeMap::new(),
                diagnostics: vec![Diagnostic {
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 4,
                        },
                        end: Position {
                            line: 0,
                            character: 9,
                        },
                    },
                    severity: Some(DiagnosticSeverity::Error),
                    code: Some(serde_json::json!("E1")),
                    source: Some("test".into()),
                    message: "diagnóstico de teste".into(),
                    code_description: None,
                    tags: Vec::new(),
                    related_information: Vec::new(),
                    data: None,
                    extra: BTreeMap::new(),
                }],
            },
            freshness: DiagnosticFreshness::Current,
        }];
        application.normalize_problem_selection();
        assert_eq!(application.problem_items().len(), 1);
        press(&mut application, KeyCode::F(6));
        press_shift(&mut application, KeyCode::F(6));
        press(&mut application, KeyCode::Char('2'));
        assert_eq!(application.problems_filter(), ProblemFilter::Warnings);
        assert!(application.problem_items().is_empty());
        press(&mut application, KeyCode::Char('1'));
        assert_eq!(application.problem_items().len(), 1);
        let selected_identity = application.selected_problem().unwrap();
        application.diagnostics[0].publication.diagnostics[0].message =
            "diagnóstico atualizado".into();
        application.normalize_problem_selection();
        assert_eq!(application.selected_problem(), Some(selected_identity));
        press(&mut application, KeyCode::Enter);
        assert_eq!(application.path(), Some(source.as_path()));
        assert_eq!(application.buffer().text().unwrap(), "let value = 1;\n");
        assert_eq!(application.view.cursor().0, 4);
        assert!(!application.problems_focused());
        application.diagnostics.clear();
        application.normalize_problem_selection();
        assert!(application.selected_problem().is_none());
    }

    #[test]
    fn ctrl_b_works_with_editor_focus() {
        let directory = TempDir::new("ctrl-b-editor-focus");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        press(&mut application, KeyCode::Esc);
        assert!(application.explorer().visible());
        assert!(!application.explorer_focused());

        // Com foco no editor, Ctrl+B continua fechando a sidebar.
        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());

        // E reabre com foco no explorador, sem depender do editor.
        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(application.explorer().visible());
        assert!(application.explorer_focused());
    }

    #[test]
    fn ctrl_b_works_with_explorer_focus() {
        let directory = TempDir::new("ctrl-b-explorer-focus");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut application = app("");

        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('b'));

        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());
    }

    #[test]
    fn ctrl_b_without_workspace_orients_user_without_creating_paths() {
        let directory = TempDir::new("ctrl-b-no-workspace");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = isolated_paths(&config);
        let missing_root = paths.root.join("projeto");
        let mut application = App::with_paths(80, 24, paths);

        assert!(application.explorer_visible_width().is_none());
        application.mark_rendered();

        press_ctrl(&mut application, KeyCode::Char('b'));

        assert!(application.workspace().is_none());
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());
        assert!(!missing_root.exists());
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("abra um projeto")
        );
        assert!(application.needs_redraw());
    }

    #[test]
    fn ctrl_b_on_start_screen_orients_user_to_open_a_project() {
        let directory = TempDir::new("ctrl-b-start-screen");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = isolated_paths(&config);
        let mut application = App::with_paths(80, 24, paths);

        application.show_start_screen();
        application.mark_rendered();

        press_ctrl(&mut application, KeyCode::Char('b'));

        assert!(application.start_screen().is_some());
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("abra um projeto")
        );
        assert!(application.needs_redraw());
    }

    #[test]
    fn explorer_adapts_to_narrow_terminals_without_starving_editor() {
        let directory = TempDir::new("explorer-adaptive");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut application = app("");

        application.update(Event::Resize {
            width: 36,
            height: 10,
        });
        application.open_workspace(&project).unwrap();

        let sidebar = application.explorer_visible_width().unwrap();
        assert!(sidebar >= crate::explorer::MIN_WIDTH);
        assert!(36 - sidebar >= MIN_EDITOR_WIDTH);

        application.update(Event::Resize {
            width: 30,
            height: 10,
        });
        assert!(application.explorer_visible_width().is_none());
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());

        application.mark_rendered();
        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(!application.explorer().visible());
        assert!(
            application
                .status_message()
                .unwrap()
                .text
                .contains("terminal estreito")
        );
    }

    #[test]
    fn restored_explorer_session_keeps_visibility_and_focus_consistent() {
        let directory = TempDir::new("explorer-session-focus");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let paths = isolated_paths(&config);

        let mut application = App::with_paths(80, 24, paths.clone());
        application.open_workspace(&project).unwrap();
        assert!(application.explorer().visible());
        assert!(application.explorer_focused());
        application.persist_before_quit();
        drop(application);

        let restored = App::with_paths(80, 24, paths);

        assert!(restored.workspace().is_some());
        assert!(restored.explorer().visible());
        assert!(restored.explorer_focused());
        assert!(restored.explorer_visible_width().is_some());
    }

    #[test]
    fn restored_explorer_without_workspace_is_closed_and_unfocused() {
        let directory = TempDir::new("explorer-session-no-workspace");
        let config = directory.join("config");
        fs::create_dir_all(&config).unwrap();
        let paths = isolated_paths(&config);
        let mut session = Session::default();
        session.explorer.visible = true;
        SessionStore::new(paths.session_file.clone())
            .save(&session)
            .unwrap();

        let application = App::with_paths(80, 24, paths);

        assert!(application.workspace().is_none());
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());
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
            kind: MouseEventKind::Down(MouseButton::Left),
            column: initial - 1,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }));
        application.update(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 24,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }));
        application.update(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
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

    // ----- Seletor nativo -----

    fn enable_native_picker(app: &mut App) {
        app.native_picker = true;
        app.picker_interactive = true;
    }

    fn ctrl_shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
    }

    #[test]
    fn ctrl_o_requests_native_file_selection() {
        let mut application = app("");
        enable_native_picker(&mut application);

        application.update(Event::Key(ctrl(KeyCode::Char('o'))));

        let request = application.take_picker_request().unwrap();
        assert_eq!(request.kind, PickerKind::Files);
        assert!(request.start_directory.is_none());
        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.prompt().is_none());
    }

    #[test]
    fn ctrl_shift_o_requests_native_directory_selection() {
        let mut application = app("");
        enable_native_picker(&mut application);

        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));

        let request = application.take_picker_request().unwrap();
        assert_eq!(request.kind, PickerKind::Directory);
        assert!(request.start_directory.is_none());
        assert_eq!(application.mode(), Mode::Editor);
    }

    #[test]
    fn open_picker_actions_dispatch_to_the_correct_picker_kind() {
        let mut files = app("");
        enable_native_picker(&mut files);
        files.apply_action(Action::OpenFilePicker);
        assert_eq!(
            files.take_picker_request().map(|request| request.kind),
            Some(PickerKind::Files)
        );

        let mut directory = app("");
        enable_native_picker(&mut directory);
        directory.apply_action(Action::OpenWorkspacePicker);
        assert_eq!(
            directory.take_picker_request().map(|request| request.kind),
            Some(PickerKind::Directory)
        );
    }

    #[test]
    fn crossterm_shift_removed_representation_requests_directory_selection() {
        let mut application = app("");
        enable_native_picker(&mut application);

        // Protocolos estendidos podem remover SHIFT e entregar 'O' com CONTROL.
        press_ctrl(&mut application, KeyCode::Char('O'));

        let request = application.take_picker_request().unwrap();
        if cfg!(windows) {
            // No Windows esse evento é Ctrl+O sob Caps Lock.
            assert_eq!(request.kind, PickerKind::Files);
        } else {
            assert_eq!(request.kind, PickerKind::Directory);
        }
    }

    #[test]
    fn f3_requests_native_directory_selection_as_fallback() {
        let mut application = app("");
        enable_native_picker(&mut application);

        press(&mut application, KeyCode::F(3));

        let request = application.take_picker_request().unwrap();
        assert_eq!(request.kind, PickerKind::Directory);
        assert_eq!(application.mode(), Mode::Editor);
    }

    #[test]
    fn f3_works_from_the_start_screen_without_opening_files() {
        let mut application = app("");
        enable_native_picker(&mut application);
        application.show_start_screen();

        press(&mut application, KeyCode::F(3));

        assert!(application.start_screen().is_some());
        let request = application.take_picker_request().unwrap();
        assert_eq!(request.kind, PickerKind::Directory);

        application.complete_picker(None);
        assert!(application.start_screen().is_some());
    }

    #[test]
    fn picker_shortcuts_work_with_explorer_focus() {
        let directory = TempDir::new("picker-explorer-focus");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();

        let mut application = app("");
        enable_native_picker(&mut application);
        application.open_workspace(&project).unwrap();
        assert!(application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('o'));
        assert_eq!(
            application
                .take_picker_request()
                .map(|request| request.kind),
            Some(PickerKind::Files)
        );
        application.complete_picker(None);

        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));
        assert_eq!(
            application
                .take_picker_request()
                .map(|request| request.kind),
            Some(PickerKind::Directory)
        );
        assert!(application.explorer_focused());
    }

    #[test]
    fn picker_shortcuts_work_from_the_help_overlay() {
        let mut application = app("");
        enable_native_picker(&mut application);

        press_ctrl(&mut application, KeyCode::Char('h'));
        assert_eq!(application.mode(), Mode::Help);

        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(
            application
                .take_picker_request()
                .map(|request| request.kind),
            Some(PickerKind::Directory)
        );
    }

    #[test]
    fn picker_shortcuts_are_isolated_while_a_confirmation_is_open() {
        let mut application = modified_app("abc");
        press_quit(&mut application);
        assert_eq!(application.mode(), Mode::Confirm);

        press_ctrl(&mut application, KeyCode::Char('o'));
        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));

        assert!(application.take_picker_request().is_none());
        assert_eq!(application.mode(), Mode::Confirm);
    }

    #[test]
    fn picker_shortcuts_are_isolated_while_a_path_prompt_is_open() {
        let mut application = app("");

        // Sem seletor nativo, Ctrl+O abre o prompt manual de caminho.
        press_ctrl(&mut application, KeyCode::Char('o'));
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::Open));

        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));

        assert!(application.take_picker_request().is_none());
        assert_eq!(application.mode(), Mode::Prompt(PromptKind::Open));
    }

    #[test]
    fn picker_selection_opens_multiple_files_in_document_manager() {
        let directory = TempDir::new("picker-files");
        let first = directory.join("primeiro arquivo.rs");
        let second = directory.join("界 café.txt");
        fs::write(&first, "fn main() {}\n").unwrap();
        fs::write(&second, "conteúdo").unwrap();

        let mut application = app("");
        application.complete_picker(Some(PickerSelection::Files(vec![
            first.clone(),
            second.clone(),
        ])));

        assert_eq!(application.tab_count(), 2);
        let paths: Vec<_> = application
            .tabs()
            .into_iter()
            .filter_map(|tab| tab.path)
            .collect();
        assert!(paths.contains(&fs::canonicalize(&first).unwrap()));
        assert!(paths.contains(&fs::canonicalize(&second).unwrap()));
        assert_eq!(text(&application), "conteúdo");
    }

    #[test]
    fn picker_selection_does_not_replace_already_open_documents() {
        let directory = TempDir::new("picker-duplicates");
        let first = directory.join("a.txt");
        let second = directory.join("b.txt");
        fs::write(&first, "a").unwrap();
        fs::write(&second, "b").unwrap();

        let mut application = app("");
        application.open_document_in_new_tab(&first).unwrap();
        application.open_document_in_new_tab(&second).unwrap();
        let tabs = application.tab_count();

        application.complete_picker(Some(PickerSelection::Files(vec![
            first.clone(),
            second.clone(),
        ])));

        assert_eq!(application.tab_count(), tabs);
        assert_eq!(text(&application), "b");
    }

    #[test]
    fn picker_directory_opens_workspace_and_updates_explorer() {
        let directory = TempDir::new("picker-workspace");
        let project = directory.join("projeto com espaço");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("arquivo café.txt");
        fs::write(&file, "x").unwrap();

        let mut application = app("");
        application.complete_picker(Some(PickerSelection::Directory(project.clone())));

        let root = fs::canonicalize(&project).unwrap();
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert_eq!(application.explorer().root(), root.as_path());
        assert!(application.explorer().visible());
        assert!(application.explorer_focused());
        assert!(
            application
                .explorer()
                .entries()
                .iter()
                .any(|entry| entry.name == "arquivo café.txt")
        );
    }

    #[test]
    fn picker_directory_accepts_an_empty_workspace() {
        let directory = TempDir::new("picker-empty-workspace");
        let project = directory.join("vazio");
        fs::create_dir_all(&project).unwrap();

        let mut application = app("");
        application.complete_picker(Some(PickerSelection::Directory(project.clone())));

        let root = fs::canonicalize(&project).unwrap();
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert!(application.explorer().visible());
        assert!(application.explorer_focused());
        assert!(application.explorer().entries().is_empty());
    }

    #[test]
    fn picker_workspace_respects_gitignore_and_hidden_files() {
        let directory = TempDir::new("picker-ignore");
        let project = directory.join("projeto");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join(".gitignore"), "ignorado.txt\ntarget/\n").unwrap();
        fs::write(project.join("visivel.txt"), "x").unwrap();
        fs::write(project.join("ignorado.txt"), "x").unwrap();
        fs::write(project.join(".oculto"), "x").unwrap();
        fs::write(project.join("target").join("build.txt"), "x").unwrap();

        let mut application = app("");
        application.complete_picker(Some(PickerSelection::Directory(project)));

        let names = |application: &App| -> Vec<String> {
            application
                .explorer()
                .entries()
                .iter()
                .map(|entry| entry.name.clone())
                .collect()
        };

        let entries = names(&application);
        assert!(entries.contains(&"visivel.txt".to_owned()));
        assert!(!entries.contains(&"ignorado.txt".to_owned()));
        assert!(!entries.contains(&".oculto".to_owned()));
        assert!(!entries.contains(&"target".to_owned()));

        application.apply_action(Action::ToggleHidden);
        assert!(names(&application).contains(&".oculto".to_owned()));
    }

    #[test]
    fn opening_another_workspace_switches_explorer_and_keeps_documents() {
        let directory = TempDir::new("picker-switch");
        let first_project = directory.join("primeiro");
        let second_project = directory.join("segundo");
        fs::create_dir_all(&first_project).unwrap();
        fs::create_dir_all(&second_project).unwrap();
        let first_file = first_project.join("a.txt");
        fs::write(&first_file, "a").unwrap();
        fs::write(second_project.join("b.txt"), "b").unwrap();

        let mut application = app("");
        application.open_workspace(&first_project).unwrap();
        application.open_document_in_new_tab(&first_file).unwrap();

        application.complete_picker(Some(PickerSelection::Directory(second_project.clone())));

        let root = fs::canonicalize(&second_project).unwrap();
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert_eq!(application.explorer().root(), root.as_path());
        assert!(
            application
                .explorer()
                .entries()
                .iter()
                .any(|entry| entry.name == "b.txt")
        );
        assert!(
            !application
                .explorer()
                .entries()
                .iter()
                .any(|entry| entry.name == "a.txt")
        );
        assert_eq!(
            application.path(),
            Some(fs::canonicalize(&first_file).unwrap().as_path())
        );
    }

    #[test]
    fn cancelling_picker_preserves_workspace_documents_and_status() {
        let directory = TempDir::new("picker-cancel");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("a.txt");
        fs::write(&file, "abc").unwrap();

        let mut application = app("");
        application.open_workspace(&project).unwrap();
        application.open_document_in_new_tab(&file).unwrap();
        let root = application.workspace().unwrap().root().to_path_buf();
        let path = application.path().map(Path::to_path_buf);
        let tabs = application.tab_count();
        let status = application.status_message().cloned();

        application.complete_picker(None);

        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert_eq!(application.path(), path.as_deref());
        assert_eq!(application.tab_count(), tabs);
        assert_eq!(application.status_message(), status.as_ref());
    }

    #[test]
    fn picker_cancel_on_start_screen_preserves_the_start_screen() {
        let mut application = app("");
        enable_native_picker(&mut application);
        application.show_start_screen();

        application.update(Event::Key(ctrl(KeyCode::Char('o'))));
        assert!(application.start_screen().is_some());

        let request = application.take_picker_request().unwrap();
        assert_eq!(request.kind, PickerKind::Files);

        application.complete_picker(None);
        assert!(application.start_screen().is_some());
    }

    #[test]
    fn cancelling_the_directory_shortcut_preserves_the_current_workspace() {
        let directory = TempDir::new("picker-shortcut-cancel");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("a.txt");
        fs::write(&file, "abc").unwrap();

        let mut application = app("");
        enable_native_picker(&mut application);
        application.open_workspace(&project).unwrap();
        application.open_document_in_new_tab(&file).unwrap();
        let root = application.workspace().unwrap().root().to_path_buf();
        let path = application.path().map(Path::to_path_buf);
        let tabs = application.tab_count();

        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));
        assert_eq!(
            application
                .take_picker_request()
                .map(|request| request.kind),
            Some(PickerKind::Directory)
        );

        application.complete_picker(None);

        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert_eq!(application.path(), path.as_deref());
        assert_eq!(application.tab_count(), tabs);
    }

    #[test]
    fn picker_error_opens_manual_workspace_prompt_and_keeps_workspace() {
        let directory = TempDir::new("picker-error");
        let project = directory.join("anterior");
        fs::create_dir_all(&project).unwrap();

        let mut application = app("");
        application.open_workspace(&project).unwrap();
        let root = application.workspace().unwrap().root().to_path_buf();

        application.report_picker_error(
            PickerKind::Directory,
            &crate::picker::FilePickerError::Unavailable("sem ambiente gráfico".to_owned()),
        );

        assert_eq!(application.mode(), Mode::Prompt(PromptKind::OpenWorkspace));
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
    }

    #[test]
    fn ctrl_shift_o_uses_manual_prompt_when_native_picker_is_disabled() {
        let directory = TempDir::new("picker-manual-workspace");
        let project = directory.join("manual");
        fs::create_dir_all(&project).unwrap();

        let mut application = app("");
        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('O'))));

        assert_eq!(application.mode(), Mode::Prompt(PromptKind::OpenWorkspace));
        type_prompt(&mut application, &project.to_string_lossy());
        application.apply_action(Action::PromptConfirm);

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(fs::canonicalize(&project).unwrap().as_path())
        );
    }

    #[test]
    fn ctrl_b_still_toggles_the_explorer_after_picker_changes() {
        let directory = TempDir::new("picker-ctrl-b");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();

        let mut application = app("");
        application.open_workspace(&project).unwrap();
        assert!(application.explorer().visible());

        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());

        press_ctrl(&mut application, KeyCode::Char('b'));
        assert!(application.explorer().visible());
        assert!(application.explorer_focused());

        application.update(Event::Key(ctrl_shift_key(KeyCode::Char('B'))));
        assert!(!application.explorer().visible());
        assert!(!application.explorer_focused());
    }

    #[derive(Clone, Default)]
    struct RecordingSyncSink(Arc<Mutex<Vec<crate::document_sync::SyncEvent>>>);

    impl SyncSink for RecordingSyncSink {
        fn handle(&mut self, event: crate::document_sync::SyncEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[derive(Default)]
    struct NavigationSinkState {
        next_id: u64,
        session_id: i64,
        completions: Vec<NavigationCompletion>,
        requested: Vec<(String, String, NavigationMethod, String, Position)>,
        cancelled: Vec<NavigationOperationId>,
        opened: Vec<PathBuf>,
    }

    #[derive(Clone, Default)]
    struct NavigationTestSink(Arc<Mutex<NavigationSinkState>>);

    impl SyncSink for NavigationTestSink {
        fn handle(&mut self, event: crate::document_sync::SyncEvent) {
            if let crate::document_sync::SyncEvent::Open { path, .. } = event {
                self.0.lock().unwrap().opened.push(path);
            }
        }

        fn drain_navigation_results(&mut self) -> Vec<NavigationCompletion> {
            std::mem::take(&mut self.0.lock().unwrap().completions)
        }

        fn request_navigation(
            &mut self,
            workspace: String,
            language_id: String,
            method: NavigationMethod,
            uri: String,
            position: Position,
        ) -> Option<NavigationOperationId> {
            let mut state = self.0.lock().unwrap();
            state.next_id += 1;
            state
                .requested
                .push((workspace, language_id, method, uri, position));
            Some(NavigationOperationId(state.next_id))
        }

        fn current_session_id(&mut self, _workspace: &str, _language_id: &str) -> Option<i64> {
            Some(self.0.lock().unwrap().session_id)
        }

        fn cancel_navigation(&mut self, operation_id: NavigationOperationId) {
            self.0.lock().unwrap().cancelled.push(operation_id);
        }
    }

    fn complete_navigation(
        sink: &Arc<Mutex<NavigationSinkState>>,
        context: &NavigationOriginContext,
        outcome: NavigationOutcome,
    ) {
        sink.lock().unwrap().completions.push(NavigationCompletion {
            context: nexus_lsp::service::NavigationRequestContext {
                operation_id: context.operation_id,
                workspace: context.workspace.clone(),
                language_id: context.language_id.clone(),
                session_id: context.session_id,
                method: context.method,
                uri: context.uri.clone(),
                document_version: Some(context.document_revision as i64),
                position: context.position,
            },
            outcome,
        });
    }

    fn destination(uri: &str, line: u32, character: u32) -> NavigationTarget {
        let range = nexus_lsp::protocol::Range {
            start: Position { line, character },
            end: Position {
                line,
                character: character + 1,
            },
        };
        NavigationTarget {
            uri: uri.to_owned(),
            target_range: range,
            selection_range: range,
            origin_selection_range: None,
        }
    }

    fn request_definition(application: &mut App) -> NavigationOriginContext {
        press(application, KeyCode::F(12));
        let operation = application
            .active_navigation
            .expect("F12 deveria registrar uma operação");
        application
            .navigation_context(operation)
            .cloned()
            .expect("contexto da operação")
    }

    fn request_references(application: &mut App) -> NavigationOriginContext {
        press_shift(application, KeyCode::F(12));
        let operation = application
            .active_navigation
            .expect("Shift+F12 deveria registrar uma operação");
        application
            .navigation_context(operation)
            .cloned()
            .expect("contexto da operação")
    }

    fn request_declaration(application: &mut App) -> NavigationOriginContext {
        press_ctrl(application, KeyCode::F(12));
        let operation = application
            .active_navigation
            .expect("Ctrl+F12 deveria registrar uma operação");
        application
            .navigation_context(operation)
            .cloned()
            .expect("contexto da operação")
    }

    fn request_type_definition(application: &mut App) -> NavigationOriginContext {
        press_ctrl(application, KeyCode::Char('t'));
        let operation = application
            .active_navigation
            .expect("Ctrl+T deveria registrar uma operação");
        application
            .navigation_context(operation)
            .cloned()
            .expect("contexto da operação")
    }

    fn finish_navigation(
        application: &mut App,
        state: &Arc<Mutex<NavigationSinkState>>,
        origin: &NavigationOriginContext,
        outcome: NavigationOutcome,
    ) {
        complete_navigation(state, origin, outcome);
        application.on_idle();
    }

    #[test]
    fn navigation_captures_document_identity_and_origin_position() {
        let directory = TempDir::new("navigation-capture");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(3))
            .unwrap();

        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let context = application.navigation_context(operation).unwrap();

        assert_eq!(context.operation_id, operation);
        assert_eq!(context.method, NavigationMethod::Definition);
        assert_eq!(context.session_id, Some(0));
        assert_eq!(context.document_revision, application.buffer.revision());
        assert_eq!(context.uri, nexus_lsp::uri::path_to_uri(&path));
        assert_eq!(
            context.position,
            Position {
                line: 0,
                character: 3
            }
        );
        assert_eq!(state.lock().unwrap().requested.len(), 1);
    }

    #[test]
    fn cursor_movement_marks_an_arriving_result_stale_without_applying_it() {
        let directory = TempDir::new("navigation-cursor-stale");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(3))
            .unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let context = application.navigation_context(operation).unwrap().clone();

        application
            .view
            .set_cursor(&application.buffer, CharOffset(4))
            .unwrap();
        complete_navigation(
            &state,
            &context,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        let results = application.take_navigation_results();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].origin.position, context.position);
        assert_eq!(
            results[0].context_state,
            NavigationContextState::Stale(vec![NavigationStaleReason::CursorMoved,])
        );
        assert!(matches!(
            results[0].status,
            NavigationOperationStatus::Response { .. }
        ));
        assert_eq!(application.view.cursor(), CharOffset(4));
        assert!(application.navigation_context(operation).is_none());
    }

    #[test]
    fn editing_keeps_the_original_revision_and_marks_the_result_stale() {
        let directory = TempDir::new("navigation-edit-stale");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let context = application.navigation_context(operation).unwrap().clone();

        application.insert_text("// edit\n", EditKind::Insert);
        application.commit_session();
        complete_navigation(
            &state,
            &context,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        let results = application.take_navigation_results();

        assert_eq!(
            results[0].origin.document_revision,
            context.document_revision
        );
        assert!(matches!(
            &results[0].context_state,
            NavigationContextState::Stale(reasons)
                if reasons.contains(&NavigationStaleReason::DocumentEdited)
        ));
    }

    #[test]
    fn closing_origin_document_invalidates_pending_navigation() {
        let directory = TempDir::new("navigation-close");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();

        application.close_tab_now(application.documents.active_index());

        let results = application.take_navigation_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].origin.operation_id, operation);
        assert_eq!(
            results[0].context_state,
            NavigationContextState::OriginDocumentMissing
        );
        assert_eq!(results[0].status, NavigationOperationStatus::Cancelled);
    }

    #[test]
    fn switching_tabs_keeps_the_operation_bound_to_its_original_document() {
        let directory = TempDir::new("navigation-tab-context");
        let first_path = directory.join("first.rs");
        let second_path = directory.join("second.rs");
        fs::write(&first_path, "fn first() {}\n").unwrap();
        fs::write(&second_path, "fn second() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first_path).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let origin = application.navigation_context(operation).unwrap().clone();
        application.open_document_in_new_tab(&second_path).unwrap();

        complete_navigation(
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        let result = application.take_navigation_results().remove(0);

        assert_eq!(result.origin.document_id, origin.document_id);
        assert_eq!(
            result.context_state,
            NavigationContextState::Stale(vec![NavigationStaleReason::ActiveDocumentChanged])
        );
        assert_eq!(application.path(), Some(second_path.as_path()));
    }

    #[test]
    fn workspace_switch_marks_result_stale_without_switching_workspace() {
        let directory = TempDir::new("navigation-workspace-context");
        let first_workspace = directory.join("first");
        let second_workspace = directory.join("second");
        fs::create_dir_all(&first_workspace).unwrap();
        fs::create_dir_all(&second_workspace).unwrap();
        let file = first_workspace.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_workspace(&first_workspace).unwrap();
        application.open_document_in_new_tab(&file).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let origin = application.navigation_context(operation).unwrap().clone();
        application.open_workspace(&second_workspace).unwrap();

        complete_navigation(
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        let result = application.take_navigation_results().remove(0);

        assert_eq!(
            result.context_state,
            NavigationContextState::Stale(vec![NavigationStaleReason::WorkspaceChanged])
        );
        assert_eq!(
            application.workspace().unwrap().root(),
            fs::canonicalize(&second_workspace).unwrap()
        );
    }

    #[test]
    fn replacing_the_server_session_is_reported_separately() {
        let directory = TempDir::new("navigation-session-context");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&file).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let origin = application.navigation_context(operation).unwrap().clone();
        state.lock().unwrap().session_id = 1;

        complete_navigation(
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        let result = application.take_navigation_results().remove(0);

        assert_eq!(
            result.context_state,
            NavigationContextState::SessionReplaced
        );
        assert!(matches!(
            result.status,
            NavigationOperationStatus::Response { .. }
        ));
    }

    #[test]
    fn mismatched_response_uri_is_not_accepted_as_the_origin_context() {
        let directory = TempDir::new("navigation-context-mismatch");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&file).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let origin = application.navigation_context(operation).unwrap().clone();
        state
            .lock()
            .unwrap()
            .completions
            .push(NavigationCompletion {
                context: nexus_lsp::service::NavigationRequestContext {
                    operation_id: operation,
                    workspace: origin.workspace.clone(),
                    language_id: origin.language_id.clone(),
                    session_id: origin.session_id,
                    method: origin.method,
                    uri: "file:///different/document.rs".to_owned(),
                    document_version: None,
                    position: origin.position,
                },
                outcome: NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
            });

        let result = application.take_navigation_results().remove(0);

        assert_eq!(
            result.context_state,
            NavigationContextState::ContextMismatch
        );
        assert!(matches!(
            result.status,
            NavigationOperationStatus::Response { .. }
        ));
    }

    #[test]
    fn manual_cancellation_is_terminal_and_late_response_is_ignored() {
        let directory = TempDir::new("navigation-cancel");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&file).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let origin = application.navigation_context(operation).unwrap().clone();

        assert!(application.cancel_navigation(operation));
        complete_navigation(
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        let results = application.take_navigation_results();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, NavigationOperationStatus::Cancelled);
        assert_eq!(results[0].context_state, NavigationContextState::Current);
        assert!(!application.cancel_navigation(operation));
    }

    #[test]
    fn timeout_is_preserved_as_a_typed_terminal_state() {
        let directory = TempDir::new("navigation-timeout");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&file).unwrap();
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let origin = application.navigation_context(operation).unwrap().clone();
        complete_navigation(
            &state,
            &origin,
            NavigationOutcome::TimedOut {
                duration: Duration::from_secs(2),
            },
        );

        let result = application.take_navigation_results().remove(0);
        assert_eq!(
            result.status,
            NavigationOperationStatus::TimedOut {
                duration: Duration::from_secs(2),
            }
        );
        assert!(application.take_navigation_results().is_empty());
    }

    #[test]
    fn external_unopened_target_is_preserved_without_opening_it() {
        let directory = TempDir::new("navigation-external-target");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&file).unwrap();
        let original_path = application.path().map(Path::to_path_buf);
        let operation = application
            .request_navigation(NavigationMethod::Definition)
            .unwrap();
        let origin = application.navigation_context(operation).unwrap().clone();
        let destination = nexus_lsp::protocol::NavigationTarget {
            uri: "file:///external/dependency/src/lib.rs".to_owned(),
            target_range: nexus_lsp::protocol::Range {
                start: Position {
                    line: 8,
                    character: 0,
                },
                end: Position {
                    line: 8,
                    character: 12,
                },
            },
            selection_range: nexus_lsp::protocol::Range {
                start: Position {
                    line: 8,
                    character: 3,
                },
                end: Position {
                    line: 8,
                    character: 7,
                },
            },
            origin_selection_range: Some(nexus_lsp::protocol::Range {
                start: origin.position,
                end: origin.position,
            }),
        };
        complete_navigation(
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination.clone(),
            ])),
        );

        let result = application.take_navigation_results().remove(0);

        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(matches!(
            result.status,
            NavigationOperationStatus::Response {
                result: nexus_lsp::protocol::NavigationResult::Destinations(ref targets)
            } if targets == &[destination]
        ));
    }

    #[test]
    fn f12_applies_single_same_file_definition_and_records_history() {
        let directory = TempDir::new("definition-same-file");
        let path = directory.join("main.rs");
        let text = "fn main() {\n    target();\n}\n\nfn target() {}\n";
        fs::write(&path, text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(3))
            .unwrap();

        let origin = request_definition(&mut application);
        let requested = state.lock().unwrap().requested.clone();
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].2, NavigationMethod::Definition);
        assert_eq!(requested[0].4, origin.position);
        assert_eq!(
            application
                .status_message()
                .map(|message| message.text.as_str()),
            Some("procurando definição...")
        );

        let target = destination(&nexus_lsp::uri::path_to_uri(&path), 4, 3);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                target.clone(),
            ])),
        );

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(
            application
                .path()
                .map(fs::canonicalize)
                .transpose()
                .unwrap(),
            Some(fs::canonicalize(&path).unwrap())
        );
        assert_eq!(
            application
                .view
                .cursor_line(&application.buffer)
                .expect("linha do cursor"),
            4
        );
        assert_eq!(application.navigation_history().len(), 1);
        let history = &application.navigation_history()[0];
        assert_eq!(history.origin.document_id, origin.document_id);
        assert_eq!(history.origin.cursor, CharOffset(3));
        assert_eq!(history.origin.uri, origin.uri);
        assert_eq!(history.target.uri, target.uri);
        assert_eq!(history.target.cursor, application.view.cursor());
        assert_eq!(history.target.document_id, application.document_id);
    }

    #[test]
    fn single_definition_in_another_file_reuses_tabs_and_preserves_content() {
        let directory = TempDir::new("definition-other-file");
        let main = directory.join("main.rs");
        let definitions = directory.join("definitions.rs");
        let main_text = "fn main() {\n    target();\n}\n";
        let definitions_text = "fn helper() {}\nfn target() {}\n";
        fs::write(&main, main_text).unwrap();
        fs::write(&definitions, definitions_text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&main).unwrap();

        let origin = request_definition(&mut application);
        let target = destination(&nexus_lsp::uri::path_to_uri(&definitions), 1, 3);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                target.clone(),
            ])),
        );

        assert_eq!(application.tab_count(), 2);
        assert_eq!(
            application
                .path()
                .map(fs::canonicalize)
                .transpose()
                .unwrap(),
            Some(fs::canonicalize(&definitions).unwrap())
        );
        assert_eq!(
            application
                .view
                .cursor_line(&application.buffer)
                .expect("linha do cursor"),
            1
        );

        // Alterações locais no destino já aberto nunca são sobrescritas.
        application.insert_text("// local\n", EditKind::Insert);
        application.commit_session();
        let definitions_local = application.buffer().text().unwrap();
        assert!(application.buffer().is_modified());

        let main_index = application
            .tabs()
            .into_iter()
            .find(|tab| {
                tab.path
                    .as_deref()
                    .is_some_and(|path| fs::canonicalize(path).ok() == fs::canonicalize(&main).ok())
            })
            .map(|tab| tab.index)
            .expect("aba principal");
        assert!(application.switch_tab(main_index));
        assert_eq!(application.buffer().text().unwrap(), main_text);
        assert!(!application.buffer().is_modified());

        // Uma segunda navegação para o mesmo arquivo deve reutilizar a aba.
        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                target,
            ])),
        );
        assert_eq!(application.tab_count(), 2);
        assert_eq!(application.navigation_history().len(), 2);
        assert_eq!(application.buffer().text().unwrap(), definitions_local);
        assert!(application.buffer().is_modified());
    }

    #[test]
    fn definition_outside_the_workspace_opens_without_changing_workspace_or_lsp_routes() {
        let directory = TempDir::new("definition-external-workspace");
        let workspace = directory.join("projeto");
        let external = directory.join("fora");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&external).unwrap();
        let main = workspace.join("main.rs");
        let dependency = external.join("lib.rs");
        fs::write(&main, "fn main() {}\n").unwrap();
        fs::write(&dependency, "pub fn externo() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_workspace(&workspace).unwrap();
        application.open_document_in_new_tab(&main).unwrap();
        let root = application.workspace().unwrap().root().to_path_buf();

        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&dependency), 0, 7),
            ])),
        );

        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert_eq!(
            application
                .path()
                .map(fs::canonicalize)
                .transpose()
                .unwrap(),
            Some(fs::canonicalize(&dependency).unwrap())
        );
        assert_eq!(application.tab_count(), 2);
        let opened = state.lock().unwrap().opened.clone();
        assert!(
            opened
                .iter()
                .any(|path| { fs::canonicalize(path).ok() == fs::canonicalize(&main).ok() })
        );
        assert!(
            !opened
                .iter()
                .any(|path| { fs::canonicalize(path).ok() == fs::canonicalize(&dependency).ok() })
        );
    }

    #[test]
    fn multiple_definitions_require_explicit_choice_and_apply_the_selected_one() {
        let directory = TempDir::new("definition-chooser");
        let path = directory.join("main.rs");
        let text = "fn main() {\n}\n\nfn one() {}\nfn two() {}\n";
        fs::write(&path, text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_cursor = application.view.cursor();

        let origin = request_definition(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let first = destination(&uri, 3, 3);
        let second = destination(&uri, 4, 3);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                first,
                second.clone(),
            ])),
        );

        assert_eq!(application.mode(), Mode::NavigationChooser);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
        let chooser = application.navigation_chooser().expect("seletor aberto");
        assert_eq!(chooser.choices().len(), 2);
        assert_eq!(chooser.selected(), 0);
        assert_eq!(chooser.choices()[0].file_name, "main.rs");
        assert!(chooser.choices()[0].relative_path.ends_with("main.rs"));
        assert_eq!(chooser.choices()[0].line, 3);
        assert!(chooser.choices()[0].context.is_some());

        press(&mut application, KeyCode::Down);
        assert_eq!(
            application
                .navigation_chooser()
                .expect("seletor aberto")
                .selected(),
            1
        );
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(
            application
                .view
                .cursor_line(&application.buffer)
                .expect("linha do cursor"),
            4
        );
        assert_eq!(application.navigation_history().len(), 1);
        let history = &application.navigation_history()[0];
        assert_eq!(history.target.uri, second.uri);
        assert_eq!(history.target.cursor, application.view.cursor());
    }

    #[test]
    fn cancelling_the_definition_chooser_keeps_the_original_document() {
        let directory = TempDir::new("definition-chooser-cancel");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\n\nfn one() {}\nfn two() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);
        let original_cursor = application.view.cursor();

        let origin = request_definition(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 3, 3),
                destination(&uri, 4, 3),
            ])),
        );
        assert_eq!(application.mode(), Mode::NavigationChooser);

        press(&mut application, KeyCode::Esc);

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Info)
        );
    }

    #[test]
    fn navigation_chooser_keeps_the_selection_visible_while_scrolling() {
        let directory = TempDir::new("definition-chooser-scroll");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let mut application = app("");
        application.open_document_in_new_tab(&path).unwrap();
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let targets = (0..20)
            .map(|line| destination(&uri, line, 0))
            .collect::<Vec<_>>();
        application.open_navigation_chooser_for_tests(targets);

        for _ in 0..15 {
            press(&mut application, KeyCode::Down);
        }

        let chooser = application.navigation_chooser().expect("seletor aberto");
        assert_eq!(chooser.selected(), 15);
        assert!(chooser.offset(5) > 0);
        assert!(chooser.selected() < chooser.offset(5) + 5);
    }

    #[test]
    fn exact_duplicate_destinations_are_normalized_but_distinct_ranges_are_not() {
        let first = destination("file:///workspace/main.rs", 3, 3);
        let second = destination("file:///workspace/main.rs", 4, 3);

        assert_eq!(
            normalize_navigation_targets(vec![
                first.clone(),
                first.clone(),
                second.clone(),
                first.clone()
            ]),
            vec![first, second]
        );
    }

    #[test]
    fn definition_target_prefers_the_selection_range() {
        let directory = TempDir::new("definition-selection-range");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n\n    target();\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let origin = request_definition(&mut application);
        let target = NavigationTarget {
            uri: nexus_lsp::uri::path_to_uri(&path),
            target_range: nexus_lsp::protocol::Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 2,
                    character: 12,
                },
            },
            selection_range: nexus_lsp::protocol::Range {
                start: Position {
                    line: 2,
                    character: 4,
                },
                end: Position {
                    line: 2,
                    character: 10,
                },
            },
            origin_selection_range: None,
        };
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                target,
            ])),
        );

        assert_eq!(
            application
                .view
                .cursor_line(&application.buffer)
                .expect("linha do cursor"),
            2
        );
        assert_eq!(
            application
                .view
                .cursor_visual_column(&application.buffer)
                .expect("coluna do cursor"),
            4
        );
    }

    #[test]
    fn stale_definition_result_does_not_move_the_cursor_or_record_history() {
        let directory = TempDir::new("definition-stale");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\n\nfn target() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        let origin = request_definition(&mut application);
        application
            .view
            .set_cursor(&application.buffer, CharOffset(4))
            .unwrap();
        let target = destination(&nexus_lsp::uri::path_to_uri(&path), 3, 3);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                target,
            ])),
        );

        assert_eq!(application.view.cursor(), CharOffset(4));
        assert!(application.navigation_history().is_empty());
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Info)
        );
    }

    #[test]
    fn a_new_f12_request_supersedes_the_previous_one() {
        let directory = TempDir::new("definition-superseded");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\n\nfn one() {}\nfn two() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        let first = request_definition(&mut application);
        let second = request_definition(&mut application);
        assert_ne!(first.operation_id, second.operation_id);
        assert_eq!(state.lock().unwrap().cancelled, vec![first.operation_id]);

        let uri = nexus_lsp::uri::path_to_uri(&path);
        complete_navigation(
            &state,
            &first,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 3, 3),
            ])),
        );
        complete_navigation(
            &state,
            &second,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 4, 3),
            ])),
        );
        application.on_idle();

        assert_eq!(application.navigation_history().len(), 1);
        assert_eq!(
            application
                .view
                .cursor_line(&application.buffer)
                .expect("linha do cursor"),
            4
        );
    }

    #[test]
    fn empty_and_unsupported_definition_results_are_reported_without_navigation() {
        let directory = TempDir::new("definition-empty-unsupported");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);

        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("nenhuma definição"))
        );

        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Unsupported,
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("não suporta"))
        );
    }

    #[test]
    fn definition_failures_are_reported_without_raw_protocol_details() {
        let directory = TempDir::new("definition-failures");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);

        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::TimedOut {
                duration: Duration::from_secs(2),
            },
        );
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("tempo esgotado"))
        );

        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Failed {
                failure: nexus_lsp::service::NavigationFailure::Protocol(
                    "{\"jsonrpc\":\"2.0\",\"error\":\"detalhe interno\"}".to_owned(),
                ),
            },
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("resposta inválida")
                    && !message.text.contains("jsonrpc"))
        );
    }

    #[test]
    fn invalid_or_missing_definition_destinations_are_reported_gracefully() {
        let directory = TempDir::new("definition-invalid-target");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);

        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination("untitled:definicao", 0, 0),
            ])),
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("URI"))
        );

        let missing = directory.join("missing.rs");
        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&missing), 0, 0),
            ])),
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("abrir a definição"))
        );
    }

    #[test]
    fn out_of_range_definition_position_is_reported_without_moving_the_cursor() {
        let directory = TempDir::new("definition-out-of-range");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_cursor = application.view.cursor();

        let origin = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&path), 999, 0),
            ])),
        );

        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("fora do documento"))
        );
    }

    #[test]
    fn shift_f12_requests_references_with_include_declaration_and_utf16_position() {
        let directory = TempDir::new("references-request");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() { 😀 alvo(); }\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        // O emoji ocupa duas unidades UTF-16 embora seja um único caractere.
        application
            .view
            .set_cursor(&application.buffer, CharOffset(14))
            .unwrap();

        let origin = request_references(&mut application);
        let requested = state.lock().unwrap().requested.clone();

        assert_eq!(requested.len(), 1);
        assert_eq!(
            requested[0].2,
            NavigationMethod::References {
                include_declaration: true,
            }
        );
        assert_eq!(requested[0].3, nexus_lsp::uri::path_to_uri(&path));
        assert_eq!(
            requested[0].4,
            Position {
                line: 0,
                character: 15,
            }
        );
        assert_eq!(origin.position, requested[0].4);
        assert_eq!(
            application
                .status_message()
                .map(|message| message.text.as_str()),
            Some("procurando referências...")
        );
    }

    #[test]
    fn a_single_reference_still_requires_an_explicit_choice_before_navigating() {
        let directory = TempDir::new("references-single");
        let path = directory.join("main.rs");
        let text = "fn main() {\n    alvo();\n}\nfn alvo() {}\n";
        fs::write(&path, text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(3))
            .unwrap();
        let original_cursor = application.view.cursor();

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&path), 1, 4),
            ])),
        );

        assert_eq!(application.mode(), Mode::NavigationChooser);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
        let chooser = application.navigation_chooser().expect("seletor aberto");
        assert!(chooser.is_references());
        assert_eq!(chooser.title(), "Referências");
        assert_eq!(chooser.choices().len(), 1);

        press(&mut application, KeyCode::Enter);

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(cursor_line_column(&application), (1, 4));
        assert_eq!(application.navigation_history().len(), 1);
        assert_eq!(
            application.navigation_history()[0].method,
            NavigationMethod::References {
                include_declaration: true,
            }
        );
        assert_eq!(
            application.navigation_history()[0].origin.uri,
            nexus_lsp::uri::path_to_uri(&path)
        );

        press_alt(&mut application, KeyCode::Left);

        assert_eq!(cursor_line_column(&application), (0, 3));
        assert_eq!(application.navigation_history().len(), 1);
    }

    #[test]
    fn references_are_sorted_deduplicated_and_filtered_before_presentation() {
        let directory = TempDir::new("references-normalize");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n    uso(); uso();\n    uso();\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let first = destination(&uri, 1, 4);
        let second_same_line = destination(&uri, 1, 11);
        let third = destination(&uri, 2, 4);
        let inverted = NavigationTarget {
            uri: uri.clone(),
            target_range: nexus_lsp::protocol::Range {
                start: Position {
                    line: 9,
                    character: 0,
                },
                end: Position {
                    line: 8,
                    character: 0,
                },
            },
            selection_range: nexus_lsp::protocol::Range {
                start: Position {
                    line: 9,
                    character: 0,
                },
                end: Position {
                    line: 8,
                    character: 0,
                },
            },
            origin_selection_range: None,
        };

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                third.clone(),
                inverted.clone(),
                first.clone(),
                second_same_line.clone(),
                destination("", 0, 0),
                first.clone(),
                second_same_line.clone(),
            ])),
        );

        let chooser = application.navigation_chooser().expect("seletor aberto");
        let positions: Vec<_> = chooser
            .choices()
            .iter()
            .map(|choice| (choice.line, choice.column))
            .collect();
        assert_eq!(positions, vec![(1, 4), (1, 11), (2, 4)]);
        assert_eq!(chooser.choices()[0].target, first);
        assert_eq!(chooser.choices()[2].target, third);

        press(&mut application, KeyCode::Esc);

        // Somente alvos inválidos não abrem uma lista vazia.
        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination("", 0, 0),
                inverted,
            ])),
        );

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("nenhuma referência"))
        );
    }

    #[test]
    fn choosing_a_reference_in_another_file_reuses_tabs_and_preserves_edits() {
        let directory = TempDir::new("references-other-file");
        let main = directory.join("main.rs");
        let uses = directory.join("uses.rs");
        fs::write(&main, "fn main() { alvo(); }\n").unwrap();
        let uses_text = "fn alvo() {}\nfn uso() { alvo(); }\n";
        fs::write(&uses, uses_text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&main).unwrap();
        application.open_document_in_new_tab(&uses).unwrap();
        application.insert_text("// local\n", EditKind::Insert);
        application.commit_session();
        let local_text = application.buffer().text().unwrap();
        assert!(application.buffer().is_modified());
        let main_index = tab_index_for_path(&application, &main);
        assert!(application.switch_tab(main_index));

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&uses), 2, 13),
            ])),
        );
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.tab_count(), 2);
        assert_active_path(&application, &uses);
        assert_eq!(application.buffer().text().unwrap(), local_text);
        assert!(application.buffer().is_modified());
        assert_eq!(cursor_line_column(&application), (2, 13));
        assert_eq!(application.navigation_history().len(), 1);
    }

    #[test]
    fn reference_outside_the_workspace_opens_without_new_lsp_routes() {
        let directory = TempDir::new("references-external-workspace");
        let workspace = directory.join("projeto");
        let external = directory.join("fora");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&external).unwrap();
        let main = workspace.join("main.rs");
        let dependency = external.join("lib.rs");
        fs::write(&main, "fn main() { alvo(); }\n").unwrap();
        fs::write(&dependency, "pub fn alvo() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_workspace(&workspace).unwrap();
        application.open_document_in_new_tab(&main).unwrap();
        let root = application.workspace().unwrap().root().to_path_buf();

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&dependency), 0, 7),
            ])),
        );
        press(&mut application, KeyCode::Enter);

        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(root.as_path())
        );
        assert_active_path(&application, &dependency);
        assert_eq!(application.tab_count(), 2);
        let opened = state.lock().unwrap().opened.clone();
        assert!(
            !opened
                .iter()
                .any(|path| { fs::canonicalize(path).ok() == fs::canonicalize(&dependency).ok() })
        );
    }

    #[test]
    fn cancelling_the_references_chooser_keeps_document_and_history_untouched() {
        let directory = TempDir::new("references-cancel");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n    alvo(); alvo();\n}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);
        let original_cursor = application.view.cursor();

        let origin = request_references(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 1, 4),
                destination(&uri, 1, 11),
            ])),
        );
        assert_eq!(application.mode(), Mode::NavigationChooser);

        press(&mut application, KeyCode::Esc);

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("cancelada"))
        );
    }

    #[test]
    fn empty_and_unsupported_reference_results_are_reported_without_navigation() {
        let directory = TempDir::new("references-empty-unsupported");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(application.navigation_chooser().is_none());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("nenhuma referência"))
        );

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Unsupported,
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(application.navigation_chooser().is_none());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("não suporta localizar referências"))
        );
    }

    #[test]
    fn reference_failures_use_friendly_method_aware_messages() {
        use nexus_lsp::service::NavigationFailure;

        let directory = TempDir::new("references-failures");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::TimedOut {
                duration: Duration::from_secs(2),
            },
        );
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
        assert!(application.status_message().is_some_and(
            |message| message.text == "tempo esgotado ao localizar referências (2000 ms)"
        ));

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Cancelled,
        );
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Info)
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text == "busca por referências cancelada")
        );

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::SessionEnded,
        );
        assert!(application.status_message().is_some_and(
            |message| message.text == "servidor LSP indisponível para localizar referências"
        ));

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Failed {
                failure: NavigationFailure::Transport("detalhe interno".to_owned()),
            },
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text
                    == "falha de comunicação com o servidor LSP ao localizar referências")
        );

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Failed {
                failure: NavigationFailure::Protocol("{\"jsonrpc\":\"2.0\"}".to_owned()),
            },
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text
                    == "resposta inválida do servidor LSP ao localizar referências")
        );

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Failed {
                failure: NavigationFailure::Server("falha".to_owned()),
            },
        );
        assert!(application.status_message().is_some_and(
            |message| message.text == "o servidor LSP não conseguiu localizar as referências"
        ));
        assert!(application.navigation_history().is_empty());
    }

    #[test]
    fn invalid_or_missing_reference_destinations_are_reported_gracefully() {
        let directory = TempDir::new("references-invalid-target");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);

        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination("untitled:referencia", 0, 0),
            ])),
        );
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(application.status_message().is_some_and(|message| {
            message.text.contains("referência") && message.text.contains("URI")
        }));

        let missing = directory.join("missing.rs");
        let origin = request_references(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&missing), 0, 0),
            ])),
        );
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("abrir a referência"))
        );
    }

    #[test]
    fn stale_reference_contexts_do_not_open_the_chooser_or_move_the_cursor() {
        let directory = TempDir::new("references-stale");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\n\nfn alvo() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let uri = nexus_lsp::uri::path_to_uri(&path);

        let origin = request_references(&mut application);
        application
            .view
            .set_cursor(&application.buffer, CharOffset(4))
            .unwrap();
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 3, 3),
            ])),
        );

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(application.view.cursor(), CharOffset(4));
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("referência descartada"))
        );

        let origin = request_references(&mut application);
        state.lock().unwrap().session_id = 1;
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 3, 3),
            ])),
        );

        assert!(application.navigation_chooser().is_none());
        assert!(application.navigation_history().is_empty());
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("contexto do servidor LSP"))
        );
    }

    #[test]
    fn reference_results_are_discarded_when_edit_tab_or_workspace_changes() {
        let directory = TempDir::new("references-context-change");
        let first_workspace = directory.join("first");
        let second_workspace = directory.join("second");
        fs::create_dir_all(&first_workspace).unwrap();
        fs::create_dir_all(&second_workspace).unwrap();
        let first = first_workspace.join("main.rs");
        let second = first_workspace.join("other.rs");
        fs::write(&first, "fn main() {}\n").unwrap();
        fs::write(&second, "fn other() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_workspace(&first_workspace).unwrap();
        application.open_document_in_new_tab(&first).unwrap();
        let uri = nexus_lsp::uri::path_to_uri(&first);

        // Documento alterado durante a requisição.
        let origin = request_references(&mut application);
        application.insert_text("// edit\n", EditKind::Insert);
        application.commit_session();
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 0, 3),
            ])),
        );
        assert!(application.navigation_chooser().is_none());
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("referência descartada"))
        );

        // Troca de aba durante a requisição.
        let origin = request_references(&mut application);
        application.open_document_in_new_tab(&second).unwrap();
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 0, 3),
            ])),
        );
        assert!(application.navigation_chooser().is_none());
        assert!(application.navigation_history().is_empty());
        assert_active_path(&application, &second);

        // Troca de workspace durante a requisição.
        let origin = request_references(&mut application);
        application.open_workspace(&second_workspace).unwrap();
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 0, 3),
            ])),
        );
        assert!(application.navigation_chooser().is_none());
        assert!(application.navigation_history().is_empty());
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(fs::canonicalize(&second_workspace).unwrap().as_path())
        );
    }

    #[test]
    fn a_new_reference_request_supersedes_the_previous_one() {
        let directory = TempDir::new("references-superseded");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\n\nfn one() {}\nfn two() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        let first = request_references(&mut application);
        let second = request_references(&mut application);
        assert_ne!(first.operation_id, second.operation_id);
        assert_eq!(state.lock().unwrap().cancelled, vec![first.operation_id]);

        let uri = nexus_lsp::uri::path_to_uri(&path);
        complete_navigation(
            &state,
            &first,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 3, 3),
            ])),
        );
        complete_navigation(
            &state,
            &second,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 4, 3),
            ])),
        );
        application.on_idle();

        let chooser = application.navigation_chooser().expect("seletor aberto");
        assert_eq!(chooser.choices().len(), 1);
        assert_eq!(chooser.choices()[0].line, 4);
        assert!(application.navigation_history().is_empty());
    }

    #[test]
    fn references_chooser_supports_j_k_pagination_and_scrolling() {
        let directory = TempDir::new("references-chooser-keys");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let mut application = app("");
        application.open_document_in_new_tab(&path).unwrap();
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let targets = (0..20)
            .map(|line| destination(&uri, line, 0))
            .collect::<Vec<_>>();
        application.open_references_chooser_for_tests(targets);
        assert_eq!(
            application.navigation_chooser().expect("seletor").title(),
            "Referências"
        );

        press(&mut application, KeyCode::Char('j'));
        assert_eq!(
            application
                .navigation_chooser()
                .expect("seletor")
                .selected(),
            1
        );
        press(&mut application, KeyCode::Char('k'));
        assert_eq!(
            application
                .navigation_chooser()
                .expect("seletor")
                .selected(),
            0
        );
        press(&mut application, KeyCode::PageDown);
        assert_eq!(
            application
                .navigation_chooser()
                .expect("seletor")
                .selected(),
            5
        );
        press(&mut application, KeyCode::PageDown);
        for _ in 0..10 {
            press(&mut application, KeyCode::Down);
        }
        let chooser = application.navigation_chooser().expect("seletor");
        assert_eq!(chooser.selected(), 19);
        assert!(chooser.offset(5) > 0);
        assert!(chooser.selected() < chooser.offset(5) + 5);
        press(&mut application, KeyCode::PageUp);
        assert_eq!(
            application
                .navigation_chooser()
                .expect("seletor")
                .selected(),
            14
        );
        press(&mut application, KeyCode::Esc);
        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
    }

    #[test]
    fn shift_f12_without_a_document_reports_indisponibility_without_requesting() {
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));

        press_shift(&mut application, KeyCode::F(12));

        assert!(state.lock().unwrap().requested.is_empty());
        assert!(application.active_navigation.is_none());
        assert!(application.status_message().is_some_and(
            |message| message.text == "nenhum documento aberto para localizar referências"
        ));
    }

    #[test]
    fn ctrl_f12_requests_declaration_and_applies_a_single_target() {
        let directory = TempDir::new("declaration-single");
        let path = directory.join("main.rs");
        let text = "fn main() {\n    alvo();\n}\n\nfn alvo() {}\n";
        fs::write(&path, text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(3))
            .unwrap();

        let origin = request_declaration(&mut application);
        let requested = state.lock().unwrap().requested.clone();

        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].2, NavigationMethod::Declaration);
        assert_eq!(requested[0].3, nexus_lsp::uri::path_to_uri(&path));
        assert_eq!(requested[0].4, origin.position);
        assert_eq!(
            application
                .status_message()
                .map(|message| message.text.as_str()),
            Some("procurando declaração...")
        );

        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&path), 4, 3),
            ])),
        );

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(cursor_line_column(&application), (4, 3));
        assert_eq!(application.navigation_history().len(), 1);
        assert_eq!(
            application.navigation_history()[0].method,
            NavigationMethod::Declaration
        );

        press_alt(&mut application, KeyCode::Left);

        assert_eq!(cursor_line_column(&application), (0, 3));
        assert_eq!(application.navigation_history().len(), 1);
    }

    #[test]
    fn multiple_declarations_open_the_chooser_and_apply_the_selected_target() {
        let directory = TempDir::new("declaration-chooser");
        let path = directory.join("main.rs");
        let text = "fn main() {\n}\n\nfn one() {}\nfn two() {}\n";
        fs::write(&path, text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_cursor = application.view.cursor();

        let origin = request_declaration(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let second = destination(&uri, 4, 3);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 3, 3),
                second.clone(),
            ])),
        );

        assert_eq!(application.mode(), Mode::NavigationChooser);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
        let chooser = application.navigation_chooser().expect("seletor aberto");
        assert_eq!(chooser.title(), "Declarações");
        assert_eq!(chooser.choices().len(), 2);

        press(&mut application, KeyCode::Down);
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(cursor_line_column(&application), (4, 3));
        assert_eq!(application.navigation_history().len(), 1);
        assert_eq!(application.navigation_history()[0].target.uri, second.uri);
        assert_eq!(
            application.navigation_history()[0].method,
            NavigationMethod::Declaration
        );
    }

    #[test]
    fn cancelling_the_declaration_chooser_has_no_side_effects() {
        let directory = TempDir::new("declaration-cancel");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\n\nfn one() {}\nfn two() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);
        let original_cursor = application.view.cursor();

        let origin = request_declaration(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 3, 3),
                destination(&uri, 4, 3),
            ])),
        );
        assert_eq!(application.mode(), Mode::NavigationChooser);

        press(&mut application, KeyCode::Esc);

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
    }

    #[test]
    fn declaration_empty_unsupported_and_failures_use_declaration_messages() {
        use nexus_lsp::service::NavigationFailure;

        let directory = TempDir::new("declaration-failures");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);

        let origin = request_declaration(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text == "nenhuma declaração encontrada")
        );

        let origin = request_declaration(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Unsupported,
        );
        assert!(
            application.status_message().is_some_and(
                |message| message.text == "o servidor LSP não suporta ir para declaração"
            )
        );

        let origin = request_declaration(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::TimedOut {
                duration: Duration::from_secs(2),
            },
        );
        assert!(
            application.status_message().is_some_and(
                |message| message.text == "tempo esgotado ao buscar declaração (2000 ms)"
            )
        );

        let origin = request_declaration(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Cancelled,
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text == "busca por declaração cancelada")
        );

        let origin = request_declaration(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Failed {
                failure: NavigationFailure::Protocol("{\"jsonrpc\":\"2.0\"}".to_owned()),
            },
        );
        assert!(application.status_message().is_some_and(
            |message| message.text == "resposta inválida do servidor LSP ao buscar declaração"
        ));
        assert!(application.navigation_history().is_empty());
    }

    #[test]
    fn stale_declaration_result_is_discarded_without_history() {
        let directory = TempDir::new("declaration-stale");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\n\nfn alvo() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        let origin = request_declaration(&mut application);
        application
            .view
            .set_cursor(&application.buffer, CharOffset(4))
            .unwrap();
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&path), 3, 3),
            ])),
        );

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(application.view.cursor(), CharOffset(4));
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("declaração descartada"))
        );
    }

    #[test]
    fn declaration_navigation_reuses_tabs_and_preserves_modified_buffers() {
        let directory = TempDir::new("declaration-other-file");
        let main = directory.join("main.rs");
        let declarations = directory.join("declarations.rs");
        fs::write(&main, "fn main() { alvo(); }\n").unwrap();
        fs::write(&declarations, "fn alvo() {}\nfn declaracao() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&main).unwrap();
        application.open_document_in_new_tab(&declarations).unwrap();
        application.insert_text("// local\n", EditKind::Insert);
        application.commit_session();
        let local_text = application.buffer().text().unwrap();
        assert!(application.buffer().is_modified());
        let main_index = tab_index_for_path(&application, &main);
        assert!(application.switch_tab(main_index));

        let origin = request_declaration(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&declarations), 2, 3),
            ])),
        );

        assert_eq!(application.tab_count(), 2);
        assert_active_path(&application, &declarations);
        assert_eq!(application.buffer().text().unwrap(), local_text);
        assert!(application.buffer().is_modified());
        assert_eq!(cursor_line_column(&application), (2, 3));
        assert_eq!(application.navigation_history().len(), 1);
    }

    #[test]
    fn ctrl_f12_without_a_document_reports_indisponibility_without_requesting() {
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));

        press_ctrl(&mut application, KeyCode::F(12));

        assert!(state.lock().unwrap().requested.is_empty());
        assert!(application.active_navigation.is_none());
        assert!(
            application.status_message().is_some_and(
                |message| message.text == "nenhum documento aberto para ir à declaração"
            )
        );
    }

    #[test]
    fn ctrl_t_requests_type_definition_and_applies_a_single_target() {
        let directory = TempDir::new("type-definition-single");
        let path = directory.join("main.py");
        let text = "class Usuario:\n    pass\n\nusuario = Usuario()\n";
        fs::write(&path, text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        let origin = request_type_definition(&mut application);
        let requested = state.lock().unwrap().requested.clone();

        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].2, NavigationMethod::TypeDefinition);
        assert_eq!(requested[0].3, nexus_lsp::uri::path_to_uri(&path));
        assert_eq!(requested[0].4, origin.position);
        assert_eq!(
            application
                .status_message()
                .map(|message| message.text.as_str()),
            Some("procurando definição de tipo...")
        );

        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&path), 0, 6),
            ])),
        );

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(cursor_line_column(&application), (0, 6));
        assert_eq!(application.navigation_history().len(), 1);
        assert_eq!(
            application.navigation_history()[0].method,
            NavigationMethod::TypeDefinition
        );

        press_alt(&mut application, KeyCode::Left);

        assert_eq!(application.view.cursor(), CharOffset(0));
        assert_eq!(application.navigation_history().len(), 1);
    }

    #[test]
    fn multiple_type_definitions_open_the_chooser_and_apply_the_selected_target() {
        let directory = TempDir::new("type-definition-chooser");
        let path = directory.join("main.py");
        fs::write(&path, "class A:\n    pass\n\nclass B:\n    pass\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_cursor = application.view.cursor();

        let origin = request_type_definition(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        let second = destination(&uri, 3, 6);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 0, 6),
                second.clone(),
            ])),
        );

        assert_eq!(application.mode(), Mode::NavigationChooser);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
        let chooser = application.navigation_chooser().expect("seletor aberto");
        assert_eq!(chooser.title(), "Definições de Tipo");
        assert_eq!(chooser.choices().len(), 2);

        press(&mut application, KeyCode::Down);
        press(&mut application, KeyCode::Enter);

        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(cursor_line_column(&application), (3, 6));
        assert_eq!(application.navigation_history().len(), 1);
        assert_eq!(application.navigation_history()[0].target.uri, second.uri);
        assert_eq!(
            application.navigation_history()[0].method,
            NavigationMethod::TypeDefinition
        );
    }

    #[test]
    fn cancelling_the_type_definition_chooser_has_no_side_effects() {
        let directory = TempDir::new("type-definition-cancel");
        let path = directory.join("main.py");
        fs::write(&path, "class A:\n    pass\n\nclass B:\n    pass\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);
        let original_cursor = application.view.cursor();

        let origin = request_type_definition(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&path);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 0, 6),
                destination(&uri, 3, 6),
            ])),
        );
        assert_eq!(application.mode(), Mode::NavigationChooser);

        press(&mut application, KeyCode::Esc);

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert_eq!(application.view.cursor(), original_cursor);
        assert!(application.navigation_history().is_empty());
    }

    #[test]
    fn type_definition_empty_unsupported_and_failures_use_specific_messages() {
        use nexus_lsp::service::NavigationFailure;

        let directory = TempDir::new("type-definition-failures");
        let path = directory.join("main.py");
        fs::write(&path, "x = 1\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();
        let original_path = application.path().map(Path::to_path_buf);

        let origin = request_type_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Empty),
        );
        assert_eq!(application.path().map(Path::to_path_buf), original_path);
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text == "nenhuma definição de tipo encontrada")
        );

        let origin = request_type_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Unsupported,
        );
        assert!(application.status_message().is_some_and(
            |message| message.text == "o servidor LSP não suporta ir à definição de tipo"
        ));

        let origin = request_type_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::TimedOut {
                duration: Duration::from_secs(2),
            },
        );
        assert!(application.status_message().is_some_and(
            |message| message.text == "tempo esgotado ao buscar definição de tipo (2000 ms)"
        ));

        let origin = request_type_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Cancelled,
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text == "busca por definição de tipo cancelada")
        );

        let origin = request_type_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Failed {
                failure: NavigationFailure::Protocol("{\"jsonrpc\":\"2.0\"}".to_owned()),
            },
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text
                    == "resposta inválida do servidor LSP ao buscar definição de tipo")
        );
        assert!(application.navigation_history().is_empty());
    }

    #[test]
    fn stale_type_definition_result_is_discarded_without_history() {
        let directory = TempDir::new("type-definition-stale");
        let path = directory.join("main.py");
        fs::write(&path, "class A:\n    pass\n\ny = A()\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        let origin = request_type_definition(&mut application);
        application
            .view
            .set_cursor(&application.buffer, CharOffset(4))
            .unwrap();
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&path), 0, 6),
            ])),
        );

        assert_eq!(application.mode(), Mode::Editor);
        assert!(application.navigation_chooser().is_none());
        assert_eq!(application.view.cursor(), CharOffset(4));
        assert!(application.navigation_history().is_empty());
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("definição de tipo descartada"))
        );
    }

    #[test]
    fn type_definition_navigation_reuses_tabs_and_preserves_modified_buffers() {
        let directory = TempDir::new("type-definition-other-file");
        let main = directory.join("main.py");
        let types = directory.join("types.py");
        fs::write(&main, "from types import Usuario\n").unwrap();
        fs::write(&types, "class Usuario:\n    pass\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&main).unwrap();
        application.open_document_in_new_tab(&types).unwrap();
        application.insert_text("# local\n", EditKind::Insert);
        application.commit_session();
        let local_text = application.buffer().text().unwrap();
        assert!(application.buffer().is_modified());
        let main_index = tab_index_for_path(&application, &main);
        assert!(application.switch_tab(main_index));

        let origin = request_type_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&types), 1, 6),
            ])),
        );

        assert_eq!(application.tab_count(), 2);
        assert_active_path(&application, &types);
        assert_eq!(application.buffer().text().unwrap(), local_text);
        assert!(application.buffer().is_modified());
        assert_eq!(cursor_line_column(&application), (1, 6));
        assert_eq!(application.navigation_history().len(), 1);
        assert_eq!(
            application.navigation_history()[0].method,
            NavigationMethod::TypeDefinition
        );
    }

    #[test]
    fn ctrl_t_without_a_document_reports_indisponibility_without_requesting() {
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));

        press_ctrl(&mut application, KeyCode::Char('t'));

        assert!(state.lock().unwrap().requested.is_empty());
        assert!(application.active_navigation.is_none());
        assert!(application.status_message().is_some_and(
            |message| message.text == "nenhum documento aberto para ir à definição de tipo"
        ));
    }

    fn assert_active_path(application: &App, path: &Path) {
        assert_eq!(
            application
                .path()
                .map(fs::canonicalize)
                .transpose()
                .unwrap(),
            Some(fs::canonicalize(path).unwrap())
        );
    }

    fn cursor_line_column(application: &App) -> (usize, usize) {
        (
            application
                .view
                .cursor_line(&application.buffer)
                .expect("linha do cursor"),
            application
                .view
                .cursor_visual_column(&application.buffer)
                .expect("coluna do cursor"),
        )
    }

    fn tab_index_for_path(application: &App, path: &Path) -> usize {
        let expected = fs::canonicalize(path).unwrap();
        application
            .tabs()
            .into_iter()
            .find(|tab| {
                tab.path.as_deref().is_some_and(|candidate| {
                    fs::canonicalize(candidate).ok() == Some(expected.clone())
                })
            })
            .map(|tab| tab.index)
            .expect("aba do caminho")
    }

    fn navigate_to(
        application: &mut App,
        state: &Arc<Mutex<NavigationSinkState>>,
        path: &Path,
        line: u32,
        character: u32,
    ) {
        let origin = request_definition(application);
        let target = destination(&nexus_lsp::uri::path_to_uri(path), line, character);
        finish_navigation(
            application,
            state,
            &origin,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                target,
            ])),
        );
    }

    #[test]
    fn navigation_history_traverses_a_b_c_and_back_forward_do_not_add_entries() {
        let directory = TempDir::new("history-abc");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        let third = directory.join("c.rs");
        fs::write(&first, "fn a() {\n    call_b();\n}\n").unwrap();
        fs::write(&second, "fn b() {\n    call_c();\n}\n").unwrap();
        fs::write(&third, "fn c() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();
        application
            .view
            .set_cursor(&application.buffer, CharOffset(11))
            .unwrap();
        let origin_cursor = application.view.cursor();

        navigate_to(&mut application, &state, &second, 1, 4);
        assert_active_path(&application, &second);
        assert_eq!(cursor_line_column(&application), (1, 4));
        navigate_to(&mut application, &state, &third, 0, 3);
        assert_active_path(&application, &third);
        assert_eq!(cursor_line_column(&application), (0, 3));

        assert_eq!(application.navigation_history().len(), 2);
        assert_eq!(application.navigation_back_steps(), 2);
        assert_eq!(application.navigation_forward_steps(), 0);
        assert!(application.can_navigate_back());

        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &second);
        assert_eq!(cursor_line_column(&application), (1, 4));
        assert_eq!(application.navigation_history().len(), 2);

        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &first);
        assert_eq!(application.view.cursor(), origin_cursor);
        assert_eq!(application.navigation_history().len(), 2);
        assert!(!application.can_navigate_back());
        assert!(application.can_navigate_forward());

        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &first);
        assert_eq!(application.navigation_history().len(), 2);

        press_alt(&mut application, KeyCode::Right);
        assert_active_path(&application, &second);
        assert_eq!(cursor_line_column(&application), (1, 4));
        press_alt(&mut application, KeyCode::Right);
        assert_active_path(&application, &third);
        assert_eq!(cursor_line_column(&application), (0, 3));
        press_alt(&mut application, KeyCode::Right);
        assert_active_path(&application, &third);
        assert_eq!(application.navigation_history().len(), 2);
        assert_eq!(application.tab_count(), 3);
    }

    #[test]
    fn navigation_history_restores_cursor_selection_and_viewport() {
        let directory = TempDir::new("history-viewport");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        let long_line = format!("{:03} {}", 60, "x".repeat(180));
        let mut lines: Vec<String> = (0..80).map(|line| format!("linha {line}")).collect();
        lines[60] = long_line;
        let first_text = lines.join("\n");
        fs::write(&first, &first_text).unwrap();
        fs::write(&second, "target();\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();

        let line_start = first_text
            .lines()
            .take(60)
            .map(|line| line.chars().count() + 1)
            .sum::<usize>();
        let cursor = CharOffset(line_start + 120);
        application
            .view
            .set_cursor(&application.buffer, cursor)
            .unwrap();
        application
            .view
            .set_selection(&application.buffer, CharOffset(line_start + 100), cursor)
            .unwrap();
        let expected_selection = application.view.selection_range();
        assert!(application.view.top_line() > 0);
        assert!(application.view.left_col() > 0);
        let expected_top = application.view.top_line();
        let expected_left = application.view.left_col();

        navigate_to(&mut application, &state, &second, 0, 0);
        press_alt(&mut application, KeyCode::Left);

        assert_active_path(&application, &first);
        assert_eq!(application.view.cursor(), cursor);
        assert_eq!(application.view.top_line(), expected_top);
        assert_eq!(application.view.left_col(), expected_left);
        assert_eq!(application.view.selection_range(), expected_selection);
        assert_eq!(application.navigation_history().len(), 1);
    }

    #[test]
    fn navigation_history_reuses_tabs_and_preserves_modified_buffers() {
        let directory = TempDir::new("history-modified");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {\n    target();\n}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();

        navigate_to(&mut application, &state, &second, 1, 4);
        application.insert_text("// local\n", EditKind::Insert);
        application.commit_session();
        let local = text(&application);
        assert!(application.buffer().is_modified());
        assert_eq!(application.tab_count(), 2);

        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &first);
        press_alt(&mut application, KeyCode::Right);

        assert_active_path(&application, &second);
        assert_eq!(text(&application), local);
        assert!(application.buffer().is_modified());
        assert_eq!(application.tab_count(), 2);
        assert_eq!(application.navigation_history().len(), 1);
    }

    #[test]
    fn navigation_history_reopens_a_closed_document_when_the_file_still_exists() {
        let directory = TempDir::new("history-reopen");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {\n    target();\n}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();

        navigate_to(&mut application, &state, &second, 1, 4);
        press_alt(&mut application, KeyCode::Left);
        let second_index = tab_index_for_path(&application, &second);
        application.close_tab(second_index);
        assert_eq!(application.tab_count(), 1);

        press_alt(&mut application, KeyCode::Right);

        assert_active_path(&application, &second);
        assert_eq!(cursor_line_column(&application), (1, 4));
        assert_eq!(application.tab_count(), 2);
        assert_eq!(application.navigation_history().len(), 1);
        assert!(!application.can_navigate_forward());
    }

    #[test]
    fn navigation_history_reports_removed_files_without_corrupting_state() {
        let directory = TempDir::new("history-removed");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        let second_text = "fn b() {\n    target();\n}\n";
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, second_text).unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();

        navigate_to(&mut application, &state, &second, 1, 4);
        press_alt(&mut application, KeyCode::Left);
        let second_index = tab_index_for_path(&application, &second);
        application.close_tab(second_index);
        fs::remove_file(&second).unwrap();

        press_alt(&mut application, KeyCode::Right);

        assert_active_path(&application, &first);
        assert_eq!(application.tab_count(), 1);
        assert_eq!(application.navigation_history().len(), 1);
        assert!(application.can_navigate_forward());
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Error)
        );
        assert!(
            application
                .status_message()
                .is_some_and(|message| message.text.contains("restaurar a localização"))
        );

        fs::write(&second, second_text).unwrap();
        press_alt(&mut application, KeyCode::Right);
        assert_active_path(&application, &second);
        assert_eq!(cursor_line_column(&application), (1, 4));
        assert!(!application.can_navigate_forward());
    }

    #[test]
    fn navigation_history_survives_document_renames() {
        let directory = TempDir::new("history-rename");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        let renamed = directory.join("renamed.rs");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {\n    target();\n}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();

        navigate_to(&mut application, &state, &second, 1, 4);
        fs::rename(&second, &renamed).unwrap();
        application.remap_open_paths_for_tests(&second, &renamed);
        assert_active_path(&application, &renamed);
        assert_eq!(
            application.navigation_history()[0].target.path.as_deref(),
            Some(fs::canonicalize(&renamed).unwrap().as_path())
        );

        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &first);
        press_alt(&mut application, KeyCode::Right);

        assert_active_path(&application, &renamed);
        assert_eq!(cursor_line_column(&application), (1, 4));
        assert_eq!(application.tab_count(), 2);
    }

    #[test]
    fn navigation_history_does_not_switch_workspaces_when_restoring() {
        let directory = TempDir::new("history-workspace");
        let first_workspace = directory.join("projeto-um");
        let second_workspace = directory.join("projeto-dois");
        fs::create_dir_all(&first_workspace).unwrap();
        fs::create_dir_all(&second_workspace).unwrap();
        let first = first_workspace.join("a.rs");
        let second = first_workspace.join("b.rs");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {\n    target();\n}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_workspace(&first_workspace).unwrap();
        let target_generation = application.workspace_generation;
        application.open_document_in_new_tab(&first).unwrap();

        navigate_to(&mut application, &state, &second, 1, 4);
        press_alt(&mut application, KeyCode::Left);
        let second_index = tab_index_for_path(&application, &second);
        application.close_tab(second_index);

        application.open_workspace(&second_workspace).unwrap();
        assert!(application.workspace_generation > target_generation);
        let current_root = application.workspace().unwrap().root().to_path_buf();
        assert_eq!(
            application.navigation_history()[0]
                .target
                .workspace_generation,
            target_generation
        );

        press_alt(&mut application, KeyCode::Right);

        assert_active_path(&application, &second);
        assert_eq!(cursor_line_column(&application), (1, 4));
        assert_eq!(
            application.workspace().map(|workspace| workspace.root()),
            Some(current_root.as_path())
        );
    }

    #[test]
    fn navigation_history_deduplicates_same_location_but_keeps_same_file_positions() {
        let directory = TempDir::new("history-same-file");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {\n}\nfn one() {}\nfn two() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        navigate_to(&mut application, &state, &path, 2, 3);
        assert_eq!(application.navigation_history().len(), 1);

        navigate_to(&mut application, &state, &path, 2, 3);
        assert_eq!(
            application.navigation_history().len(),
            1,
            "navegação para a mesma localização não duplica a entrada"
        );

        navigate_to(&mut application, &state, &path, 3, 3);
        assert_eq!(application.navigation_history().len(), 2);
        assert_eq!(cursor_line_column(&application), (3, 3));

        press_alt(&mut application, KeyCode::Left);
        assert_eq!(cursor_line_column(&application), (2, 3));
        press_alt(&mut application, KeyCode::Left);
        assert_eq!(cursor_line_column(&application), (0, 0));
        press_alt(&mut application, KeyCode::Right);
        assert_eq!(cursor_line_column(&application), (2, 3));
        press_alt(&mut application, KeyCode::Right);
        assert_eq!(cursor_line_column(&application), (3, 3));
    }

    #[test]
    fn navigation_history_discards_forward_entries_after_a_new_navigation() {
        let directory = TempDir::new("history-branch");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        let third = directory.join("c.rs");
        let fourth = directory.join("d.rs");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {}\n").unwrap();
        fs::write(&third, "fn c() {}\n").unwrap();
        fs::write(&fourth, "fn d() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();

        navigate_to(&mut application, &state, &second, 0, 3);
        navigate_to(&mut application, &state, &third, 0, 3);
        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &second);

        navigate_to(&mut application, &state, &fourth, 0, 3);
        assert_active_path(&application, &fourth);
        assert_eq!(application.navigation_history().len(), 2);
        assert!(!application.can_navigate_forward());

        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &second);
        press_alt(&mut application, KeyCode::Left);
        assert_active_path(&application, &first);
        press_alt(&mut application, KeyCode::Right);
        assert_active_path(&application, &second);
        press_alt(&mut application, KeyCode::Right);
        assert_active_path(&application, &fourth);
    }

    #[test]
    fn navigation_history_ignores_stale_timeout_cancel_and_pending_requests() {
        let directory = TempDir::new("history-not-recorded");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        let third = directory.join("c.rs");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {\n    target();\n}\n").unwrap();
        fs::write(&third, "fn c() {}\n").unwrap();
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));
        application.open_document_in_new_tab(&first).unwrap();

        navigate_to(&mut application, &state, &second, 1, 4);
        assert_eq!(application.navigation_history().len(), 1);

        // Requisição pendente não cria entrada.
        let pending = request_definition(&mut application);
        assert_eq!(application.navigation_history().len(), 1);
        application.cancel_navigation(pending.operation_id);

        // Resposta stale não cria entrada.
        let stale = request_definition(&mut application);
        application
            .view
            .set_cursor(&application.buffer, CharOffset(2))
            .unwrap();
        finish_navigation(
            &mut application,
            &state,
            &stale,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&nexus_lsp::uri::path_to_uri(&third), 0, 0),
            ])),
        );
        assert!(application.navigation_history().len() == 1);
        assert_active_path(&application, &second);

        // Timeout não cria entrada.
        let timed_out = request_definition(&mut application);
        finish_navigation(
            &mut application,
            &state,
            &timed_out,
            NavigationOutcome::TimedOut {
                duration: Duration::from_secs(1),
            },
        );
        assert_eq!(application.navigation_history().len(), 1);

        // Cancelar o seletor também não cria entrada.
        let multiple = request_definition(&mut application);
        let uri = nexus_lsp::uri::path_to_uri(&third);
        finish_navigation(
            &mut application,
            &state,
            &multiple,
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(vec![
                destination(&uri, 0, 0),
                destination(&uri, 0, 3),
            ])),
        );
        assert_eq!(application.mode(), Mode::NavigationChooser);
        press(&mut application, KeyCode::Esc);
        assert_eq!(application.mode(), Mode::Editor);
        assert_eq!(application.navigation_history().len(), 1);
        assert!(application.can_navigate_back());
    }

    #[test]
    fn f12_without_a_document_reports_indisponibility_without_requesting() {
        let state = Arc::new(Mutex::new(NavigationSinkState::default()));
        let mut application = app("");
        application.set_sync_sink(Box::new(NavigationTestSink(state.clone())));

        press(&mut application, KeyCode::F(12));

        assert!(state.lock().unwrap().requested.is_empty());
        assert!(application.active_navigation.is_none());
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(MessageKind::Info)
        );
    }

    #[test]
    fn buffer_events_are_forwarded_to_lsp_sync() {
        use crate::document_sync::{ChangeSet, SyncEvent};

        let directory = TempDir::new("lsp-sync");
        let path = directory.join("main.rs");
        fs::write(&path, "fn main() {}\n").unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let mut application = app("");
        application.set_sync_sink(Box::new(RecordingSyncSink(events.clone())));
        application.open_document_in_new_tab(&path).unwrap();

        assert!(matches!(
            events.lock().unwrap().first(),
            Some(SyncEvent::Open { language_id, .. }) if language_id == "rust"
        ));

        application.insert_text("x", EditKind::Insert);
        application.commit_session();
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            SyncEvent::Change {
                changes: ChangeSet::Edits(changes),
                ..
            } if !changes.is_empty()
        )));

        application.save_document_to(&path).unwrap();
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, SyncEvent::Save { .. }))
        );

        let index = application.documents.active_index();
        application.close_tab_now(index);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, SyncEvent::Close { .. }))
        );
    }
}
