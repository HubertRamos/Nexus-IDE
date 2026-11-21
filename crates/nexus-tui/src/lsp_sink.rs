//! Sink de produção que liga os eventos do editor ao serviço LSP.
//!
//! O sink é a única camada que conhece, ao mesmo tempo, caminhos de arquivo,
//! URIs, linguagem e workspace. Ele converte o caminho em URI com
//! [`nexus_lsp::uri::path_to_uri`], escolhe a chave `(workspace, linguagem)` e
//! envia o comando ao [`LspService`]. O serviço mantém um [`ServerPool`] e
//! compartilha um processo entre todos os documentos da mesma chave.
//!
//! Fechar um documento envia apenas `didClose`; o processo continua vivo para
//! os demais documentos. O processo só é encerrado quando o serviço é
//! descartado.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nexus_lsp::completion::CompletionResponse;
use nexus_lsp::protocol::Position;
use nexus_lsp::service::{CompletionOperationId, LspService, NavigationCompletion, ServiceEvent};

use crate::document_sync::{
    ChangeSet, CompletionResult, DiagnosticFreshness, DiagnosticsKey, StoredDiagnostics,
    SyncEvent, SyncSink,
};
use nexus_lsp::protocol::PublishDiagnosticsParams;

/// Para onde um documento aberto foi encaminhado.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentRoute {
    workspace: String,
    language_id: String,
    /// Último conteúdo conhecido. Permite reconstruir o documento quando o
    /// servidor compartilhado falha e é substituído por um processo novo.
    text: String,
}

/// Sink concreto usado pela aplicação.
pub struct LspSyncSink {
    service: LspService,
    workspace: Option<PathBuf>,
    routes: BTreeMap<PathBuf, DocumentRoute>,
    recovery: BTreeMap<(String, String), RecoveryState>,
    diagnostics: BTreeMap<DiagnosticsKey, StoredDiagnostics>,
    sessions: BTreeMap<(String, String), i64>,
    navigation_results: VecDeque<NavigationCompletion>,
    completion_results: VecDeque<CompletionResult>,
    pending_notices: VecDeque<String>,
}

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
];

#[derive(Debug, Clone)]
struct RecoveryState {
    /// Tentativas automáticas já enfileiradas nesta falha.
    attempts: usize,
    retry_at: Option<Instant>,
}

impl LspSyncSink {
    pub fn new(service: LspService) -> Self {
        Self {
            service,
            workspace: None,
            routes: BTreeMap::new(),
            recovery: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
            sessions: BTreeMap::new(),
            navigation_results: VecDeque::new(),
            completion_results: VecDeque::new(),
            pending_notices: VecDeque::new(),
        }
    }

    /// Define a raiz do workspace ativo.
    ///
    /// O caminho é canonicalizado para coincidir com a normalização aplicada
    /// pela ponte aos caminhos dos documentos.
    pub fn set_workspace(&mut self, workspace: Option<&Path>) {
        self.workspace = workspace
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    }

    /// Workspace usado como chave do pool para um documento.
    ///
    /// Documentos dentro do workspace ativo pertencem a ele. Fora dele, o
    /// diretório pai funciona como raiz independente, preservando a separação
    /// entre projetos que não foram abertos explicitamente.
    fn workspace_for(&self, path: &Path) -> String {
        if let Some(root) = &self.workspace
            && path.starts_with(root)
        {
            return root.to_string_lossy().into_owned();
        }

        path.parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    }
}

impl SyncSink for LspSyncSink {
    fn handle(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::Open {
                path,
                language_id,
                text,
            } => {
                let workspace = self.workspace_for(&path);
                let uri = nexus_lsp::uri::path_to_uri(&path);

                self.routes.insert(
                    path,
                    DocumentRoute {
                        workspace: workspace.clone(),
                        language_id: language_id.clone(),
                        text: text.clone(),
                    },
                );

                self.reset_exhausted_recovery(&workspace, &language_id);

                self.service.open(workspace, language_id, uri, text);
            }
            SyncEvent::Change {
                path,
                changes,
                text,
                ..
            } => {
                let Some(route) = self.routes.get_mut(&path) else {
                    return;
                };
                let workspace = route.workspace.clone();
                let language_id = route.language_id.clone();
                route.text = text.clone();
                let uri = nexus_lsp::uri::path_to_uri(&path);
                for stored in self
                    .diagnostics
                    .values_mut()
                    .filter(|stored| stored.key.uri == uri)
                {
                    stored.freshness = DiagnosticFreshness::PotentiallyStale;
                }
                self.reset_exhausted_recovery(&workspace, &language_id);

                match changes {
                    ChangeSet::Edits(changes) => {
                        self.service
                            .change(workspace, language_id, uri, changes, text);
                    }
                    ChangeSet::Resync => {
                        self.service.resync(workspace, language_id, uri, text);
                    }
                }
            }
            SyncEvent::Save { path, text } => {
                let Some(route) = self.routes.get_mut(&path) else {
                    return;
                };
                let workspace = route.workspace.clone();
                let language_id = route.language_id.clone();
                route.text = text.clone();
                let uri = nexus_lsp::uri::path_to_uri(&path);
                self.reset_exhausted_recovery(&workspace, &language_id);

                self.service.save(workspace, language_id, uri, text);
            }
            SyncEvent::Close { path } => {
                let Some(route) = self.routes.remove(&path) else {
                    return;
                };
                let uri = nexus_lsp::uri::path_to_uri(&path);

                let key = (route.workspace, route.language_id);
                self.service.close(key.0.clone(), key.1.clone(), uri);
                self.diagnostics
                    .retain(|stored_key, _| stored_key.uri != nexus_lsp::uri::path_to_uri(&path));
                if !self
                    .routes
                    .values()
                    .any(|open| open.workspace == key.0 && open.language_id == key.1)
                {
                    self.recovery.remove(&key);
                    // Também libera uma chave bloqueada por falha de spawn,
                    // para que uma abertura futura comece um ciclo novo.
                    self.service.retry(key.0, key.1);
                }
            }
        }
    }

    fn workspace_changed(&mut self, workspace: Option<&Path>) {
        self.set_workspace(workspace);
    }

    fn drain_notices(&mut self) -> Vec<String> {
        let notices = self.poll_service_events();
        self.pending_notices.extend(notices);
        std::mem::take(&mut self.pending_notices)
            .into_iter()
            .collect()
    }

    fn drain_navigation_results(&mut self) -> Vec<NavigationCompletion> {
        let notices = self.poll_service_events();
        self.pending_notices.extend(notices);
        std::mem::take(&mut self.navigation_results)
            .into_iter()
            .collect()
    }

    fn request_navigation(
        &mut self,
        workspace: String,
        language_id: String,
        method: nexus_lsp::protocol::NavigationMethod,
        uri: String,
        position: nexus_lsp::protocol::Position,
    ) -> Option<nexus_lsp::service::NavigationOperationId> {
        Some(
            self.service
                .request_navigation(workspace, language_id, method, uri, position),
        )
    }

    fn cancel_navigation(&mut self, operation_id: nexus_lsp::service::NavigationOperationId) {
        self.service.cancel_navigation(operation_id);
    }

    fn request_completion(
        &mut self,
        path: &Path,
        position: Position,
        trigger_kind: u8,
        trigger_character: Option<String>,
    ) -> Option<CompletionOperationId> {
        let route = self.routes.get(path)?;
        let uri = nexus_lsp::uri::path_to_uri(path);
        Some(self.service.request_completion(
            route.workspace.clone(),
            route.language_id.clone(),
            uri,
            position,
            trigger_kind,
            trigger_character,
        ))
    }

    fn drain_completion_results(&mut self) -> Vec<CompletionResult> {
        let notices = self.poll_service_events();
        self.pending_notices.extend(notices);
        std::mem::take(&mut self.completion_results).into_iter().collect()
    }

    fn current_session_id(&mut self, workspace: &str, language_id: &str) -> Option<i64> {
        let notices = self.poll_service_events();
        self.pending_notices.extend(notices);
        self.sessions
            .get(&(workspace.to_owned(), language_id.to_owned()))
            .copied()
            .filter(|session_id| *session_id > 0)
    }

    fn drain_diagnostics(&mut self) -> Vec<StoredDiagnostics> {
        self.diagnostics.values().cloned().collect()
    }

    fn shutdown(&mut self) {
        self.recovery.clear();
        self.routes.clear();
        self.diagnostics.clear();
        self.navigation_results.clear();
        self.completion_results.clear();
        self.pending_notices.clear();
        self.service.shutdown();
    }
}

