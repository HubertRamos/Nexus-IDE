//! Ponte entre os eventos dos buffers e a sincronização LSP.
//!
//! Este módulo não conhece Ratatui, processos ou detalhes JSON-RPC. O `App`
//! informa abertura, edição, salvamento e fechamento; a ponte normaliza o
//! caminho e encaminha [`SyncEvent`]s e operações tipadas de navegação ao
//! [`SyncSink`].
//!
//! Um sink de produção pode manter um servidor por `(workspace, linguagem)` e
//! traduzir os eventos para `DocumentSync`. Vários documentos do mesmo sink
//! compartilham o servidor; fechar um deles não encerra o processo.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nexus_lsp::completion::CompletionResponse;
use nexus_lsp::protocol::{NavigationMethod, Position, PublishDiagnosticsParams};
use nexus_lsp::service::{CompletionOperationId, NavigationCompletion, NavigationOperationId};

/// Resultado assíncrono de uma solicitação de completion.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResult {
    pub operation_id: CompletionOperationId,
    pub uri: String,
    pub document_version: Option<i64>,
    pub result: Result<Option<CompletionResponse>, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiagnosticsKey {
    pub workspace: String,
    pub language_id: String,
    pub session_id: i64,
    pub uri: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticFreshness {
    Current,
    Unverified,
    PotentiallyStale,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredDiagnostics {
    pub key: DiagnosticsKey,
    pub current_document_version: Option<i64>,
    pub publication: PublishDiagnosticsParams,
    pub freshness: DiagnosticFreshness,
}
use nexus_core::buffer::Transaction;
use nexus_lsp::sync::TextChange;
use nexus_syntax::LanguageId;

/// Conjunto de alterações produzido por uma edição do buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeSet {
    /// Edições na ordem em que foram aplicadas, em offsets de caracteres.
    Edits(Vec<TextChange>),
    /// O conteúdo mudou fora de uma transação (Undo/Redo ou recarga).
    Resync,
}

/// Evento de sincronização independente de servidor e de interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    Open {
        path: PathBuf,
        language_id: String,
        text: String,
    },
    Change {
        path: PathBuf,
        language_id: String,
        changes: ChangeSet,
        text: String,
    },
    Save {
        path: PathBuf,
        text: String,
    },
    Close {
        path: PathBuf,
    },
}

/// Destino dos eventos de sincronização.
///
/// A implementação concreta decide como agrupar documentos em servidores. A
/// ponte não inicia nem encerra processos.
pub trait SyncSink {
    fn handle(&mut self, event: SyncEvent);

    /// Informa a raiz do workspace ativo, quando houver.
    ///
    /// Sinks que compartilham servidores por workspace usam este valor para
    /// decidir a chave do pool. A implementação padrão ignora a mudança.
    fn workspace_changed(&mut self, _workspace: Option<&Path>) {}

    /// Coleta avisos assíncronos produzidos pelo sink.
    ///
    /// A ponte não interpreta os avisos; apenas os entrega à interface.
    fn drain_notices(&mut self) -> Vec<String> {
        Vec::new()
    }

    fn drain_diagnostics(&mut self) -> Vec<StoredDiagnostics> {
        Vec::new()
    }

    /// Coleta conclusões tipadas de navegação para a camada de aplicação.
    fn drain_navigation_results(&mut self) -> Vec<NavigationCompletion> {
        Vec::new()
    }

    /// Enfileira uma navegação para o serviço associado a este sink.
    fn request_navigation(
        &mut self,
        _workspace: String,
        _language_id: String,
        _method: NavigationMethod,
        _uri: String,
        _position: Position,
    ) -> Option<NavigationOperationId> {
        None
    }

    fn cancel_navigation(&mut self, _operation_id: NavigationOperationId) {}

