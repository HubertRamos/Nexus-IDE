//! Fachada síncrona sobre o núcleo assíncrono do LSP.
//!
//! A TUI é síncrona e não depende de Tokio. Este módulo cria uma thread
//! dedicada com um runtime `current_thread` e expõe uma API de envio não
//! bloqueante: a interface apenas enfileira comandos e continua respondendo.
//!
//! # Fluxo
//!
//! 1. A TUI chama [`LspService::open`], [`LspService::change`], etc.
//! 2. O comando entra em um canal ilimitado do Tokio.
//! 3. A thread do serviço processa comandos **em ordem** e usa um
//!    [`ServerPool`] para compartilhar processos por `(workspace, linguagem)`.
//!    Respostas de navegação aguardam em tarefas Tokio independentes, então o
//!    loop continua processando sincronização e demais comandos.
//! 4. Eventos, incluindo resultados tipados de navegação, voltam por um canal
//!    `std` que a TUI pode drenar sem bloquear.
//!
//! O serviço só inicia um processo no primeiro `Open` de uma linguagem que
//! tenha configuração. Nenhum servidor é criado apenas por abrir o editor.
//!
//! # Canais ilimitados
//!
//! O canal de comandos é ilimitado para que a TUI nunca bloqueie em uma
//! operação de arquivo ou de processo. O risco é crescimento de memória se a
//! interface produzir comandos mais rápido do que o serviço consegue drená-los.
//! Na prática os comandos são pequenos e o processamento é síncrono; uma etapa
//! futura pode introduzir coalescência ou um limite com descarte explícito,
//! sem mudar a API da interface.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::mpsc::{self as tokio_mpsc, UnboundedReceiver, UnboundedSender};

use crate::languages::{PYTHON_LANGUAGE_ID, RUST_LANGUAGE_ID, pyright, rust_analyzer};
use crate::manager::{
    NavigationCancellation, ServerConfig, ServerError, ServerEvent, ServerStatus,
};
use crate::pool::{ServerKey, ServerPool};
use crate::protocol::{
    NavigationMethod, NavigationResult, Position, PublishDiagnosticsParams, TextDocumentIdentifier,
};
use crate::sync::TextChange;
use crate::uri::path_to_uri;

/// Intervalo de drenagem de eventos dos servidores quando não há comandos.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Identidade estável de uma operação de navegação solicitada pelo consumidor.
/// O identificador é escopado à instância de [`LspService`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NavigationOperationId(pub u64);

/// Contexto original necessário para o consumidor validar uma resposta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationRequestContext {
    pub operation_id: NavigationOperationId,
    pub workspace: String,
    pub language_id: String,
    pub session_id: Option<i64>,
    pub method: NavigationMethod,
    pub uri: String,
    pub document_version: Option<i64>,
    pub position: Position,
}

/// Resultado terminal de uma operação de navegação.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationOutcome {
    Result(NavigationResult),
    Unsupported,
    TimedOut { duration: Duration },
    Cancelled,
    SessionEnded,
    Failed { failure: NavigationFailure },
}

/// Categoria de falha preservada para a camada consumidora.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationFailure {
    Transport(String),
    Protocol(String),
    Server(String),
    Internal(String),
}

/// Conclusão tipada pronta para consumo por uma interface ou cliente GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationCompletion {
    pub context: NavigationRequestContext,
    pub outcome: NavigationOutcome,
}

struct ActiveNavigation {
    context: NavigationRequestContext,
    cancellation: NavigationCancellation,
}

struct NavigationRequestInput {
    operation_id: NavigationOperationId,
    workspace: String,
    language_id: String,
    method: NavigationMethod,
    uri: String,
    position: Position,
}

/// Operação que originou uma falha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOperation {
    Start,
    Open,
    Change,
    Resync,
    Save,
    Close,
    Shutdown,
}

impl std::fmt::Display for ServiceOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Start => "iniciar",
            Self::Open => "abrir",
            Self::Change => "alterar",
            Self::Resync => "ressincronizar",
            Self::Save => "salvar",
            Self::Close => "fechar",
            Self::Shutdown => "encerrar",
        };
        formatter.write_str(name)
    }
}