impl LspSyncSink {
    fn poll_service_events(&mut self) -> Vec<String> {
        let mut notices = Vec::new();
        let mut retry = Vec::new();

        for event in self.service.drain() {
            match event {
                ServiceEvent::Failure {
                    operation,
                    uri,
                    message,
                } => {
                    if operation == nexus_lsp::service::ServiceOperation::Start
                        && let Some(uri) = uri.as_deref()
                        && let Some((workspace, language_id)) = self.route_key_for_uri(uri)
                    {
                        retry.push((workspace, language_id));
                    }
                    notices.push(match uri {
                        Some(uri) => format!("LSP ({operation}) em {uri}: {message}"),
                        None => format!("LSP ({operation}): {message}"),
                    });
                }
                ServiceEvent::ServerExited {
                    workspace,
                    language_id,
                    message,
                } => {
                    self.diagnostics.retain(|key, _| {
                        key.workspace != workspace || key.language_id != language_id
                    });
                    self.sessions
                        .insert((workspace.clone(), language_id.clone()), -1);
                    notices.push(format!(
                        "servidor LSP {language_id} de {workspace} encerrou: {message}"
                    ));
                    retry.push((workspace, language_id));
                }
                ServiceEvent::ServerStarted {
                    workspace,
                    language_id,
                    session_id,
                } => {
                    self.recovery
                        .remove(&(workspace.clone(), language_id.clone()));
                    let server = (workspace, language_id);
                    if self
                        .sessions
                        .insert(server.clone(), session_id)
                        .is_some_and(|old| old != session_id)
                    {
                        self.diagnostics.retain(|key, _| {
                            key.workspace != server.0 || key.language_id != server.1
                        });
                    }
                }
                ServiceEvent::DiagnosticsPublished {
                    workspace,
                    language_id,
                    session_id,
                    current_document_version,
                    publication,
                } => {
                    self.apply_publication(
                        workspace,
                        language_id,
                        session_id,
                        current_document_version,
                        publication,
                    );
                }
                ServiceEvent::NavigationCompleted { context, outcome } => {
                    self.navigation_results
                        .push_back(NavigationCompletion { context, outcome });
                }
                ServiceEvent::CompletionCompleted {
                    operation_id, uri, document_version, result, ..
                } => {
                    self.completion_results.push_back(CompletionResult {
                        operation_id, uri, document_version, result,
                    });
                }
                ServiceEvent::Stopped => {}
            }
        }

        for (workspace, language_id) in retry {
            self.schedule_retry(workspace, language_id);
        }

        self.run_due_retries();

        notices
    }
}

impl LspSyncSink {
    fn apply_publication(
        &mut self,
        workspace: String,
        language_id: String,
        session_id: i64,
        current_document_version: Option<i64>,
        publication: PublishDiagnosticsParams,
    ) {
        let route_workspace =
            if let Some((route_workspace, _)) = self.route_key_for_uri(&publication.uri) {
                route_workspace
            } else {
                // O servidor pode publicar diagnósticos de arquivos do workspace
                // ainda não abertos em abas. Aceitamos apenas arquivos reais sob
                // a raiz ativa, mantendo a identidade URI como chave.
                let Some(path) = nexus_lsp::uri::uri_to_path(&publication.uri) else {
                    return;
                };
                let Some(root) = self.workspace.as_deref() else {
                    return;
                };
                if !path.is_file() || !path.starts_with(root) {
                    return;
                }
                root.to_string_lossy().into_owned()
            };
        if route_workspace != workspace {
            return;
        }
        if self
            .sessions
            .get(&(workspace.clone(), language_id.clone()))
            .is_some_and(|active| *active != session_id)
        {
            return;
        }
        if publication
            .version
            .zip(current_document_version)
            .is_some_and(|(published, current)| published < current)
        {
            return;
        }
        let key = DiagnosticsKey {
            workspace,
            language_id,
            session_id,
            uri: publication.uri.clone(),
        };
        if publication.diagnostics.is_empty() {
            self.diagnostics.remove(&key);
        } else {
            let freshness = if current_document_version
                .is_some_and(|current| publication.version == Some(current))
            {
                DiagnosticFreshness::Current
            } else {
                DiagnosticFreshness::Unverified
            };
            let stored = StoredDiagnostics {
                key: key.clone(),
                current_document_version,
                publication,
                freshness,
            };
            self.diagnostics.insert(key, stored);
        }
    }

    fn route_key_for_uri(&self, uri: &str) -> Option<(String, String)> {
        self.routes.iter().find_map(|(path, route)| {
            (nexus_lsp::uri::path_to_uri(path) == uri)
                .then(|| (route.workspace.clone(), route.language_id.clone()))
        })
    }

    fn schedule_retry(&mut self, workspace: String, language_id: String) {
        let key = (workspace, language_id);
        let state = self.recovery.entry(key).or_insert(RecoveryState {
            attempts: 0,
            retry_at: None,
        });
        if state.attempts < RETRY_DELAYS.len() && state.retry_at.is_none() {
            state.retry_at = Some(Instant::now() + RETRY_DELAYS[state.attempts]);
        }
    }

    /// Atividade nova do usuário permite outro ciclo limitado depois que o
    /// ciclo automático anterior se esgotou.
    fn reset_exhausted_recovery(&mut self, workspace: &str, language_id: &str) {
        if let Some(state) = self
            .recovery
            .get_mut(&(workspace.to_owned(), language_id.to_owned()))
            && state.attempts >= RETRY_DELAYS.len()
            && state.retry_at.is_none()
        {
            state.attempts = 0;
            state.retry_at = Some(Instant::now());
        }
    }