    /// Solicita sugestões para um documento aberto no sink.
    fn request_completion(
        &mut self,
        _path: &Path,
        _position: Position,
        _trigger_kind: u8,
        _trigger_character: Option<String>,
    ) -> Option<CompletionOperationId> {
        None
    }

    /// Coleta respostas assíncronas de completion.
    fn drain_completion_results(&mut self) -> Vec<CompletionResult> {
        Vec::new()
    }

    fn current_session_id(&mut self, _workspace: &str, _language_id: &str) -> Option<i64> {
        None
    }

    /// Encerra recursos do sink depois que os documentos foram fechados.
    ///
    /// A implementação padrão não faz nada. Sinks que mantêm processos devem
    /// usar este ponto para finalizar threads/runtimes de forma idempotente.
    fn shutdown(&mut self) {}
}

#[derive(Debug, Clone)]
struct OpenState {
    language_id: String,
    text: String,
}

/// Encaminha eventos do documento ativo para o sink configurado.
#[derive(Default)]
pub struct DocumentSyncBridge {
    sink: Option<Box<dyn SyncSink>>,
    /// Impede que uma segunda chamada repita a finalização do sink.
    shutdown: bool,
    /// Raiz do workspace ativo, repassada ao sink.
    workspace: Option<PathBuf>,
    /// Caminhos normalizados atualmente abertos e o último conteúdo enviado.
    open: BTreeMap<PathBuf, OpenState>,
}

impl DocumentSyncBridge {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_sink(sink: Box<dyn SyncSink>) -> Self {
        let mut bridge = Self::default();
        bridge.set_sink(sink);
        bridge
    }

    pub fn set_sink(&mut self, sink: Box<dyn SyncSink>) {
        self.sink = Some(sink);
        if let Some(sink) = self.sink.as_mut() {
            sink.workspace_changed(self.workspace.as_deref());
        }

        // Documentos podem ter sido restaurados antes de o sink existir. A
        // ponte já os rastreia, mas o sink novo ainda não recebeu `didOpen`;
        // reemiti-los aqui evita abas silenciosamente dessincronizadas.
        let open: Vec<(PathBuf, String, String)> = self
            .open
            .iter()
            .map(|(path, state)| (path.clone(), state.language_id.clone(), state.text.clone()))
            .collect();
        for (path, language_id, text) in open {
            self.emit(SyncEvent::Open {
                path,
                language_id,
                text,
            });
        }
    }

    /// Atualiza o workspace ativo e o repassa ao sink.
    ///
    /// Documentos cuja chave efetiva de workspace muda são fechados no contexto
    /// antigo e reabertos no novo. Isso envia `didClose` ao servidor anterior e
    /// garante que o `ServerPool` use a raiz correta, sem deixar URIs órfãs.
    pub fn set_workspace(&mut self, workspace: Option<&Path>) {
        let new_workspace = workspace.map(Path::to_path_buf);
        let old_workspace = self.workspace.clone();
        let mut rekey = Vec::new();

        if old_workspace != new_workspace {
            for (path, state) in &self.open {
                if workspace_key(path, old_workspace.as_deref())
                    != workspace_key(path, new_workspace.as_deref())
                {
                    rekey.push((path.clone(), state.text.clone()));
                }
            }

            for (path, _) in &rekey {
                self.close_path(path);
            }
        }

        self.workspace = new_workspace;
        if let Some(sink) = self.sink.as_mut() {
            sink.workspace_changed(workspace);
        }

        for (path, text) in rekey {
            self.open(&path, &text);
        }
    }

    /// Fecha todos os documentos e encerra o sink de forma ordenada.
    ///
    /// É idempotente: fechar uma lista vazia não emite eventos e o sink decide
    /// como tornar seu próprio encerramento repetível.
    pub fn shutdown(&mut self) {
        if self.shutdown {
            return;
        }

        self.close_all();
        if let Some(sink) = self.sink.as_mut() {
            sink.shutdown();
        }
        self.shutdown = true;
    }