/// Eventos que o serviço devolve à interface.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceEvent {
    /// Diagnósticos publicados por uma instância específica do servidor.
    DiagnosticsPublished {
        workspace: String,
        language_id: String,
        session_id: i64,
        current_document_version: Option<i64>,
        publication: PublishDiagnosticsParams,
    },
    /// Resposta, timeout, cancelamento ou invalidação de uma navegação.
    NavigationCompleted {
        context: NavigationRequestContext,
        outcome: NavigationOutcome,
    },
    /// Um processo foi iniciado para o par `(workspace, linguagem)`.
    ServerStarted {
        workspace: String,
        language_id: String,
        session_id: i64,
    },
    /// Uma operação falhou. O documento pode continuar aberto na interface.
    Failure {
        operation: ServiceOperation,
        uri: Option<String>,
        message: String,
    },
    /// O processo terminou ou perdeu o transporte.
    ServerExited {
        workspace: String,
        language_id: String,
        message: String,
    },
    /// A thread do serviço terminou.
    Stopped,
}

/// Comandos enviados pela interface para a thread do serviço.
enum Command {
    Open {
        workspace: String,
        language_id: String,
        uri: String,
        text: String,
    },
    Change {
        workspace: String,
        language_id: String,
        uri: String,
        changes: Vec<TextChange>,
        text: String,
    },
    Resync {
        workspace: String,
        language_id: String,
        uri: String,
        text: String,
    },
    Save {
        workspace: String,
        language_id: String,
        uri: String,
        text: String,
    },
    Close {
        workspace: String,
        language_id: String,
        uri: String,
    },
    Retry {
        workspace: String,
        language_id: String,
    },
    Navigate(NavigationRequestInput),
    CancelNavigation(NavigationOperationId),
    Shutdown,
}

/// Configuração de produção: apenas linguagens com preset conhecido.
pub fn production_config(_workspace: &str, language_id: &str) -> Option<ServerConfig> {
    match language_id {
        RUST_LANGUAGE_ID => Some(rust_analyzer()),
        PYTHON_LANGUAGE_ID => Some(pyright()),
        _ => None,
    }
}

/// Serviço LSP síncrono para a interface.
///
/// O envio de comandos nunca bloqueia. O recebimento de eventos é opcional e
/// feito por [`LspService::try_recv`] ou [`LspService::drain`].
pub struct LspService {
    commands: UnboundedSender<Command>,
    events: Receiver<ServiceEvent>,
    thread: Option<JoinHandle<()>>,
    next_navigation_id: AtomicU64,
}

impl LspService {
    /// Inicia o serviço com os presets de produção.
    pub fn start() -> io::Result<Self> {
        Self::with_provider(production_config)
    }

    /// Inicia o serviço com um provedor de configuração injetável.
    ///
    /// O provedor recebe o workspace e a linguagem e devolve `None` quando não
    /// há servidor para aquela combinação. Ele é consultado apenas na primeira
    /// abertura de cada par `(workspace, linguagem)`.
    pub fn with_provider<P>(provider: P) -> io::Result<Self>
    where
        P: Fn(&str, &str) -> Option<ServerConfig> + Send + 'static,
    {
        let (command_sender, command_receiver) = tokio_mpsc::unbounded_channel();
        let (event_sender, event_receiver) = mpsc::channel();

        let thread = thread::Builder::new()
            .name("nexus-lsp".to_owned())
            .spawn(move || run_service(command_receiver, event_sender, provider))?;

        Ok(Self {
            commands: command_sender,
            events: event_receiver,
            thread: Some(thread),
            next_navigation_id: AtomicU64::new(1),
        })
    }

    /// Enfileira a abertura de um documento.
    pub fn open(
        &self,
        workspace: impl Into<String>,
        language_id: impl Into<String>,
        uri: impl Into<String>,
        text: impl Into<String>,
    ) {
        let _ = self.commands.send(Command::Open {
            workspace: workspace.into(),
            language_id: language_id.into(),
            uri: uri.into(),
            text: text.into(),
        });
    }

    /// Enfileira as alterações de uma transação já aplicada.
    pub fn change(
        &self,
        workspace: impl Into<String>,
        language_id: impl Into<String>,
        uri: impl Into<String>,
        changes: Vec<TextChange>,
        text: impl Into<String>,
    ) {
        let _ = self.commands.send(Command::Change {
            workspace: workspace.into(),
            language_id: language_id.into(),
            uri: uri.into(),
            changes,
            text: text.into(),
        });
    }