    fn run_due_retries(&mut self) {
        let now = Instant::now();
        let due: Vec<(String, String)> = self
            .recovery
            .iter()
            .filter(|(_, state)| state.retry_at.is_some_and(|at| at <= now))
            .map(|(key, _)| key.clone())
            .collect();

        for (workspace, language_id) in due {
            let documents: Vec<(PathBuf, String)> = self
                .routes
                .iter()
                .filter(|(_, route)| {
                    route.workspace == workspace && route.language_id == language_id
                })
                .map(|(path, route)| (path.clone(), route.text.clone()))
                .collect();

            if documents.is_empty() {
                self.recovery.remove(&(workspace, language_id));
                continue;
            }

            let state = self
                .recovery
                .get_mut(&(workspace.clone(), language_id.clone()))
                .expect("tentativa selecionada continua registrada");
            state.attempts += 1;
            state.retry_at = None;

            // O comando Retry libera apenas a chave que falhou ao iniciar.
            // DidOpen posterior usa snapshots atuais, nunca deltas acumulados.
            self.service.retry(workspace.clone(), language_id.clone());
            for (path, text) in documents {
                let uri = nexus_lsp::uri::path_to_uri(&path);
                self.service
                    .open(workspace.clone(), language_id.clone(), uri, text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use nexus_lsp::manager::ServerConfig;
    use nexus_lsp::service::LspService;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::Value;

    use super::*;
    use crate::app::App;
    use crate::event::Event;
    use crate::paths::AppPaths;
    use crate::recovery::{RECOVERY_VERSION, RecoveryManager, RecoveryRecord, now_seconds};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-tui-lsp-{}-{counter}-{label}",
                std::process::id()
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

    /// Caminhos e contador usados para dar um processo independente a cada
    /// combinação `(workspace, linguagem)` criada pelo serviço.
    #[derive(Clone)]
    struct FakeEnvironment {
        base: PathBuf,
        next: Arc<AtomicUsize>,
    }

    impl FakeEnvironment {
        fn new(base: PathBuf) -> Self {
            fs::create_dir_all(&base).expect("criar diretório do fake server");
            Self {
                base,
                next: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn provider(&self) -> impl Fn(&str, &str) -> Option<ServerConfig> + Send + 'static {
            self.provider_with_mode("sync-incremental")
        }

        fn provider_with_mode(
            &self,
            mode: &'static str,
        ) -> impl Fn(&str, &str) -> Option<ServerConfig> + Send + 'static {
            self.provider_with_modes(vec![mode])
        }

        fn provider_with_modes(
            &self,
            modes: Vec<&'static str>,
        ) -> impl Fn(&str, &str) -> Option<ServerConfig> + Send + 'static {
            let base = self.base.clone();
            let next = Arc::clone(&self.next);

            move |_workspace, language_id| {
                if language_id != "rust" && language_id != "python" {
                    return None;
                }

                let index = next.fetch_add(1, Ordering::SeqCst);
                let mode = modes
                    .get(index)
                    .copied()
                    .or_else(|| modes.last().copied())
                    .unwrap_or("sync-incremental");
                Some(
                    ServerConfig::new(language_id, fake_server_path())
                        .with_argument(mode)
                        .with_environment(
                            "NEXUS_FAKE_LSP_SYNC_FILE",
                            base.join(format!("sync-{index}.jsonl")),
                        )
                        .with_environment(
                            "NEXUS_FAKE_LSP_SNAPSHOT_FILE",
                            base.join(format!("snapshots-{index}.jsonl")),
                        )
                        .with_environment(
                            "NEXUS_FAKE_LSP_PID_FILE",
                            base.join(format!("pid-{index}.txt")),
                        )
                        .with_environment(
                            "NEXUS_FAKE_LSP_MARKER_FILE",
                            base.join(format!("markers-{index}.txt")),
                        ),
                )
            }
        }

        fn snapshot_files(&self) -> Vec<PathBuf> {
            self.files_with_prefix("snapshots-")
        }

        fn start_count(&self) -> usize {
            self.next.load(Ordering::SeqCst)
        }

        fn sync_files(&self) -> Vec<PathBuf> {
            self.files_with_prefix("sync-")
        }

        fn all_markers(&self) -> Vec<String> {
            let mut files: Vec<PathBuf> = fs::read_dir(&self.base)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("markers-"))
                })
                .collect();
            files.sort();

            files
                .iter()
                .filter_map(|path| fs::read_to_string(path).ok())
                .flat_map(|contents| contents.lines().map(str::to_owned).collect::<Vec<String>>())
                .collect()
        }

        fn files_with_prefix(&self, prefix: &str) -> Vec<PathBuf> {
            let mut files: Vec<PathBuf> = fs::read_dir(&self.base)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".jsonl"))
                })
                .collect();
            files.sort();
            files
        }

        fn all_snapshots(&self) -> Vec<Value> {
            self.snapshot_files()
                .iter()
                .flat_map(|path| read_jsonl(path))
                .collect()
        }

        fn all_events(&self) -> Vec<Value> {
            self.sync_files()
                .iter()
                .flat_map(|path| read_jsonl(path))
                .collect()
        }

        fn last_snapshot(&self, uri: &str) -> Option<Value> {
            self.all_snapshots()
                .into_iter()
                .rev()
                .find(|entry| entry.get("uri").and_then(Value::as_str) == Some(uri))
        }
    }

    fn fake_server_path() -> PathBuf {
        let mut path = std::env::current_exe().expect("executável de teste");
        path.pop();
        if path.file_name().and_then(|name| name.to_str()) == Some("deps") {
            path.pop();
        }
        path.join(format!(
            "nexus-lsp-fake-server{}",
            std::env::consts::EXE_SUFFIX
        ))
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        let Ok(contents) = fs::read_to_string(path) else {
            return Vec::new();
        };
        contents
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn uri_for(path: &Path) -> String {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        nexus_lsp::uri::path_to_uri(&canonical)
    }

    fn app_with_lsp(workspace: Option<&Path>, environment: &FakeEnvironment) -> App {
        app_with_lsp_mode(workspace, environment, "sync-incremental")
    }

    fn app_with_lsp_mode(
        workspace: Option<&Path>,
        environment: &FakeEnvironment,
        mode: &'static str,
    ) -> App {
        app_with_lsp_modes(workspace, environment, vec![mode])
    }

    fn app_with_lsp_modes(
        workspace: Option<&Path>,
        environment: &FakeEnvironment,
        modes: Vec<&'static str>,
    ) -> App {
        let mut app = App::with_text(80, 24, "");
        app.mark_rendered();

        if let Some(root) = workspace {
            app.open_workspace(root).expect("abrir workspace");
        }

        let service = LspService::with_provider(environment.provider_with_modes(modes))
            .expect("iniciar serviço");
        app.set_sync_sink(Box::new(LspSyncSink::new(service)));
        app
    }

    fn sink_with_modes(environment: &FakeEnvironment, modes: Vec<&'static str>) -> LspSyncSink {
        let service = LspService::with_provider(environment.provider_with_modes(modes))
            .expect("iniciar serviço");
        LspSyncSink::new(service)
    }

    fn wait_for_sink(sink: &mut LspSyncSink, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let _ = sink.drain_notices();
            if condition() {
                return;
            }
            assert!(Instant::now() < deadline, "tempo esgotado no teste do sink");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn sink_preserves_navigation_completion_for_application_consumer() {
        let directory = TempDir::new("navigation-completion");
        let file = directory.join("main.rs");
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut sink = sink_with_modes(&environment, vec!["navigation"]);
        sink.set_workspace(Some(&directory.path));
        let uri = uri_for(&file);
        sink.handle(SyncEvent::Open {
            path: file,
            language_id: "rust".to_owned(),
            text: "fn main() {}".to_owned(),
        });
        let startup_deadline = Instant::now() + Duration::from_secs(5);
        while sink.sessions.is_empty() {
            let _ = sink.drain_notices();
            assert!(Instant::now() < startup_deadline, "servidor não iniciou");
            std::thread::sleep(Duration::from_millis(10));
        }

        let operation_id = sink.service.request_navigation(
            directory.path.to_string_lossy().into_owned(),
            "rust",
            nexus_lsp::protocol::NavigationMethod::Definition,
            uri.clone(),
            nexus_lsp::protocol::Position {
                line: 0,
                character: 1,
            },
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let completed = sink.drain_navigation_results();
            if let Some(completion) = completed
                .into_iter()
                .find(|completion| completion.context.operation_id == operation_id)
            {
                assert_eq!(completion.context.uri, uri);
                assert!(matches!(
                    completion.outcome,
                    nexus_lsp::service::NavigationOutcome::Result(
                        nexus_lsp::protocol::NavigationResult::Destinations(_)
                    )
                ));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "conclusão não chegou à ponte da TUI"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn f12_definition_round_trip_reaches_the_application_with_the_fake_server() {
        use nexus_core::buffer::position::CharOffset;

        let directory = TempDir::new("definition-round-trip");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_mode(None, &environment, "navigation");
        app.open_document_in_new_tab(&file).unwrap();
        let uri = uri_for(&file);

        // Garante que o servidor já processou `didOpen` e tem sessão ativa.
        wait_for(|| environment.last_snapshot(&uri).is_some());
        for _ in 0..3 {
            app.update(Event::Key(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::NONE,
            )));
        }

        app.update(Event::Key(KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::NONE,
        )));

        let deadline = Instant::now() + Duration::from_secs(20);
        while app.navigation_history().is_empty() {
            app.on_idle();
            assert!(
                Instant::now() < deadline,
                "a definição não chegou à aplicação"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(app.mode(), crate::app::Mode::Editor);
        assert_eq!(app.navigation_history().len(), 1);
        let history = &app.navigation_history()[0];
        assert_eq!(
            history.method,
            nexus_lsp::protocol::NavigationMethod::Definition
        );
        assert_eq!(history.target.uri, uri);
        assert_eq!(history.origin.uri, uri);
        assert_eq!(history.target.cursor, CharOffset(3));
        assert_eq!(app.view().cursor(), CharOffset(3));
    }

    #[test]
    fn shift_f12_references_round_trip_opens_the_chooser_and_records_history() {
        use nexus_core::buffer::position::CharOffset;

        let directory = TempDir::new("references-round-trip");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_mode(None, &environment, "navigation");
        app.open_document_in_new_tab(&file).unwrap();
        let uri = uri_for(&file);

        wait_for(|| environment.last_snapshot(&uri).is_some());
        for _ in 0..3 {
            press(&mut app, KeyCode::Right);
        }

        app.update(Event::Key(KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::SHIFT,
        )));

        let deadline = Instant::now() + Duration::from_secs(20);
        while app.mode() != crate::app::Mode::NavigationChooser {
            app.on_idle();
            assert!(
                Instant::now() < deadline,
                "o seletor de referências não abriu"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let chooser = app.navigation_chooser().expect("seletor de referências");
        assert!(chooser.is_references());
        assert_eq!(chooser.title(), "Referências");
        assert_eq!(chooser.choices().len(), 1);

        press(&mut app, KeyCode::Enter);
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.navigation_history().is_empty() {
            app.on_idle();
            assert!(
                Instant::now() < deadline,
                "a referência não chegou ao histórico"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(app.mode(), crate::app::Mode::Editor);
        assert_eq!(app.navigation_history().len(), 1);
        let history = &app.navigation_history()[0];
        assert_eq!(
            history.method,
            nexus_lsp::protocol::NavigationMethod::References {
                include_declaration: true,
            }
        );
        assert_eq!(history.target.uri, uri);
        assert_eq!(history.target.cursor, CharOffset(3));
    }

    #[test]
    fn ctrl_f12_declaration_round_trip_navigates_and_records_history() {
        use nexus_core::buffer::position::CharOffset;

        let directory = TempDir::new("declaration-round-trip");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_mode(None, &environment, "navigation");
        app.open_document_in_new_tab(&file).unwrap();
        let uri = uri_for(&file);

        wait_for(|| environment.last_snapshot(&uri).is_some());
        for _ in 0..3 {
            press(&mut app, KeyCode::Right);
        }

        app.update(Event::Key(KeyEvent::new(
            KeyCode::F(12),
            KeyModifiers::CONTROL,
        )));

        let deadline = Instant::now() + Duration::from_secs(20);
        while app.navigation_history().is_empty() {
            app.on_idle();
            assert!(
                Instant::now() < deadline,
                "a declaração não chegou à aplicação"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(app.mode(), crate::app::Mode::Editor);
        assert_eq!(app.navigation_history().len(), 1);
        let history = &app.navigation_history()[0];
        assert_eq!(
            history.method,
            nexus_lsp::protocol::NavigationMethod::Declaration
        );
        assert_eq!(history.target.uri, uri);
        assert_eq!(history.target.cursor, CharOffset(3));
    }

    #[test]
    fn ctrl_t_type_definition_round_trip_navigates_and_records_history() {
        use nexus_core::buffer::position::CharOffset;

        let directory = TempDir::new("type-definition-round-trip");
        let file = directory.join("main.py");
        fs::write(&file, "x = 1\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_mode(None, &environment, "navigation");
        app.open_document_in_new_tab(&file).unwrap();
        let uri = uri_for(&file);

        wait_for(|| environment.last_snapshot(&uri).is_some());
        for _ in 0..3 {
            press(&mut app, KeyCode::Right);
        }

        app.update(Event::Key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        )));

        let deadline = Instant::now() + Duration::from_secs(20);
        while app.navigation_history().is_empty() {
            app.on_idle();
            assert!(
                Instant::now() < deadline,
                "a definição de tipo não chegou à aplicação"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(app.mode(), crate::app::Mode::Editor);
        assert_eq!(app.navigation_history().len(), 1);
        let history = &app.navigation_history()[0];
        assert_eq!(
            history.method,
            nexus_lsp::protocol::NavigationMethod::TypeDefinition
        );
        assert_eq!(history.target.uri, uri);
        assert_eq!(history.target.cursor, CharOffset(3));
    }

    fn crash_sink_document(
        sink: &mut LspSyncSink,
        environment: &FakeEnvironment,
        file: &Path,
        workspace: &Path,
    ) -> String {
        sink.set_workspace(Some(workspace));
        let uri = uri_for(file);
        sink.handle(SyncEvent::Open {
            path: fs::canonicalize(file).unwrap(),
            language_id: "rust".to_owned(),
            text: "original\n".to_owned(),
        });
        wait_for_sink(sink, || {
            // O fake publica snapshot de didOpen somente após processar a
            // notificação e atualizar seu estado em memória.
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });
        sink.service.change(
            workspace.to_string_lossy().into_owned(),
            "rust",
            uri.clone(),
            vec![nexus_lsp::sync::TextChange::new(0, 0, "x")],
            "xoriginal\n",
        );
        uri
    }

    fn press(app: &mut App, code: KeyCode) {
        app.update(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn ctrl(app: &mut App, code: KeyCode) {
        app.update(Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL)));
    }

    fn type_text(app: &mut App, text: &str) {
        for character in text.chars() {
            press(app, KeyCode::Char(character));
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if condition() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tempo esgotado aguardando o servidor LSP simulado"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Variante que drena eventos/avisos da TUI enquanto espera, permitindo
    /// que a reconstrução automática após uma falha de servidor aconteça.
    fn wait_for_app(app: &mut App, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            app.on_idle();
            if condition() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tempo esgotado aguardando o servidor LSP simulado"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn app_open_change_save_close_reaches_fake_server() {
        assert!(
            fake_server_path().is_file(),
            "binário ausente: {}",
            fake_server_path().display()
        );

        let directory = TempDir::new("open-change-save-close");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);

        app.open_document_in_new_tab(&file).expect("abrir arquivo");
        let uri = uri_for(&file);

        // Abrir um workspace coloca o foco no explorador; o fluxo real devolve
        // o foco ao editor ao abrir o arquivo pelo teclado.
        press(&mut app, KeyCode::Esc);

        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        // As cinco edições pertencem à mesma transação: o cursor avança e cada
        // range depende do estado intermediário produzido pela anterior.
        type_text(&mut app, "abc");
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "d");
        app.on_idle();

        let expected = "abc\ndfn main() {}\n";
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["text"] == expected)
        });

        ctrl(&mut app, KeyCode::Char('s'));
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didSave")
        });

        app.close_current_tab();
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didClose")
        });

        let methods: Vec<String> = environment
            .all_events()
            .iter()
            .filter_map(|event| event["method"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            methods,
            vec![
                "textDocument/didOpen",
                "textDocument/didChange",
                "textDocument/didSave",
                "textDocument/didClose",
            ]
        );

        let change = environment
            .all_events()
            .into_iter()
            .find(|event| event["method"] == "textDocument/didChange")
            .expect("didChange registrado");
        assert_eq!(
            change["params"]["contentChanges"]
                .as_array()
                .expect("contentChanges")
                .len(),
            5
        );

        assert_eq!(environment.last_snapshot(&uri).unwrap()["text"], expected);
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    }

    #[test]
    fn undo_resyncs_the_document_in_the_fake_server() {
        let directory = TempDir::new("undo-resync");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "abc\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);

        app.open_document_in_new_tab(&file).expect("abrir arquivo");
        press(&mut app, KeyCode::Esc);
        let uri = uri_for(&file);

        type_text(&mut app, "X");
        app.on_idle();
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["text"] == "Xabc\n")
        });

        ctrl(&mut app, KeyCode::Char('z'));
        app.on_idle();
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["text"] == "abc\n")
        });

        let snapshot = environment.last_snapshot(&uri).expect("snapshot final");
        assert_eq!(snapshot["method"], "textDocument/didChange");
        assert_eq!(snapshot["text"], "abc\n");
    }

    #[test]
    fn two_documents_share_one_server_and_closing_one_keeps_the_other() {
        let directory = TempDir::new("shared-server");
        let project = directory.join("projeto");
        let first = project.join("a/main.rs");
        let second = project.join("b/main.rs");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);

        app.open_document_in_new_tab(&first)
            .expect("abrir primeiro");
        app.open_document_in_new_tab(&second)
            .expect("abrir segundo");

        let first_uri = uri_for(&first);
        let second_uri = uri_for(&second);

        wait_for(|| {
            environment
                .last_snapshot(&first_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });
        wait_for(|| {
            environment
                .last_snapshot(&second_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        assert_eq!(
            environment.snapshot_files().len(),
            1,
            "documentos do mesmo workspace e linguagem devem compartilhar o processo"
        );

        // A primeira aba está inativa; fechá-la envia didClose sem encerrar o
        // processo que atende a segunda.
        app.close_tab(0);
        wait_for(|| {
            environment
                .last_snapshot(&first_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didClose")
        });

        press(&mut app, KeyCode::Esc);
        type_text(&mut app, "x");
        app.on_idle();

        let expected = "xfn b() {}\n";
        wait_for(|| {
            environment
                .last_snapshot(&second_uri)
                .is_some_and(|entry| entry["text"] == expected)
        });

        assert_eq!(
            environment.snapshot_files().len(),
            1,
            "fechar uma aba não pode iniciar outro servidor"
        );
        assert_eq!(
            environment.last_snapshot(&first_uri).unwrap()["text"],
            "fn a() {}\n"
        );
        assert_eq!(
            environment.last_snapshot(&second_uri).unwrap()["text"],
            expected
        );
    }

    #[test]
    fn same_filename_in_different_workspaces_stays_independent() {
        let directory = TempDir::new("same-name");
        let left = directory.join("esquerda");
        let right = directory.join("direita");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&right).unwrap();
        let left_file = left.join("main.rs");
        let right_file = right.join("main.rs");
        fs::write(&left_file, "fn left() {}\n").unwrap();
        fs::write(&right_file, "fn right() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(None, &environment);

        app.open_document_in_new_tab(&left_file)
            .expect("abrir esquerda");
        app.open_document_in_new_tab(&right_file)
            .expect("abrir direita");

        let left_uri = uri_for(&left_file);
        let right_uri = uri_for(&right_file);
        assert_ne!(left_uri, right_uri);

        wait_for(|| {
            environment
                .last_snapshot(&left_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });
        wait_for(|| {
            environment
                .last_snapshot(&right_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        assert_eq!(
            environment.snapshot_files().len(),
            2,
            "workspaces diferentes devem ter processos independentes"
        );

        type_text(&mut app, "r");
        app.on_idle();

        wait_for(|| {
            environment
                .last_snapshot(&right_uri)
                .is_some_and(|entry| entry["text"] == "rfn right() {}\n")
        });

        assert_eq!(
            environment.last_snapshot(&left_uri).unwrap()["text"],
            "fn left() {}\n"
        );
    }

    #[test]
    fn replacing_active_document_closes_previous_and_keeps_other_tabs() {
        let directory = TempDir::new("replace-active");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let first = project.join("a.rs");
        let second = project.join("b.rs");
        let third = project.join("c.rs");
        for (path, text) in [
            (&first, "fn a() {}\n"),
            (&second, "fn b() {}\n"),
            (&third, "fn c() {}\n"),
        ] {
            fs::write(path, text).unwrap();
        }

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);

        app.open_document_in_new_tab(&first).unwrap();
        app.open_document_in_new_tab(&second).unwrap();
        press(&mut app, KeyCode::Esc);

        let first_uri = uri_for(&first);
        let second_uri = uri_for(&second);
        let third_uri = uri_for(&third);
        wait_for(|| {
            environment
                .last_snapshot(&second_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        app.open_document_for_tests(&third).expect("substituir aba");

        wait_for(|| {
            environment
                .last_snapshot(&third_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });
        wait_for(|| {
            environment
                .last_snapshot(&second_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didClose")
        });

        assert!(
            environment
                .last_snapshot(&first_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen"),
            "a aba inativa não pode ser fechada"
        );
        assert_eq!(
            environment.snapshot_files().len(),
            1,
            "a substituição deve reutilizar o servidor compartilhado"
        );
        assert_eq!(app.tab_names(), vec!["a.rs".to_owned(), "c.rs".to_owned()]);
    }

    #[test]
    fn deleting_an_open_file_sends_did_close_and_keeps_other_tabs() {
        let directory = TempDir::new("delete-open");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let keep = project.join("keep.rs");
        let remove = project.join("remove.rs");
        fs::write(&keep, "fn keep() {}\n").unwrap();
        fs::write(&remove, "fn remove() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);

        app.open_document_in_new_tab(&keep).unwrap();
        app.open_document_in_new_tab(&remove).unwrap();

        let keep_uri = uri_for(&keep);
        let remove_uri = uri_for(&remove);
        wait_for(|| {
            environment
                .last_snapshot(&remove_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        app.perform_delete_for_tests(&remove, true);

        wait_for(|| {
            environment
                .last_snapshot(&remove_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didClose")
        });
        assert!(!remove.exists());
        assert!(
            environment
                .last_snapshot(&keep_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen"),
            "excluir um arquivo não pode fechar outro"
        );
        assert_eq!(
            app.tab_names(),
            vec!["keep.rs".to_owned(), "sem título".to_owned()]
        );
    }

    #[test]
    fn renaming_an_open_file_moves_its_lsp_identity() {
        let directory = TempDir::new("rename-open");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let old = project.join("old.rs");
        let new = project.join("new.rs");
        fs::write(&old, "fn old() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);

        app.open_document_in_new_tab(&old).unwrap();
        press(&mut app, KeyCode::Esc);
        let old_uri = uri_for(&old);
        let new_uri = uri_for(&new);
        wait_for(|| {
            environment
                .last_snapshot(&old_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        fs::rename(&old, &new).unwrap();
        app.remap_open_paths_for_tests(&old, &new);

        wait_for(|| {
            environment
                .last_snapshot(&new_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });
        wait_for(|| {
            environment
                .last_snapshot(&old_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didClose")
        });

        let reopened = environment.last_snapshot(&new_uri).unwrap();
        assert_eq!(reopened["text"], "fn old() {}\n");

        type_text(&mut app, "x");
        app.on_idle();
        wait_for(|| {
            environment
                .last_snapshot(&new_uri)
                .is_some_and(|entry| entry["text"] == "xfn old() {}\n")
        });
    }

    #[test]
    fn switching_workspace_rekeys_documents_to_the_new_scope() {
        let directory = TempDir::new("workspace-switch");
        let left = directory.join("esquerda");
        let right = directory.join("direita");
        let nested = left.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&right).unwrap();
        let file = nested.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&left), &environment);
        app.open_document_in_new_tab(&file).unwrap();

        let uri = uri_for(&file);
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });
        assert_eq!(environment.snapshot_files().len(), 1);

        app.open_workspace(&right).expect("trocar workspace");

        // O documento saiu da raiz `esquerda` e passou a usar `esquerda/src`.
        wait_for(|| {
            environment
                .all_snapshots()
                .iter()
                .any(|entry| entry["method"] == "textDocument/didClose" && entry["uri"] == uri)
        });
        wait_for(|| {
            environment
                .all_snapshots()
                .iter()
                .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
                .count()
                >= 2
        });

        assert_eq!(environment.snapshot_files().len(), 2);
        assert_eq!(
            app.workspace().map(|workspace| workspace.root()),
            Some(fs::canonicalize(&right).unwrap().as_path())
        );
        assert_eq!(
            environment
                .all_snapshots()
                .into_iter()
                .rfind(|entry| { entry["method"] == "textDocument/didOpen" && entry["uri"] == uri })
                .unwrap()["text"],
            "fn main() {}\n"
        );
    }

    #[test]
    fn closing_workspace_rekeys_documents_and_clears_the_root() {
        let directory = TempDir::new("workspace-close");
        let project = directory.join("projeto");
        let nested = project.join("src");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);
        app.open_document_in_new_tab(&file).unwrap();

        let uri = uri_for(&file);
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        app.close_workspace();

        wait_for(|| {
            environment
                .all_snapshots()
                .iter()
                .any(|entry| entry["method"] == "textDocument/didClose" && entry["uri"] == uri)
        });
        wait_for(|| {
            environment
                .all_snapshots()
                .iter()
                .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
                .count()
                >= 2
        });
        assert!(app.workspace().is_none());
        assert_eq!(environment.snapshot_files().len(), 2);
    }

    #[test]
    fn shutdown_closes_documents_and_stops_servers_idempotently() {
        let directory = TempDir::new("shutdown");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let first = project.join("a.rs");
        let second = project.join("b.rs");
        let python = project.join("tool.py");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {}\n").unwrap();
        fs::write(&python, "print('ok')\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp(Some(&project), &environment);
        app.open_document_in_new_tab(&first).unwrap();
        app.open_document_in_new_tab(&second).unwrap();
        app.open_document_in_new_tab(&python).unwrap();

        let first_uri = uri_for(&first);
        let second_uri = uri_for(&second);
        let python_uri = uri_for(&python);
        wait_for(|| {
            environment
                .last_snapshot(&second_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
                && environment
                    .last_snapshot(&python_uri)
                    .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        // Fecha uma aba antes do encerramento geral; as demais abas e o
        // servidor compartilhado continuam ativos até o shutdown.
        app.close_tab(0);
        wait_for(|| {
            environment
                .last_snapshot(&first_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didClose")
        });

        app.shutdown_lsp();
        app.shutdown_lsp();

        wait_for(|| {
            [&first_uri, &second_uri, &python_uri].iter().all(|uri| {
                environment
                    .last_snapshot(uri)
                    .is_some_and(|entry| entry["method"] == "textDocument/didClose")
            })
        });
        let markers = environment.all_markers();
        assert_eq!(
            markers
                .iter()
                .filter(|marker| *marker == "shutdown")
                .count(),
            2
        );
        assert_eq!(markers.iter().filter(|marker| *marker == "exit").count(), 2);
        assert_eq!(environment.snapshot_files().len(), 2);
    }

    #[test]
    fn restored_recovered_documents_are_opened_exactly_once() {
        let directory = TempDir::new("restore");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "conteúdo do disco\n").unwrap();

        let paths = AppPaths {
            config_file: directory.join("config.toml"),
            themes_dir: directory.join("themes"),
            root: directory.path.clone(),
            session_file: directory.join("session.toml"),
            recent_file: directory.join("recent.toml"),
            recovery_dir: directory.join("recovery"),
        };

        let mut recovery = RecoveryManager::new(&paths.recovery_dir);
        recovery
            .save(&RecoveryRecord {
                version: RECOVERY_VERSION,
                id: RecoveryManager::id_for_path(&file),
                path: Some(file.clone()),
                name: "main.rs".to_owned(),
                text: "conteúdo recuperado\n".to_owned(),
                saved_at: now_seconds(),
            })
            .unwrap();

        let mut app = App::with_paths(80, 24, paths);
        app.mark_rendered();
        assert!(app.has_open_documents());

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let service = LspService::with_provider(environment.provider()).expect("iniciar serviço");
        app.set_sync_sink(Box::new(LspSyncSink::new(service)));

        let uri = uri_for(&file);
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        let opens: Vec<Value> = environment
            .all_snapshots()
            .into_iter()
            .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
            .collect();
        assert_eq!(opens.len(), 1, "a restauração não pode duplicar didOpen");
        assert_eq!(opens[0]["text"], "conteúdo recuperado\n");
        assert_eq!(fs::read_to_string(&file).unwrap(), "conteúdo do disco\n");
    }

    #[test]
    fn unexpected_server_exit_invalidates_and_rebuilds_open_documents() {
        let directory = TempDir::new("exit-rebuild");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_mode(Some(&project), &environment, "exit-on-change");
        app.open_document_in_new_tab(&file).unwrap();
        press(&mut app, KeyCode::Esc);

        let uri = uri_for(&file);
        wait_for(|| {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        type_text(&mut app, "x");
        app.on_idle();

        // O primeiro processo morre ao receber didChange. O sink precisa
        // remover o estado inconsistente e reabrir o documento em um processo
        // novo, com o conteúdo atual.
        wait_for_app(&mut app, || {
            environment
                .all_snapshots()
                .iter()
                .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
                .count()
                >= 2
        });
        assert!(environment.snapshot_files().len() >= 2);

        let reopened = environment
            .all_snapshots()
            .into_iter()
            .rfind(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
            .unwrap();
        assert_eq!(reopened["text"], "xfn main() {}\n");
        assert_eq!(
            environment
                .all_snapshots()
                .iter()
                .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
                .count(),
            2,
            "uma sessão reconstruída deve abrir o documento uma única vez"
        );

        // Uma edição posterior precisa chegar ao servidor reconstruído.
        type_text(&mut app, "y");
        app.on_idle();
        wait_for_app(&mut app, || {
            environment.all_snapshots().iter().any(|entry| {
                entry["method"] == "textDocument/didChange"
                    && entry["uri"] == uri
                    && entry["text"] == "xyfn main() {}\n"
            })
        });
    }

    #[test]
    fn failed_initialization_is_retried_and_reopens_from_current_text_once() {
        let directory = TempDir::new("init-retry");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "estado atual\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_modes(
            Some(&project),
            &environment,
            vec!["fail-init", "sync-incremental"],
        );
        app.open_document_in_new_tab(&file).unwrap();
        let uri = uri_for(&file);

        wait_for_app(&mut app, || {
            environment
                .last_snapshot(&uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        let opens: Vec<Value> = environment
            .all_snapshots()
            .into_iter()
            .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
            .collect();
        assert_eq!(environment.start_count(), 2);
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0]["text"], "estado atual\n");
        app.shutdown_lsp();
    }

    #[test]
    fn edits_during_outage_rebuild_multiple_documents_from_latest_snapshots() {
        let directory = TempDir::new("multi-outage");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let first = project.join("a.rs");
        let second = project.join("b.rs");
        fs::write(&first, "fn a() {}\n").unwrap();
        fs::write(&second, "fn b() {}\n").unwrap();

        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_mode(Some(&project), &environment, "exit-on-change");
        app.open_document_in_new_tab(&first).unwrap();
        app.open_document_in_new_tab(&second).unwrap();
        press(&mut app, KeyCode::Esc);
        let first_uri = uri_for(&first);
        let second_uri = uri_for(&second);
        wait_for(|| {
            environment
                .last_snapshot(&second_uri)
                .is_some_and(|entry| entry["method"] == "textDocument/didOpen")
        });

        // O fake termina no primeiro didChange; a segunda edição ocorre antes
        // de o sink drenar o evento de queda e deve aparecer no didOpen novo.
        type_text(&mut app, "xy");
        app.on_idle();
        wait_for_app(&mut app, || {
            let snapshots = environment.all_snapshots();
            [(&first_uri, "fn a() {}\n"), (&second_uri, "xyfn b() {}\n")]
                .iter()
                .all(|(uri, text)| {
                    snapshots.iter().any(|entry| {
                        entry["method"] == "textDocument/didOpen"
                            && entry["uri"].as_str() == Some(uri.as_str())
                            && entry["text"] == *text
                    })
                })
        });

        for uri in [&first_uri, &second_uri] {
            assert_eq!(
                environment
                    .all_snapshots()
                    .iter()
                    .filter(|entry| {
                        entry["method"] == "textDocument/didOpen"
                            && entry["uri"].as_str() == Some(uri.as_str())
                    })
                    .count(),
                2
            );
        }
        app.shutdown_lsp();
    }

    #[test]
    fn missing_executable_has_bounded_retries_and_does_not_block_editor() {
        let directory = TempDir::new("missing-server");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "texto\n").unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let executable = directory.join("servidor-que-nao-existe");

        let service_workspace = fs::canonicalize(&project).unwrap();
        let service = LspService::with_provider(move |_workspace, language| {
            observed.fetch_add(1, Ordering::SeqCst);
            Some(ServerConfig::new(language, &executable))
        })
        .unwrap();
        let mut sink = LspSyncSink::new(service);
        sink.set_workspace(Some(&service_workspace));
        let path = fs::canonicalize(&file).unwrap();
        sink.handle(SyncEvent::Open {
            path: path.clone(),
            language_id: "rust".to_owned(),
            text: "texto\n".to_owned(),
        });

        let mut notices = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::SeqCst) < 4 {
            notices.extend(sink.drain_notices());
            assert!(
                Instant::now() < deadline,
                "retries automáticos não concluíram"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        notices.extend(sink.drain_notices());
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("servidor-que-nao-existe"))
        );
        assert_eq!(sink.routes.len(), 1, "a falha não fecha o documento na TUI");
        let deadline = Instant::now() + Duration::from_millis(700);
        while Instant::now() < deadline {
            notices.extend(sink.drain_notices());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 4, "1 início + 3 retries");

        // Nova edição explícita permite um segundo ciclo limitado; não há
        // polling nem tentativas contínuas enquanto o usuário não interage.
        sink.handle(SyncEvent::Change {
            path,
            language_id: "rust".to_owned(),
            changes: ChangeSet::Edits(vec![nexus_lsp::sync::TextChange::new(0, 0, "y")]),
            text: "ytexto\n".to_owned(),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::SeqCst) < 7 {
            notices.extend(sink.drain_notices());
            assert!(
                Instant::now() < deadline,
                "novo ciclo após edição não iniciou"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            notices.extend(sink.drain_notices());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 7);
        sink.shutdown();
    }

    #[test]
    fn closing_last_document_cancels_pending_recovery() {
        let directory = TempDir::new("close-during-recovery");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "original\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut sink = sink_with_modes(&environment, vec!["exit-on-change"]);
        let uri = crash_sink_document(&mut sink, &environment, &file, &project);

        let deadline = Instant::now() + Duration::from_secs(2);
        while sink.recovery.is_empty() && Instant::now() < deadline {
            let _ = sink.drain_notices();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!sink.recovery.is_empty(), "queda precisa ser observada");
        sink.handle(SyncEvent::Close {
            path: fs::canonicalize(&file).unwrap(),
        });

        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            let _ = sink.drain_notices();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(environment.start_count(), 1);
        assert_eq!(
            environment
                .all_snapshots()
                .iter()
                .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
                .count(),
            1
        );
        sink.shutdown();
    }

    #[test]
    fn workspace_change_during_recovery_uses_only_current_routes() {
        let directory = TempDir::new("workspace-during-recovery");
        let old_workspace = directory.join("antigo");
        let new_workspace = directory.join("novo");
        fs::create_dir_all(&old_workspace).unwrap();
        fs::create_dir_all(&new_workspace).unwrap();
        let file = old_workspace.join("main.rs");
        fs::write(&file, "original\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut sink = sink_with_modes(&environment, vec!["exit-on-change", "sync-incremental"]);
        let uri = crash_sink_document(&mut sink, &environment, &file, &old_workspace);

        let deadline = Instant::now() + Duration::from_secs(2);
        while sink.recovery.is_empty() && Instant::now() < deadline {
            let _ = sink.drain_notices();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!sink.recovery.is_empty());
        let path = fs::canonicalize(&file).unwrap();
        sink.handle(SyncEvent::Close { path: path.clone() });
        sink.set_workspace(Some(&new_workspace));
        sink.handle(SyncEvent::Open {
            path,
            language_id: "rust".to_owned(),
            text: "novo estado\n".to_owned(),
        });

        wait_for_sink(&mut sink, || {
            environment.last_snapshot(&uri).is_some_and(|entry| {
                entry["method"] == "textDocument/didOpen" && entry["text"] == "novo estado\n"
            })
        });
        assert_eq!(environment.start_count(), 2);
        assert_eq!(
            environment
                .all_snapshots()
                .iter()
                .filter(|entry| entry["method"] == "textDocument/didOpen" && entry["uri"] == uri)
                .count(),
            2
        );
        sink.shutdown();
    }

    #[test]
    fn shutdown_during_recovery_cancels_timers_and_returns_promptly() {
        let directory = TempDir::new("shutdown-during-recovery");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "original\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut sink = sink_with_modes(&environment, vec!["exit-on-change"]);
        let _ = crash_sink_document(&mut sink, &environment, &file, &project);

        let deadline = Instant::now() + Duration::from_secs(2);
        while sink.recovery.is_empty() && Instant::now() < deadline {
            let _ = sink.drain_notices();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!sink.recovery.is_empty());
        let started = Instant::now();
        sink.shutdown();
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(environment.start_count(), 1);
    }

    #[test]
    fn app_stores_published_diagnostics_and_unversioned_results_become_stale_on_edit() {
        let directory = TempDir::new("diagnostic-storage");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "x\n").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut app = app_with_lsp_mode(
            Some(&project),
            &environment,
            "publish-diagnostics-unversioned",
        );
        app.open_document_in_new_tab(&file).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.diagnostics().is_empty() && Instant::now() < deadline {
            app.on_idle();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.diagnostics().len(), 1);
        assert_eq!(
            app.diagnostics()[0].publication.diagnostics[0].message,
            "diagnóstico simulado"
        );
        assert_eq!(
            app.diagnostics()[0].freshness,
            DiagnosticFreshness::Unverified
        );

        app.update(crate::event::Event::Key(
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('b'),
                ratatui::crossterm::event::KeyModifiers::CONTROL,
            ),
        ));
        app.update(crate::event::Event::Key(
            ratatui::crossterm::event::KeyEvent::new(
                ratatui::crossterm::event::KeyCode::Char('y'),
                ratatui::crossterm::event::KeyModifiers::NONE,
            ),
        ));
        app.on_idle();
        assert_eq!(
            app.diagnostics()[0].freshness,
            DiagnosticFreshness::PotentiallyStale
        );
        app.shutdown_lsp();
    }

    #[test]
    fn diagnostic_store_replaces_clears_and_isolates_server_workspace_version_and_generation() {
        use serde_json::json;
        let directory = TempDir::new("diagnostic-store-isolation");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("main.rs");
        fs::write(&file, "x").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut sink = sink_with_modes(&environment, vec!["sync-incremental"]);
        let path = fs::canonicalize(&file).unwrap();
        let uri = uri_for(&path);
        let workspace = project.to_string_lossy().into_owned();
        sink.routes.insert(
            path.clone(),
            DocumentRoute {
                workspace: workspace.clone(),
                language_id: "rust".into(),
                text: "x".into(),
            },
        );
        sink.sessions.insert((workspace.clone(), "rust".into()), 10);
        sink.sessions
            .insert((workspace.clone(), "python".into()), 20);
        let publication = |version, message: &str| {
            serde_json::from_value(json!({"uri":uri,"version":version,"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":message}]})).unwrap()
        };
        sink.apply_publication(
            workspace.clone(),
            "rust".into(),
            10,
            Some(3),
            publication(2, "old"),
        );
        assert!(
            sink.diagnostics.is_empty(),
            "versão inferior deve ser rejeitada"
        );
        sink.apply_publication(
            workspace.clone(),
            "rust".into(),
            10,
            Some(3),
            publication(3, "current"),
        );
        sink.apply_publication(
            workspace.clone(),
            "rust".into(),
            10,
            Some(3),
            publication(3, "replacement"),
        );
        assert_eq!(
            sink.diagnostics
                .values()
                .find(|value| value.key.language_id == "rust")
                .unwrap()
                .publication
                .diagnostics[0]
                .message,
            "replacement"
        );
        sink.apply_publication(
            workspace.clone(),
            "python".into(),
            20,
            Some(3),
            publication(3, "other server"),
        );
        assert_eq!(
            sink.diagnostics.len(),
            2,
            "servidores distintos mantêm conjuntos separados"
        );
        sink.apply_publication(
            "outro-workspace".into(),
            "rust".into(),
            10,
            Some(3),
            publication(3, "wrong workspace"),
        );
        assert_eq!(
            sink.diagnostics.len(),
            2,
            "workspace não pode compartilhar estado"
        );
        sink.apply_publication(
            workspace.clone(),
            "rust".into(),
            99,
            Some(3),
            publication(3, "old session"),
        );
        assert_eq!(
            sink.diagnostics.len(),
            2,
            "sessão antiga não pode substituir a atual"
        );
        sink.apply_publication(
            workspace.clone(),
            "rust".into(),
            10,
            Some(3),
            serde_json::from_value(json!({"uri":uri,"diagnostics":[]})).unwrap(),
        );
        assert_eq!(
            sink.diagnostics.len(),
            1,
            "publicação vazia limpa somente seu conjunto"
        );
        sink.handle(SyncEvent::Close { path });
        assert!(
            sink.diagnostics.is_empty(),
            "fechar documento limpa todos os servidores associados à URI"
        );
        sink.shutdown();
    }

    #[test]
    fn unopened_workspace_file_can_receive_diagnostics_for_problem_navigation() {
        let directory = TempDir::new("unopened-workspace-diagnostic");
        let project = directory.join("project");
        fs::create_dir_all(&project).unwrap();
        let file = project.join("closed.rs");
        fs::write(&file, "let x = 1;").unwrap();
        let root = fs::canonicalize(&project).unwrap();
        let path = fs::canonicalize(&file).unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut sink = sink_with_modes(&environment, vec!["sync-incremental"]);
        sink.set_workspace(Some(&root));
        assert!(sink.routes.is_empty(), "arquivo propositalmente não aberto");
        let uri = uri_for(&path);
        let publication = serde_json::from_value(serde_json::json!({
            "uri": uri,
            "diagnostics": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                "message": "diagnóstico fora da aba"
            }]
        }))
        .unwrap();
        sink.apply_publication(
            root.to_string_lossy().into_owned(),
            "rust".into(),
            1,
            None,
            publication,
        );
        assert_eq!(sink.diagnostics.len(), 1);
        assert_eq!(
            sink.diagnostics
                .values()
                .next()
                .unwrap()
                .publication
                .diagnostics[0]
                .message,
            "diagnóstico fora da aba"
        );
        sink.service.shutdown();
    }

    #[test]
    fn renamed_document_drops_diagnostics_from_its_old_uri() {
        use serde_json::json;
        let directory = TempDir::new("diagnostic-rename");
        let project = directory.join("projeto");
        fs::create_dir_all(&project).unwrap();
        let old_path = project.join("old.rs");
        let new_path = project.join("new.rs");
        fs::write(&old_path, "x").unwrap();
        fs::write(&new_path, "x").unwrap();
        let environment = FakeEnvironment::new(directory.join("lsp"));
        let mut sink = sink_with_modes(&environment, vec!["sync-incremental"]);
        let old_path = fs::canonicalize(old_path).unwrap();
        let new_path = fs::canonicalize(new_path).unwrap();
        let workspace = project.to_string_lossy().into_owned();
        let old_uri = uri_for(&old_path);
        let new_uri = uri_for(&new_path);
        sink.routes.insert(
            old_path.clone(),
            DocumentRoute {
                workspace: workspace.clone(),
                language_id: "rust".into(),
                text: "x".into(),
            },
        );
        sink.sessions.insert((workspace.clone(), "rust".into()), 1);
        let published = |uri: &str| {
            serde_json::from_value(json!({"uri":uri,"version":1,"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"problem"}]})).unwrap()
        };
        sink.apply_publication(
            workspace.clone(),
            "rust".into(),
            1,
            Some(1),
            published(&old_uri),
        );
        sink.handle(SyncEvent::Close { path: old_path });
        sink.handle(SyncEvent::Open {
            path: new_path,
            language_id: "rust".into(),
            text: "x".into(),
        });
        assert!(sink.diagnostics.is_empty());
        assert_ne!(old_uri, new_uri);
        sink.shutdown();
    }
}