    /// Coleta avisos assíncronos do sink.
    pub fn drain_notices(&mut self) -> Vec<String> {
        self.sink
            .as_mut()
            .map(|sink| sink.drain_notices())
            .unwrap_or_default()
    }

    pub fn drain_diagnostics(&mut self) -> Vec<StoredDiagnostics> {
        self.sink
            .as_mut()
            .map(|sink| sink.drain_diagnostics())
            .unwrap_or_default()
    }

    pub fn drain_navigation_results(&mut self) -> Vec<NavigationCompletion> {
        self.sink
            .as_mut()
            .map(|sink| sink.drain_navigation_results())
            .unwrap_or_default()
    }

    pub fn request_navigation(
        &mut self,
        workspace: impl Into<String>,
        language_id: impl Into<String>,
        method: NavigationMethod,
        uri: impl Into<String>,
        position: Position,
    ) -> Option<NavigationOperationId> {
        self.sink.as_mut()?.request_navigation(
            workspace.into(),
            language_id.into(),
            method,
            uri.into(),
            position,
        )
    }

    pub fn cancel_navigation(&mut self, operation_id: NavigationOperationId) {
        if let Some(sink) = self.sink.as_mut() {
            sink.cancel_navigation(operation_id);
        }
    }

    /// Solicita completion para um documento rastreado pela ponte.
    pub fn request_completion(
        &mut self,
        path: &Path,
        position: Position,
        trigger_kind: u8,
        trigger_character: Option<String>,
    ) -> Option<CompletionOperationId> {
        self.sink.as_mut()?.request_completion(
            &normalize(path),
            position,
            trigger_kind,
            trigger_character,
        )
    }

    pub fn drain_completion_results(&mut self) -> Vec<CompletionResult> {
        self.sink
            .as_mut()
            .map(|sink| sink.drain_completion_results())
            .unwrap_or_default()
    }

    /// Workspace efetivo usado para rotear este caminho ao servidor LSP.
    pub fn workspace_for(&self, path: &Path) -> String {
        workspace_key(&normalize(path), self.workspace.as_deref())
            .to_string_lossy()
            .into_owned()
    }

    /// Sessão corrente do servidor, atualizando primeiro os eventos pendentes.
    pub fn current_session_id(&mut self, workspace: &str, language_id: &str) -> Option<i64> {
        self.sink
            .as_mut()?
            .current_session_id(workspace, language_id)
    }

    pub fn has_sink(&self) -> bool {
        self.sink.is_some()
    }

    pub fn is_open(&self, path: &Path) -> bool {
        self.open.contains_key(&normalize(path))
    }

    pub fn open_paths(&self) -> impl Iterator<Item = &Path> {
        self.open.keys().map(PathBuf::as_path)
    }

    /// Registra a abertura de um documento com linguagem reconhecida.
    ///
    /// Documentos sem linguagem LSP (texto simples, sem título) são ignorados.
    /// Reabrir o mesmo caminho com a mesma linguagem é idempotente.
    pub fn open(&mut self, path: &Path, text: &str) {
        let Some(language_id) = language_id_for_path(path) else {
            return;
        };

        let path = normalize(path);
        if let Some(existing) = self.open.get(&path) {
            if existing.language_id == language_id {
                return;
            }
            self.close_path(&path);
        }

        self.open.insert(
            path.clone(),
            OpenState {
                language_id: language_id.to_owned(),
                text: text.to_owned(),
            },
        );
        self.emit(SyncEvent::Open {
            path,
            language_id: language_id.to_owned(),
            text: text.to_owned(),
        });
    }