    /// Enfileira uma ressincronização integral (Undo/Redo ou recarga).
    pub fn resync(
        &self,
        workspace: impl Into<String>,
        language_id: impl Into<String>,
        uri: impl Into<String>,
        text: impl Into<String>,
    ) {
        let _ = self.commands.send(Command::Resync {
            workspace: workspace.into(),
            language_id: language_id.into(),
            uri: uri.into(),
            text: text.into(),
        });
    }

    /// Enfileira o salvamento de um documento.
    pub fn save(
        &self,
        workspace: impl Into<String>,
        language_id: impl Into<String>,
        uri: impl Into<String>,
        text: impl Into<String>,
    ) {
        let _ = self.commands.send(Command::Save {
            workspace: workspace.into(),
            language_id: language_id.into(),
            uri: uri.into(),
            text: text.into(),
        });
    }

    /// Enfileira o fechamento de um documento.
    pub fn close(
        &self,
        workspace: impl Into<String>,
        language_id: impl Into<String>,
        uri: impl Into<String>,
    ) {
        let _ = self.commands.send(Command::Close {
            workspace: workspace.into(),
            language_id: language_id.into(),
            uri: uri.into(),
        });
    }

    /// Permite uma nova tentativa depois de uma falha de inicialização.
    /// A tentativa continua sendo executada na thread do serviço.
    pub fn retry(&self, workspace: impl Into<String>, language_id: impl Into<String>) {
        let _ = self.commands.send(Command::Retry {
            workspace: workspace.into(),
            language_id: language_id.into(),
        });
    }

    /// Solicita uma navegação sem esperar a resposta do servidor.
    pub fn request_navigation(
        &self,
        workspace: impl Into<String>,
        language_id: impl Into<String>,
        method: NavigationMethod,
        uri: impl Into<String>,
        position: Position,
    ) -> NavigationOperationId {
        let operation_id =
            NavigationOperationId(self.next_navigation_id.fetch_add(1, Ordering::Relaxed));
        let _ = self
            .commands
            .send(Command::Navigate(NavigationRequestInput {
                operation_id,
                workspace: workspace.into(),
                language_id: language_id.into(),
                method,
                uri: uri.into(),
                position,
            }));
        operation_id
    }

    /// Cancela uma operação ainda ativa. A resposta terminal chega como evento.
    /// Se o servidor já respondeu, o resultado original permanece válido.
    pub fn cancel_navigation(&self, operation_id: NavigationOperationId) {
        let _ = self.commands.send(Command::CancelNavigation(operation_id));
    }

    /// Obtém um evento sem aguardar.
    pub fn try_recv(&self) -> Option<ServiceEvent> {
        self.events.try_recv().ok()
    }

    /// Drena todos os eventos disponíveis.
    pub fn drain(&self) -> Vec<ServiceEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.try_recv() {
            events.push(event);
        }
        events
    }

    /// Encerra a thread e todos os servidores.
    ///
    /// É seguro chamar mais de uma vez. O `Drop` usa este mesmo caminho.
    pub fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };

        let _ = self.commands.send(Command::Shutdown);
        let _ = thread.join();
    }
}

impl Drop for LspService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_service<P>(
    mut commands: UnboundedReceiver<Command>,
    events: Sender<ServiceEvent>,
    provider: P,
) where
    P: Fn(&str, &str) -> Option<ServerConfig>,
{
    let runtime = match Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(ServiceEvent::Failure {
                operation: ServiceOperation::Start,
                uri: None,
                message: format!("não foi possível criar o runtime LSP: {error}"),
            });
            return;
        }
    };

    runtime.block_on(async move {
        let mut pool = ServerPool::new();
        // Uma falha de spawn/initialize não deve ser repetida por cada `Open`
        // enfileirado. Apenas `Retry` (controlado pelo sink) libera a chave.
        let mut unavailable = BTreeSet::new();
        let mut active_navigation = BTreeMap::new();
        let (completion_sender, mut completion_receiver) = tokio_mpsc::unbounded_channel();

        loop {
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(Command::Shutdown) | None => break,
                        Some(command) => {
                            handle_command(
                                command,
                                &mut pool,
                                &mut unavailable,
                                &mut active_navigation,
                                &completion_sender,
                                &provider,
                                &events,
                            ).await;
                            drain_server_events(&mut pool, &events, &mut active_navigation);
                        }
                    }
                }
                completion = completion_receiver.recv() => {
                    if let Some((operation_id, context, outcome)) = completion
                        && active_navigation.remove(&operation_id).is_some()
                    {
                        let _ = events.send(ServiceEvent::NavigationCompleted { context, outcome });
                    }
                }
                _ = tokio::time::sleep(EVENT_POLL_INTERVAL) => {
                    drain_server_events(&mut pool, &events, &mut active_navigation);
                }
            }
        }

        for (_, navigation) in std::mem::take(&mut active_navigation) {
            navigation.cancellation.cancel();
            let _ = events.send(ServiceEvent::NavigationCompleted {
                context: navigation.context,
                outcome: NavigationOutcome::Cancelled,
            });
        }
        drain_server_events(&mut pool, &events, &mut active_navigation);
        for (key, error) in pool.shutdown_all().await {
            let _ = events.send(ServiceEvent::Failure {
                operation: ServiceOperation::Shutdown,
                uri: None,
                message: format!(
                    "falha ao encerrar {} ({}): {error}",
                    key.workspace, key.language_id
                ),
            });
        }
        let _ = events.send(ServiceEvent::Stopped);
    });
}

async fn handle_command<P>(
    command: Command,
    pool: &mut ServerPool,
    unavailable: &mut BTreeSet<ServerKey>,
    active_navigation: &mut BTreeMap<NavigationOperationId, ActiveNavigation>,
    completion_sender: &tokio_mpsc::UnboundedSender<(
        NavigationOperationId,
        NavigationRequestContext,
        NavigationOutcome,
    )>,
    provider: &P,
    events: &Sender<ServiceEvent>,
) where
    P: Fn(&str, &str) -> Option<ServerConfig>,
{
    match command {
        Command::Open {
            workspace,
            language_id,
            uri,
            text,
        } => {
            let key = ServerKey::new(&workspace, &language_id);

            if unavailable.contains(&key) {
                return;
            }

            if !pool.contains(&key) {
                let Some(mut config) = provider(&workspace, &language_id) else {
                    // Linguagens sem preset são ignoradas sem iniciar processo.
                    return;
                };

                if config.root_uri.is_none() {
                    config.root_uri = Some(path_to_uri(Path::new(&workspace)));
                }

                if let Err(error) = pool.ensure(key.clone(), config).await {
                    unavailable.insert(key);
                    let _ = events.send(ServiceEvent::Failure {
                        operation: ServiceOperation::Start,
                        uri: Some(uri),
                        message: error.to_string(),
                    });
                    return;
                }

                if let Some(server) = pool.get(&key) {
                    let _ = events.send(ServiceEvent::ServerStarted {
                        workspace,
                        language_id: language_id.clone(),
                        session_id: server.session_id(),
                    });
                }
            }

            if let Some(server) = pool.get(&key)
                && let Err(error) = server.open_document(uri.clone(), language_id, text)
            {
                report_sync_failure(pool, &key, events, ServiceOperation::Open, uri, error);
            }
        }
        Command::Change {
            workspace,
            language_id,
            uri,
            changes,
            text,
        } => {
            let key = ServerKey::new(&workspace, &language_id);
            if let Some(server) = pool.get(&key)
                && let Err(error) = server.change_document(&uri, &changes, &text)
            {
                report_sync_failure(pool, &key, events, ServiceOperation::Change, uri, error);
            }
        }
        Command::Resync {
            workspace,
            language_id,
            uri,
            text,
        } => {
            let key = ServerKey::new(&workspace, &language_id);
            if let Some(server) = pool.get(&key)
                && let Err(error) = server.resync_document(&uri, &text)
            {
                report_sync_failure(pool, &key, events, ServiceOperation::Resync, uri, error);
            }
        }
        Command::Save {
            workspace,
            language_id,
            uri,
            text,
        } => {
            let key = ServerKey::new(&workspace, &language_id);
            if let Some(server) = pool.get(&key)
                && let Err(error) = server.save_document(&uri, &text)
            {
                report_sync_failure(pool, &key, events, ServiceOperation::Save, uri, error);
            }
        }
        Command::Close {
            workspace,
            language_id,
            uri,
        } => {
            cancel_navigation_matching(
                active_navigation,
                events,
                |context| {
                    context.workspace == workspace
                        && context.language_id == language_id
                        && context.uri == uri
                },
                NavigationOutcome::Cancelled,
            );
            let key = ServerKey::new(&workspace, &language_id);
            if let Some(server) = pool.get(&key)
                && let Err(error) = server.close_document(&uri)
            {
                report_sync_failure(pool, &key, events, ServiceOperation::Close, uri, error);
            }
        }
        Command::Retry {
            workspace,
            language_id,
        } => {
            unavailable.remove(&ServerKey::new(workspace, language_id));
        }
        Command::Navigate(request) => {
            start_navigation(request, pool, active_navigation, completion_sender, events)
        }
        Command::CancelNavigation(operation_id) => {
            let cancelled = active_navigation
                .get(&operation_id)
                .is_some_and(|navigation| navigation.cancellation.cancel());
            if cancelled && let Some(navigation) = active_navigation.remove(&operation_id) {
                let _ = events.send(ServiceEvent::NavigationCompleted {
                    context: navigation.context,
                    outcome: NavigationOutcome::Cancelled,
                });
            }
        }
        Command::Shutdown => {}
    }
}