    /// Encaminha as edições de uma transação já aplicada ao buffer.
    pub fn change(&mut self, path: &Path, transaction: &Transaction, text: &str) {
        let path = normalize(path);
        let Some(state) = self.open.get(&path) else {
            return;
        };
        if state.text == text {
            return;
        }
        let language_id = state.language_id.clone();

        let changes: Vec<TextChange> = transaction
            .edits()
            .iter()
            .map(|edit| {
                TextChange::new(
                    edit.range.start.0,
                    edit.range.end.0,
                    edit.inserted_text.clone(),
                )
            })
            .collect();

        self.mark_text(&path, text);

        if changes.is_empty() {
            self.emit(SyncEvent::Change {
                path,
                language_id,
                changes: ChangeSet::Resync,
                text: text.to_owned(),
            });
        } else {
            self.emit(SyncEvent::Change {
                path,
                language_id,
                changes: ChangeSet::Edits(changes),
                text: text.to_owned(),
            });
        }
    }

    /// Recalcula o conteúdo após Undo/Redo ou recarregamento.
    pub fn resync(&mut self, path: &Path, text: &str) {
        let path = normalize(path);
        let Some(state) = self.open.get(&path) else {
            return;
        };
        if state.text == text {
            return;
        }
        let language_id = state.language_id.clone();

        self.mark_text(&path, text);
        self.emit(SyncEvent::Change {
            path,
            language_id,
            changes: ChangeSet::Resync,
            text: text.to_owned(),
        });
    }

    /// Informa que o conteúdo foi gravado no disco.
    pub fn save(&mut self, path: &Path, text: &str) {
        let path = normalize(path);
        if !self.open.contains_key(&path) {
            return;
        }

        self.mark_text(&path, text);
        self.emit(SyncEvent::Save {
            path,
            text: text.to_owned(),
        });
    }

    /// Remove o documento do sink sem encerrar servidores compartilhados.
    pub fn close(&mut self, path: &Path) {
        self.close_path(&normalize(path));
    }

    /// Fecha todos os documentos conhecidos.
    pub fn close_all(&mut self) {
        let paths: Vec<PathBuf> = self.open.keys().cloned().collect();
        for path in paths {
            self.close_path(&path);
        }
    }

    /// Move a identidade de um documento após Salvar Como ou renomeação.
    pub fn rename(&mut self, old: &Path, new: &Path, text: &str) {
        let old = normalize(old);
        let new = normalize(new);
        if old == new {
            self.save(&new, text);
            return;
        }

        self.close_path(&old);
        self.open(&new, text);
    }

    fn mark_text(&mut self, path: &Path, text: &str) {
        if let Some(state) = self.open.get_mut(path) {
            state.text = text.to_owned();
        }
    }

    fn close_path(&mut self, path: &Path) {
        if self.open.remove(path).is_some() {
            self.emit(SyncEvent::Close {
                path: path.to_path_buf(),
            });
        }
    }

    fn emit(&mut self, event: SyncEvent) {
        if let Some(sink) = self.sink.as_mut() {
            sink.handle(event);
        }
    }
}

/// Identificador LSP para as linguagens reconhecidas pelo editor.
pub fn language_id_for_path(path: &Path) -> Option<&'static str> {
    match LanguageId::from_path(path)? {
        LanguageId::Rust => Some("rust"),
        LanguageId::Python => Some("python"),
        LanguageId::JavaScript => Some("javascript"),
        LanguageId::TypeScript => Some("typescript"),
        LanguageId::Tsx => Some("typescriptreact"),
    }
}

fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    if path.is_absolute() {
        return path.to_path_buf();
    }

    std::env::current_dir()
        .map(|directory| directory.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Chave efetiva de workspace para um documento.
///
/// A regra precisa ser idêntica à usada por [`crate::lsp_sink::LspSyncSink`]:
/// documentos dentro da raiz ativa pertencem a ela; documentos fora usam o
/// diretório pai como raiz independente. Comparar as chaves antes e depois de
/// uma troca evita reabrir documentos que continuariam no mesmo servidor.
fn workspace_key(path: &Path, workspace: Option<&Path>) -> PathBuf {
    if let Some(root) = workspace {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if path.starts_with(&root) {
            return root;
        }
    }

    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use nexus_core::buffer::Buffer;
    use nexus_core::buffer::edit::{Edit, TextRange};
    use nexus_core::buffer::position::CharOffset;

    use super::*;

    #[derive(Clone, Default)]
    struct Recorder {
        events: Arc<Mutex<Vec<SyncEvent>>>,
        shutdowns: Arc<std::sync::atomic::AtomicUsize>,
        workspace_changes: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Recorder {
        fn events(&self) -> Vec<SyncEvent> {
            self.events.lock().unwrap().clone()
        }

        fn shutdowns(&self) -> usize {
            self.shutdowns.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn workspace_changes(&self) -> usize {
            self.workspace_changes
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl SyncSink for Recorder {
        fn handle(&mut self, event: SyncEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn workspace_changed(&mut self, _workspace: Option<&Path>) {
            self.workspace_changes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn shutdown(&mut self) {
            self.shutdowns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn bridge() -> (DocumentSyncBridge, Recorder) {
        let recorder = Recorder::default();
        let bridge = DocumentSyncBridge::with_sink(Box::new(recorder.clone()));
        (bridge, recorder)
    }

    fn transaction(text: &str, range: TextRange, inserted: &str) -> (Transaction, String) {
        let mut buffer = Buffer::from_text(text);
        let transaction = buffer
            .apply_transaction(vec![Edit::new(range, inserted.to_owned())])
            .unwrap()
            .unwrap();
        (transaction, buffer.text().unwrap())
    }

    #[test]
    fn emits_open_change_save_and_close_for_recognized_language() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-tui-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.rs");
        std::fs::write(&path, "abc").unwrap();

        let (mut bridge, recorder) = bridge();
        bridge.open(&path, "abc");

        let (transaction, text) =
            transaction("abc", TextRange::new(CharOffset(1), CharOffset(2)), "X");
        bridge.change(&path, &transaction, &text);
        bridge.save(&path, "aXc");
        bridge.close(&path);

        let events = recorder.events();
        assert_eq!(events.len(), 4);
        assert!(matches!(
            &events[0],
            SyncEvent::Open { path: event_path, language_id, text }
                if event_path == &normalize(&path) && language_id == "rust" && text == "abc"
        ));
        assert!(matches!(
            &events[1],
            SyncEvent::Change { changes: ChangeSet::Edits(changes), text, .. }
                if changes.len() == 1 && changes[0].start == 1 && text == "aXc"
        ));
        assert!(matches!(&events[2], SyncEvent::Save { text, .. } if text == "aXc"));
        assert!(matches!(&events[3], SyncEvent::Close { .. }));
        assert!(!bridge.is_open(&path));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resync_is_emitted_for_undo_or_reload() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-tui-resync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.py");
        std::fs::write(&path, "a").unwrap();

        let (mut bridge, recorder) = bridge();
        bridge.open(&path, "a");
        bridge.resync(&path, "novo");

        let events = recorder.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[1],
            SyncEvent::Change { changes: ChangeSet::Resync, text, language_id, .. }
                if text == "novo" && language_id == "python"
        ));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn same_name_in_different_directories_is_independent() {
        let root = std::env::temp_dir().join(format!(
            "nexus-tui-same-name-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = root.join("a/main.rs");
        let second = root.join("b/main.rs");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&first, "a").unwrap();
        std::fs::write(&second, "b").unwrap();

        let (mut bridge, recorder) = bridge();
        bridge.open(&first, "a");
        bridge.open(&second, "b");
        bridge.close(&first);

        assert!(!bridge.is_open(&first));
        assert!(bridge.is_open(&second));
        assert_eq!(recorder.events().len(), 3);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_language_is_ignored() {
        let (mut bridge, recorder) = bridge();
        bridge.open(Path::new("/tmp/notas.txt"), "texto");
        bridge.save(Path::new("/tmp/notas.txt"), "texto");

        assert!(recorder.events().is_empty());
        assert!(!bridge.is_open(Path::new("/tmp/notas.txt")));
    }

    #[test]
    fn rename_closes_old_path_and_opens_new_one() {
        let root = std::env::temp_dir().join(format!(
            "nexus-tui-rename-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let old = root.join("old.rs");
        let new = root.join("new.rs");
        std::fs::write(&old, "a").unwrap();
        std::fs::write(&new, "a").unwrap();

        let (mut bridge, recorder) = bridge();
        bridge.open(&old, "a");
        bridge.rename(&old, &new, "a");

        assert!(!bridge.is_open(&old));
        assert!(bridge.is_open(&new));
        let events = recorder.events();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[1], SyncEvent::Close { .. }));
        assert!(matches!(&events[2], SyncEvent::Open { .. }));

        let _ = std::fs::remove_dir_all(root);
    }

    fn workspace_fixture(label: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "nexus-tui-workspace-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("projeto");
        let nested = project.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("main.rs");
        std::fs::write(&file, "a").unwrap();
        (root, project, file)
    }

    #[test]
    fn changing_workspace_rekeys_documents_once() {
        let (root, project, file) = workspace_fixture("rekey");
        let (mut bridge, recorder) = bridge();
        bridge.open(&file, "a");
        let before_changes = recorder.workspace_changes();

        // A chave efetiva passa de `projeto/src` para `projeto`: o documento é
        // fechado no contexto antigo e reaberto no novo.
        bridge.set_workspace(Some(&project));

        let events = recorder.events();
        assert_eq!(events.len(), 3, "eventos: {events:#?}");
        assert!(matches!(&events[1], SyncEvent::Close { .. }));
        assert!(matches!(
            &events[2],
            SyncEvent::Open { path, .. } if path == &normalize(&file)
        ));
        assert!(bridge.is_open(&file));
        assert_eq!(recorder.workspace_changes(), before_changes + 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn setting_the_same_workspace_does_not_duplicate_events() {
        let (root, project, file) = workspace_fixture("idempotent");
        let (mut bridge, recorder) = bridge();
        bridge.open(&file, "a");
        let before_changes = recorder.workspace_changes();

        // A chave efetiva continua `projeto/src`, então não há close/open.
        bridge.set_workspace(Some(&nested_of(&project)));
        bridge.set_workspace(Some(&nested_of(&project)));

        assert_eq!(recorder.events().len(), 1);
        assert!(bridge.is_open(&file));
        assert_eq!(recorder.workspace_changes(), before_changes + 2);

        let _ = std::fs::remove_dir_all(root);
    }

    fn nested_of(project: &Path) -> PathBuf {
        project.join("src")
    }

    #[test]
    fn closing_the_workspace_moves_documents_to_their_parent_scope() {
        let (root, project, file) = workspace_fixture("close");
        let (mut bridge, recorder) = bridge();
        bridge.open(&file, "a");
        bridge.set_workspace(Some(&project));

        let before = recorder.events().len();
        bridge.set_workspace(None);

        let events = recorder.events();
        assert_eq!(events.len(), before + 2, "eventos: {events:#?}");
        assert!(matches!(&events[before], SyncEvent::Close { .. }));
        assert!(matches!(&events[before + 1], SyncEvent::Open { .. }));
        assert!(bridge.is_open(&file));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shutdown_closes_every_document_and_is_idempotent() {
        let (root, _project, file) = workspace_fixture("shutdown");
        let (mut bridge, recorder) = bridge();
        bridge.open(&file, "a");

        bridge.shutdown();
        let events = recorder.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1], SyncEvent::Close { .. }));
        assert_eq!(recorder.shutdowns(), 1);
        assert!(!bridge.is_open(&file));

        bridge.shutdown();
        assert_eq!(recorder.events().len(), 2);
        assert_eq!(recorder.shutdowns(), 1);

        let _ = std::fs::remove_dir_all(root);
    }
}