fn start_navigation(
    request: NavigationRequestInput,
    pool: &ServerPool,
    active_navigation: &mut BTreeMap<NavigationOperationId, ActiveNavigation>,
    completion_sender: &tokio_mpsc::UnboundedSender<(
        NavigationOperationId,
        NavigationRequestContext,
        NavigationOutcome,
    )>,
    events: &Sender<ServiceEvent>,
) {
    let NavigationRequestInput {
        operation_id,
        workspace,
        language_id,
        method,
        uri,
        position,
    } = request;
    let key = ServerKey::new(&workspace, &language_id);
    let Some(server) = pool.get(&key) else {
        let _ = events.send(ServiceEvent::NavigationCompleted {
            context: NavigationRequestContext {
                operation_id,
                workspace,
                language_id,
                session_id: None,
                method,
                uri,
                document_version: None,
                position,
            },
            outcome: NavigationOutcome::SessionEnded,
        });
        return;
    };

    let context = NavigationRequestContext {
        operation_id,
        workspace,
        language_id,
        session_id: Some(server.session_id()),
        method,
        uri: uri.clone(),
        document_version: server.document_version(&uri),
        position,
    };
    if !server.capabilities().supports_navigation(method) {
        let _ = events.send(ServiceEvent::NavigationCompleted {
            context,
            outcome: NavigationOutcome::Unsupported,
        });
        return;
    }

    let params = method.params(TextDocumentIdentifier { uri }, position);
    let params = match serde_json::to_value(params) {
        Ok(params) => params,
        Err(error) => {
            let _ = events.send(ServiceEvent::NavigationCompleted {
                context,
                outcome: NavigationOutcome::Failed {
                    failure: NavigationFailure::Protocol(format!(
                        "parâmetros de navegação inválidos: {error}"
                    )),
                },
            });
            return;
        }
    };
    let pending = match server.begin_navigation_request(method.lsp_method(), Some(params)) {
        Ok(pending) => pending,
        Err(error) => {
            let outcome = navigation_error_outcome(error);
            let _ = events.send(ServiceEvent::NavigationCompleted { context, outcome });
            return;
        }
    };
    let cancellation = pending.cancellation_handle();
    let timeout = server.navigation_timeout();
    active_navigation.insert(
        operation_id,
        ActiveNavigation {
            context: context.clone(),
            cancellation,
        },
    );
    let completion_sender = completion_sender.clone();
    tokio::spawn(async move {
        let outcome = match pending.wait(timeout).await {
            Ok(value) => match NavigationResult::from_value(method, value) {
                Ok(result) => NavigationOutcome::Result(result),
                Err(error) => NavigationOutcome::Failed {
                    failure: NavigationFailure::Protocol(format!(
                        "resposta de navegação inválida: {error}"
                    )),
                },
            },
            Err(error) => navigation_error_outcome(error),
        };
        let _ = completion_sender.send((operation_id, context, outcome));
    });
}

fn navigation_error_outcome(error: ServerError) -> NavigationOutcome {
    match error {
        ServerError::Timeout { duration, .. } => NavigationOutcome::TimedOut { duration },
        ServerError::Cancelled => NavigationOutcome::Cancelled,
        ServerError::Closed | ServerError::NotRunning | ServerError::ShuttingDown => {
            NavigationOutcome::SessionEnded
        }
        ServerError::Io(error) => NavigationOutcome::Failed {
            failure: NavigationFailure::Transport(error.to_string()),
        },
        ServerError::Transport(error) => NavigationOutcome::Failed {
            failure: NavigationFailure::Transport(error.to_string()),
        },
        ServerError::Response(error) => NavigationOutcome::Failed {
            failure: NavigationFailure::Server(error.to_string()),
        },
        ServerError::Protocol(message) => NavigationOutcome::Failed {
            failure: NavigationFailure::Protocol(message),
        },
        error => NavigationOutcome::Failed {
            failure: NavigationFailure::Internal(error.to_string()),
        },
    }
}

fn cancel_navigation_matching(
    active_navigation: &mut BTreeMap<NavigationOperationId, ActiveNavigation>,
    events: &Sender<ServiceEvent>,
    predicate: impl Fn(&NavigationRequestContext) -> bool,
    outcome: NavigationOutcome,
) {
    let matching: Vec<_> = active_navigation
        .iter()
        .filter(|(_, navigation)| predicate(&navigation.context))
        .map(|(id, _)| *id)
        .collect();
    for operation_id in matching {
        if let Some(navigation) = active_navigation.remove(&operation_id) {
            navigation.cancellation.cancel();
            let _ = events.send(ServiceEvent::NavigationCompleted {
                context: navigation.context,
                outcome: outcome.clone(),
            });
        }
    }
}

fn report_sync_failure(
    pool: &mut ServerPool,
    key: &ServerKey,
    events: &Sender<ServiceEvent>,
    operation: ServiceOperation,
    uri: String,
    error: ServerError,
) {
    // A falha pode ser apenas uma alteração inválida rejeitada antes do envio;
    // nesse caso o servidor continua consistente. Só removemos o processo do
    // pool quando o próprio estado do servidor foi invalidado.
    let failed = pool.get(key).is_some_and(|server| {
        matches!(server.status(), ServerStatus::Failed | ServerStatus::Exited)
    });

    let _ = events.send(ServiceEvent::Failure {
        operation,
        uri: Some(uri),
        message: error.to_string(),
    });

    if failed && let Some(server) = pool.remove(key) {
        drop(server);
        let _ = events.send(ServiceEvent::ServerExited {
            workspace: key.workspace.clone(),
            language_id: key.language_id.clone(),
            message: format!("estado inconsistente após falha ao {operation}: {error}"),
        });
    }
}

fn drain_server_events(
    pool: &mut ServerPool,
    events: &Sender<ServiceEvent>,
    active_navigation: &mut BTreeMap<NavigationOperationId, ActiveNavigation>,
) {
    let keys: Vec<ServerKey> = pool.keys().cloned().collect();
    let mut dead = Vec::new();

    for key in keys {
        let mut fatal = false;
        let Some(server) = pool.get_mut(&key) else {
            continue;
        };

        while let Some(event) = server.try_next_event() {
            if let ServerEvent::Notification(notification) = event {
                if notification.method == "textDocument/publishDiagnostics"
                    && let Some(params) = notification.params
                    && let Ok(publication) =
                        serde_json::from_value::<PublishDiagnosticsParams>(params)
                {
                    let _ = events.send(ServiceEvent::DiagnosticsPublished {
                        workspace: key.workspace.clone(),
                        language_id: key.language_id.clone(),
                        session_id: server.session_id(),
                        current_document_version: server.document_version(&publication.uri),
                        publication,
                    });
                }
                continue;
            }
            let message = match event {
                ServerEvent::Exited(reason) => {
                    server.invalidate_sync();
                    fatal = true;
                    format!("processo encerrado: {reason:?}")
                }
                ServerEvent::TransportError(message) => {
                    server.invalidate_sync();
                    fatal = true;
                    message
                }
                ServerEvent::Request(request) => {
                    // O cliente desta etapa não consome requisições do
                    // servidor. Responder `null` evita que servidores reais
                    // fiquem esperando indefinidamente por uma resposta.
                    let _ = server.respond_ok(request.id, serde_json::Value::Null);
                    continue;
                }
                _ => continue,
            };

            let _ = events.send(ServiceEvent::ServerExited {
                workspace: key.workspace.clone(),
                language_id: key.language_id.clone(),
                message,
            });

            if fatal {
                break;
            }
        }

        if fatal {
            dead.push(key);
        }
    }

    // Um processo encerrado ou com transporte quebrado não pode continuar no
    // pool: a próxima abertura da chave precisa iniciar um processo novo.
    for key in dead {
        cancel_navigation_matching(
            active_navigation,
            events,
            |context| context.workspace == key.workspace && context.language_id == key.language_id,
            NavigationOutcome::SessionEnded,
        );
        if let Some(server) = pool.remove(&key) {
            drop(server);
        }
    }
}
