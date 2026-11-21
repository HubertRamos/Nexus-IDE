//! Gerenciamento do ciclo de vida dos servidores LSP.
//!
//! # Decisões arquiteturais
//!
//! - O processo é criado com [`tokio::process::Command`] e
//!   `kill_on_drop(true)`. Assim, mesmo que o dono do servidor seja descartado
//!   sem chamar [`LspServer::shutdown`], o processo não fica órfão.
//! - Leitura, escrita e drenagem de `stderr` rodam em tarefas independentes.
//!   Uma escrita bloqueada nunca impede o leitor de receber respostas, o que
//!   evita deadlocks entre requisições e notificações.
//! - Requisições pendentes são indexadas por [`Id`]. O leitor entrega ao mapa
//!   apenas mensagens classificadas como resposta; notificações e requisições
//!   enviadas pelo servidor viram eventos distintos.
//! - O canal de eventos é ilimitado para que o leitor nunca bloqueie; o
//!   consumidor deve drená-lo. O histórico de `stderr` é limitado por
//!   [`ServerConfig::stderr_capacity`].
//! - O encerramento é *best-effort*: tenta `shutdown`, envia `exit`, fecha o
//!   `stdin` e mata o processo se ele não terminar dentro do prazo.
//! - O núcleo é genérico. Presets de linguagem ficam em [`crate::languages`].

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::jsonrpc::{ErrorObject, Id, Message, Request, Response};
use crate::protocol::{
    ClientCapabilities, ClientInfo, InitializeParams, InitializeResult, ServerCapabilities,
    ServerInfo, WorkspaceFolder,
};
use crate::sync::{DocumentSync, SyncError, SyncNotification, SyncSettings, TextChange};
use crate::transport::{LspReader, LspWriter, TransportError};

/// Tempo padrão para o handshake inicial.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Tempo padrão para uma requisição comum.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Prazo padrão para requisições de navegação, independente das demais.
pub const DEFAULT_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(10);
/// Tempo padrão para cada etapa do encerramento.
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Quantidade padrão de linhas de `stderr` mantidas em memória.
pub const DEFAULT_STDERR_CAPACITY: usize = 200;
static NEXT_SESSION_ID: AtomicI64 = AtomicI64::new(1);

/// Configuração genérica de um servidor LSP externo.
///
/// O núcleo não conhece detalhes de Rust ou Python. Presets prontos ficam em
/// [`crate::languages`] e podem ser ajustados com os métodos `with_*`.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Identificador da linguagem usado futuramente na sincronização.
    pub language_id: String,
    /// Executável do servidor.
    pub executable: PathBuf,
    /// Argumentos de linha de comando.
    pub arguments: Vec<OsString>,
    /// Variáveis de ambiente adicionais.
    pub environment: Vec<(OsString, OsString)>,
    /// Diretório de trabalho opcional.
    pub working_directory: Option<PathBuf>,
    /// URI da raiz do workspace, quando houver.
    pub root_uri: Option<String>,
    /// Opções específicas do servidor, repassadas em `initializationOptions`.
    pub initialization_options: Option<Value>,
    /// Informações do cliente enviadas em `initialize`.
    pub client_info: ClientInfo,
    /// Pastas de trabalho declaradas em `initialize`.
    pub workspace_folders: Vec<WorkspaceFolder>,
    /// Prazo para concluir o handshake inicial.
    pub startup_timeout: Duration,
    /// Prazo padrão para requisições feitas depois do handshake.
    pub request_timeout: Duration,
    /// Prazo específico das requisições de navegação.
    pub navigation_timeout: Duration,
    /// Prazo para cada etapa do encerramento.
    pub shutdown_timeout: Duration,
    /// Quantidade máxima de linhas de `stderr` mantidas em memória.
    pub stderr_capacity: usize,
}

impl ServerConfig {
    /// Cria uma configuração com os padrões do Nexus.
    pub fn new(language_id: impl Into<String>, executable: impl Into<PathBuf>) -> Self {
        Self {
            language_id: language_id.into(),
            executable: executable.into(),
            arguments: Vec::new(),
            environment: Vec::new(),
            working_directory: None,
            root_uri: None,
            initialization_options: None,
            client_info: ClientInfo {
                name: "Nexus".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            },
            workspace_folders: Vec::new(),
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            navigation_timeout: DEFAULT_NAVIGATION_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            stderr_capacity: DEFAULT_STDERR_CAPACITY,
        }
    }

    /// Substitui o executável padrão.
    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = executable.into();
        self
    }

    /// Substitui todos os argumentos.
    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    /// Acrescenta um argumento.
    pub fn with_argument(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    /// Acrescenta uma variável de ambiente.
    pub fn with_environment(
        mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Self {
        self.environment.push((key.into(), value.into()));
        self
    }

    /// Define o diretório de trabalho do processo.
    pub fn with_working_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.working_directory = Some(directory.into());
        self
    }

    /// Define a URI da raiz do workspace.
    pub fn with_root_uri(mut self, root_uri: impl Into<String>) -> Self {
        self.root_uri = Some(root_uri.into());
        self
    }

    /// Define `initializationOptions`.
    pub fn with_initialization_options(mut self, options: Value) -> Self {
        self.initialization_options = Some(options);
        self
    }

    /// Substitui as informações do cliente.
    pub fn with_client_info(mut self, client_info: ClientInfo) -> Self {
        self.client_info = client_info;
        self
    }

    /// Acrescenta uma pasta de trabalho.
    pub fn with_workspace_folder(
        mut self,
        uri: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        self.workspace_folders.push(WorkspaceFolder {
            uri: uri.into(),
            name: name.into(),
        });
        self
    }

    /// Ajusta o prazo do handshake inicial.
    pub fn with_startup_timeout(mut self, duration: Duration) -> Self {
        self.startup_timeout = duration;
        self
    }

    /// Ajusta o prazo padrão de requisições.
    pub fn with_request_timeout(mut self, duration: Duration) -> Self {
        self.request_timeout = duration;
        self
    }

    /// Ajusta o timeout de navegação sem afetar as requisições comuns.
    pub fn with_navigation_timeout(mut self, duration: Duration) -> Self {
        self.navigation_timeout = duration;
        self
    }

    /// Ajusta o prazo de encerramento.
    pub fn with_shutdown_timeout(mut self, duration: Duration) -> Self {
        self.shutdown_timeout = duration;
        self
    }

    /// Ajusta quantas linhas de `stderr` são mantidas.
    pub fn with_stderr_capacity(mut self, capacity: usize) -> Self {
        self.stderr_capacity = capacity.max(1);
        self
    }
}

/// Estado observável do servidor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ServerStatus {
    /// Processo iniciado, handshake em andamento.
    Starting = 0,
    /// `initialize` e `initialized` concluídos.
    Running = 1,
    /// Encerramento em andamento.
    ShuttingDown = 2,
    /// Processo encerrado.
    Exited = 3,
    /// Falha de inicialização ou de transporte.
    Failed = 4,
}

impl ServerStatus {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Starting,
            1 => Self::Running,
            2 => Self::ShuttingDown,
            3 => Self::Exited,
            _ => Self::Failed,
        }
    }
}

/// Motivo do encerramento observado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// O encerramento foi solicitado por [`LspServer::shutdown`].
    Requested,
    /// O processo terminou sem solicitação.
    Unexpected,
}

/// Eventos produzidos pelo servidor fora do fluxo de respostas.
#[derive(Debug)]
pub enum ServerEvent {
    /// Notificação enviada pelo servidor.
    Notification(crate::jsonrpc::Notification),
    /// Requisição enviada pelo servidor; responda com [`LspServer::respond`].
    Request(Request),
    /// Resposta cujo `Id` não corresponde a nenhuma requisição pendente.
    UnknownResponse(Response),
    /// Linha recebida em `stderr`.
    Stderr(String),
    /// Processo encerrado.
    Exited(ExitReason),
    /// Falha de transporte; o servidor será encerrado.
    TransportError(String),
}

/// Falhas do gerenciamento de servidores.
#[derive(Debug)]
pub enum ServerError {
    /// O executável não pôde ser iniciado.
    Spawn {
        executable: PathBuf,
        source: std::io::Error,
    },
    /// Falha de I/O ao interagir com o processo.
    Io(std::io::Error),
    /// Falha no enquadramento ou na serialização JSON-RPC.
    Transport(TransportError),
    /// Resposta ou configuração incompatível com o protocolo esperado.
    Protocol(String),
    /// O servidor respondeu com um erro JSON-RPC.
    Response(ErrorObject),
    /// Prazo esgotado.
    Timeout {
        operation: &'static str,
        duration: Duration,
    },
    /// O processo ou o canal de comunicação já foi encerrado.
    Closed,
    /// O servidor não está em execução.
    NotRunning,
    /// O servidor está sendo encerrado.
    ShuttingDown,
    /// Requisição de navegação cancelada local e remotamente.
    Cancelled,
}

impl ServerError {
    /// Indica que o executável não foi encontrado.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Spawn { source, .. } if source.kind() == ErrorKind::NotFound
        )
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { executable, source } if source.kind() == ErrorKind::NotFound => {
                write!(
                    formatter,
                    "servidor LSP não encontrado: `{}`; instale-o ou configure o caminho do executável",
                    executable.display()
                )
            }
            Self::Spawn { executable, source } => write!(
                formatter,
                "não foi possível iniciar o servidor LSP `{}`: {source}",
                executable.display()
            ),
            Self::Io(error) => write!(formatter, "falha de I/O no servidor LSP: {error}"),
            Self::Transport(error) => write!(formatter, "falha de transporte LSP: {error}"),
            Self::Protocol(message) => write!(formatter, "falha de protocolo LSP: {message}"),
            Self::Response(error) => {
                write!(formatter, "o servidor LSP respondeu com erro: {error}")
            }
            Self::Timeout {
                operation,
                duration,
            } => write!(
                formatter,
                "tempo esgotado durante {operation} do servidor LSP ({duration:?})"
            ),
            Self::Closed => write!(formatter, "servidor LSP encerrado ou canal fechado"),
            Self::NotRunning => write!(formatter, "servidor LSP não está em execução"),
            Self::ShuttingDown => write!(formatter, "servidor LSP está sendo encerrado"),
            Self::Cancelled => write!(formatter, "requisição LSP cancelada"),
        }
    }
}

impl std::error::Error for ServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Io(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Response(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ServerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<TransportError> for ServerError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

enum PendingOutcome {
    Value(Value),
    Error(ErrorObject),
    Closed,
}

enum Outgoing {
    Message(Message),
    Close,
}

struct Shared {
    pending: StdMutex<HashMap<Id, oneshot::Sender<PendingOutcome>>>,
    status: AtomicU8,
    stderr: StdMutex<VecDeque<String>>,
    stderr_capacity: usize,
    transport_error: StdMutex<Option<String>>,
    events: mpsc::UnboundedSender<ServerEvent>,
    process: StdMutex<Option<Child>>,
    /// Teste determinístico de falha de envio. Permanece `false` em produção.
    fail_next_send: AtomicBool,
}

fn lock_std<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sync_protocol_error(error: SyncError) -> ServerError {
    ServerError::Protocol(format!("falha de sincronização: {error}"))
}

impl Shared {
    fn status(&self) -> ServerStatus {
        ServerStatus::from_u8(self.status.load(Ordering::SeqCst))
    }

    fn can_send_request(&self) -> bool {
        matches!(
            self.status(),
            ServerStatus::Starting | ServerStatus::Running
        )
    }

    fn set_status(&self, status: ServerStatus) {
        self.status.store(status as u8, Ordering::SeqCst);
    }

    fn mark_running(&self) -> bool {
        self.status
            .compare_exchange(
                ServerStatus::Starting as u8,
                ServerStatus::Running as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    fn process_id(&self) -> Option<u32> {
        lock_std(&self.process).as_ref().and_then(Child::id)
    }

    fn take_process(&self) -> Option<Child> {
        lock_std(&self.process).take()
    }

    fn kill_process(&self) {
        let mut process = lock_std(&self.process);
        if let Some(child) = process.as_mut() {
            let _ = child.start_kill();
        }
    }

    fn push_stderr(&self, line: String) {
        {
            let mut buffer = lock_std(&self.stderr);
            while buffer.len() >= self.stderr_capacity {
                buffer.pop_front();
            }
            buffer.push_back(line.clone());
        }
        let _ = self.events.send(ServerEvent::Stderr(line));
    }

    fn stderr_snapshot(&self) -> Vec<String> {
        lock_std(&self.stderr).iter().cloned().collect()
    }

    fn set_transport_error(&self, message: String) {
        *lock_std(&self.transport_error) = Some(message);
    }

    fn transport_error(&self) -> Option<String> {
        lock_std(&self.transport_error).clone()
    }

    fn insert_pending(&self, id: Id, sender: oneshot::Sender<PendingOutcome>) {
        lock_std(&self.pending).insert(id, sender);
    }

    fn remove_pending(&self, id: &Id) -> Option<oneshot::Sender<PendingOutcome>> {
        lock_std(&self.pending).remove(id)
    }

    fn fail_pending(&self) {
        let mut pending = lock_std(&self.pending);
        for (_, sender) in pending.drain() {
            let _ = sender.send(PendingOutcome::Closed);
        }
    }
}

/// Remove a entrada pendente quando a requisição é concluída, expira ou é
/// cancelada. Requisições de navegação também enviam cancelamento remoto.
struct PendingGuard {
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    id: Id,
    cancel_requested: Arc<AtomicBool>,
    cancel_on_drop: bool,
}

impl PendingGuard {
    fn cancel(&mut self) -> bool {
        NavigationCancellation {
            shared: self.shared.clone(),
            outgoing: self.outgoing.clone(),
            id: self.id.clone(),
            cancel_requested: self.cancel_requested.clone(),
        }
        .cancel()
    }

    fn send_cancel(&self) {
        let mut notification = crate::jsonrpc::Notification::new("$/cancelRequest");
        notification.params = Some(serde_json::json!({"id": self.id}));
        let _ = self
            .outgoing
            .send(Outgoing::Message(Message::Notification(notification)));
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.shared.remove_pending(&self.id).is_some()
            && self.cancel_on_drop
            && self.shared.can_send_request()
        {
            self.send_cancel();
        }
    }
}

/// Requisição de navegação enviada e ainda aguardando resposta.
///
/// Descartar o handle remove o ID pendente e envia `$/cancelRequest` se a
/// resposta ainda não chegou.
pub struct PendingNavigationRequest {
    id: Id,
    receiver: Option<oneshot::Receiver<PendingOutcome>>,
    guard: Option<PendingGuard>,
}

/// Handle independente para cancelar a espera e notificar o servidor.
#[derive(Clone)]
pub struct NavigationCancellation {
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    id: Id,
    cancel_requested: Arc<AtomicBool>,
}

impl NavigationCancellation {
    /// Cancela apenas se a requisição ainda estiver pendente.
    pub fn cancel(&self) -> bool {
        if self.cancel_requested.swap(true, Ordering::SeqCst) {
            return false;
        }
        if self.shared.remove_pending(&self.id).is_none() {
            return false;
        }
        if self.shared.can_send_request() {
            let mut notification = crate::jsonrpc::Notification::new("$/cancelRequest");
            notification.params = Some(serde_json::json!({"id": self.id}));
            let _ = self
                .outgoing
                .send(Outgoing::Message(Message::Notification(notification)));
        }
        true
    }
}

impl PendingNavigationRequest {
    /// ID JSON-RPC original, reutilizado em `$/cancelRequest`.
    pub fn id(&self) -> &Id {
        &self.id
    }

    pub fn cancellation_handle(&self) -> NavigationCancellation {
        let guard = self.guard.as_ref().expect("guard da requisição pendente");
        NavigationCancellation {
            shared: guard.shared.clone(),
            outgoing: guard.outgoing.clone(),
            id: self.id.clone(),
            cancel_requested: guard.cancel_requested.clone(),
        }
    }

    /// Cancela uma única vez, remove o registro e envia `$/cancelRequest`.
    pub fn cancel(&mut self) -> bool {
        self.guard.as_mut().is_some_and(PendingGuard::cancel)
    }

    /// Aguarda a resposta sem bloquear o runtime.
    pub async fn wait(mut self, duration: Duration) -> Result<Value, ServerError> {
        let receiver = self
            .receiver
            .take()
            .expect("uma requisição pendente só pode ser aguardada uma vez");
        match timeout(duration, receiver).await {
            Ok(Ok(PendingOutcome::Value(value))) => {
                self.guard.take();
                Ok(value)
            }
            Ok(Ok(PendingOutcome::Error(error))) => {
                self.guard.take();
                Err(ServerError::Response(error))
            }
            Ok(Ok(PendingOutcome::Closed)) | Ok(Err(_)) => {
                if self
                    .guard
                    .as_ref()
                    .is_some_and(|guard| guard.cancel_requested.load(Ordering::SeqCst))
                {
                    Err(ServerError::Cancelled)
                } else {
                    Err(ServerError::Closed)
                }
            }
            Err(_) => Err(ServerError::Timeout {
                operation: "navegação LSP",
                duration,
            }),
        }
    }
}

/// Servidor LSP iniciado e com handshake concluído.
///
/// O tipo mantém o processo, as tarefas de I/O e o mapa de requisições
/// pendentes. Chame [`LspServer::shutdown`] para o encerramento ordenado;
/// o [`Drop`] faz uma limpeza de emergência.
pub struct LspServer {
    session_id: i64,
    config: ServerConfig,
    initialize_result: InitializeResult,
    shared: Arc<Shared>,
    outgoing: mpsc::UnboundedSender<Outgoing>,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    reader_task: Option<JoinHandle<()>>,
    writer_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    next_id: AtomicI64,
    /// Estado de sincronização compartilhado por todos os documentos atendidos
    /// por este servidor. O `StdMutex` serializa versões e envio de notificações
    /// mesmo quando a API é usada de threads diferentes.
    sync: StdMutex<DocumentSync>,
}

impl fmt::Debug for LspServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LspServer")
            .field("language_id", &self.config.language_id)
            .field("status", &self.status())
            .field("process_id", &self.process_id())
            .field("open_documents", &lock_std(&self.sync).len())
            .finish_non_exhaustive()
    }
}

impl LspServer {
    /// Inicia o processo, estabelece o transporte e conclui o handshake.
    ///
    /// O retorno só acontece depois de `initialize` ser respondido e
    /// `initialized` ser enviado. Falhas de processo, protocolo ou prazo
    /// encerram o processo antes de retornar o erro.
    pub async fn start(config: ServerConfig) -> Result<Self, ServerError> {
        let mut command = Command::new(&config.executable);
        command
            .args(&config.arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(directory) = &config.working_directory {
            command.current_dir(directory);
        }
        for (key, value) in &config.environment {
            command.env(key, value);
        }

        let mut child = command.spawn().map_err(|source| ServerError::Spawn {
            executable: config.executable.clone(),
            source,
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            ServerError::Protocol("stdin do servidor LSP indisponível".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ServerError::Protocol("stdout do servidor LSP indisponível".to_owned())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ServerError::Protocol("stderr do servidor LSP indisponível".to_owned())
        })?;

        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            pending: StdMutex::new(HashMap::new()),
            status: AtomicU8::new(ServerStatus::Starting as u8),
            stderr: StdMutex::new(VecDeque::new()),
            stderr_capacity: config.stderr_capacity.max(1),
            transport_error: StdMutex::new(None),
            events: event_sender,
            process: StdMutex::new(Some(child)),
            fail_next_send: AtomicBool::new(false),
        });

        let (outgoing, outgoing_receiver) = mpsc::unbounded_channel();
        let writer_task = tokio::spawn(writer_loop(
            LspWriter::new(stdin),
            outgoing_receiver,
            shared.clone(),
        ));
        let reader_task = tokio::spawn(reader_loop(LspReader::new(stdout), shared.clone()));
        let stderr_task = tokio::spawn(stderr_loop(stderr, shared.clone()));

        let mut server = Self {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            config,
            initialize_result: InitializeResult::default(),
            shared,
            outgoing,
            events: event_receiver,
            reader_task: Some(reader_task),
            writer_task: Some(writer_task),
            stderr_task: Some(stderr_task),
            next_id: AtomicI64::new(1),
            sync: StdMutex::new(DocumentSync::disabled()),
        };

        match server.handshake().await {
            Ok(result) => {
                // O Nexus só implementa posições UTF-16. Não anunciamos outras
                // codificações no `initialize`; se um servidor escolher uma
                // codificação diferente, continuar enviaria posições erradas.
                if let Some(encoding) = result.capabilities.position_encoding.as_deref()
                    && !encoding.eq_ignore_ascii_case("utf-16")
                {
                    server.cleanup_after_failure().await;
                    return Err(ServerError::Protocol(format!(
                        "servidor anunciou positionEncoding `{encoding}`; o Nexus suporta apenas utf-16"
                    )));
                }

                server.sync = StdMutex::new(DocumentSync::from_capabilities(&result.capabilities));
                server.initialize_result = result;
                // Se o processo morreu logo após responder `initialize`, o
                // leitor já pode ter marcado o estado como `Exited`. Não
                // sobrescrevemos esse estado; o evento de saída continua
                // disponível para o consumidor.
                server.shared.mark_running();
                Ok(server)
            }
            Err(error) => {
                server.cleanup_after_failure().await;
                Err(error)
            }
        }
    }

    async fn handshake(&self) -> Result<InitializeResult, ServerError> {
        let capabilities = ClientCapabilities::nexus_defaults();

        let params = InitializeParams {
            process_id: Some(i64::from(std::process::id())),
            client_info: Some(self.config.client_info.clone()),
            locale: None,
            root_path: None,
            root_uri: self.config.root_uri.clone(),
            initialization_options: self.config.initialization_options.clone(),
            capabilities,
            trace: None,
            workspace_folders: if self.config.workspace_folders.is_empty() {
                None
            } else {
                Some(self.config.workspace_folders.clone())
            },
        };

        let params = serde_json::to_value(&params).map_err(|error| {
            ServerError::Protocol(format!("parâmetros de initialize inválidos: {error}"))
        })?;

        let result = self
            .request_internal(
                "initialize",
                Some(params),
                "initialize",
                self.config.startup_timeout,
            )
            .await?;
        let result: InitializeResult = serde_json::from_value(result).map_err(|error| {
            ServerError::Protocol(format!("resposta de initialize inválida: {error}"))
        })?;

        self.notify_internal("initialized", None)?;
        Ok(result)
    }

    /// Resultado completo do `initialize`.
    pub fn initialize_result(&self) -> &InitializeResult {
        &self.initialize_result
    }

    /// Capacidades anunciadas pelo servidor.
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.initialize_result.capabilities
    }

    /// Informações do servidor, quando anunciadas.
    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.initialize_result.server_info.as_ref()
    }

    /// Identificador da linguagem configurada.
    pub fn language_id(&self) -> &str {
        &self.config.language_id
    }

    /// Identidade única desta instância de sessão, diferente após reinício.
    pub fn session_id(&self) -> i64 {
        self.session_id
    }

    /// Versão LSP atual do documento aberto.
    pub fn document_version(&self, uri: &str) -> Option<i64> {
        lock_std(&self.sync)
            .document(uri)
            .map(|document| document.version)
    }

    /// Estado atual do servidor.
    pub fn status(&self) -> ServerStatus {
        self.shared.status()
    }

    /// Prazo configurado para requisições de navegação.
    pub fn navigation_timeout(&self) -> Duration {
        self.config.navigation_timeout
    }

    /// Indica se o servidor está ativo e aceitando mensagens.
    pub fn is_running(&self) -> bool {
        self.status() == ServerStatus::Running
    }

    /// PID do processo, enquanto ele estiver sob gerenciamento.
    pub fn process_id(&self) -> Option<u32> {
        self.shared.process_id()
    }

    /// Últimas linhas capturadas de `stderr`.
    pub fn stderr_snapshot(&self) -> Vec<String> {
        self.shared.stderr_snapshot()
    }

    /// Última falha de transporte registrada, se houver.
    pub fn last_transport_error(&self) -> Option<String> {
        self.shared.transport_error()
    }

    /// Capacidades de sincronização negociadas no `initialize`.
    pub fn sync_settings(&self) -> SyncSettings {
        lock_std(&self.sync).settings()
    }

    /// Quantidade de documentos atualmente abertos neste servidor.
    pub fn open_document_count(&self) -> usize {
        lock_std(&self.sync).len()
    }

    /// Indica se a URI está aberta neste servidor.
    pub fn is_document_open(&self, uri: &str) -> bool {
        lock_std(&self.sync).is_open(uri)
    }

    /// Registra um documento e envia `textDocument/didOpen` quando cabível.
    ///
    /// Um mesmo servidor pode atender vários documentos. A URI é a chave, de
    /// modo que arquivos com o mesmo nome em diretórios diferentes permanecem
    /// independentes.
    pub fn open_document(
        &self,
        uri: impl Into<String>,
        language_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<(), ServerError> {
        self.apply_sync(|sync| sync.open(uri, language_id, text))
    }

    /// Envia as alterações de uma transação do buffer.
    ///
    /// `changes` estão na ordem de aplicação e `current_text` é o conteúdo
    /// final. O modo completo e o incremental são escolhidos pelo estado de
    /// sincronização do servidor.
    pub fn change_document(
        &self,
        uri: &str,
        changes: &[TextChange],
        current_text: &str,
    ) -> Result<(), ServerError> {
        self.apply_sync(|sync| sync.change(uri, changes, current_text))
    }

    /// Recalcula o conteúdo do documento fora de uma `Transaction`.
    ///
    /// Usado por Undo/Redo, recarregamento do disco e recuperação. No modo
    /// incremental, envia o menor intervalo substituído; no modo completo,
    /// envia o texto inteiro.
    pub fn resync_document(&self, uri: &str, text: &str) -> Result<(), ServerError> {
        self.apply_sync(|sync| sync.resync(uri, text))
    }

    /// Registra o salvamento e envia `textDocument/didSave` quando cabível.
    pub fn save_document(&self, uri: &str, text: &str) -> Result<(), ServerError> {
        self.apply_sync(|sync| sync.save(uri, text))
    }

    /// Remove o documento e envia `textDocument/didClose` quando cabível.
    ///
    /// O processo do servidor e os demais documentos continuam ativos.
    pub fn close_document(&self, uri: &str) -> Result<(), ServerError> {
        self.apply_sync(|sync| sync.close(uri))
    }

    /// Aplica uma operação de sincronização e só a considera concluída depois
    /// que a notificação foi enfileirada com sucesso.
    ///
    /// O `DocumentSync` é atualizado antes do envio para produzir a versão e o
    /// delta corretos. Se o envio falhar, todo o estado daquele servidor é
    /// descartado: o transporte pode ter recebido uma mensagem parcial, então
    /// continuar enviando incrementais sobre a versão antiga seria pior do que
    /// exigir uma reconstrução. O servidor também passa a `Failed`, o que
    /// impede novas operações até uma inicialização futura.
    fn apply_sync<F>(&self, apply: F) -> Result<(), ServerError>
    where
        F: FnOnce(&mut DocumentSync) -> Result<Option<SyncNotification>, SyncError>,
    {
        self.ensure_accepting()?;
        let mut sync = lock_std(&self.sync);
        let notification = apply(&mut sync).map_err(sync_protocol_error)?;
        let Some(notification) = notification else {
            return Ok(());
        };

        if let Err(error) = self.send_sync_notification(notification) {
            sync.clear();
            drop(sync);
            self.mark_inconsistent(format!(
                "estado de sincronização invalidado após falha de envio: {error}"
            ));
            return Err(error);
        }

        Ok(())
    }

    /// Descarta o estado de sincronização sem tentar notificar o servidor.
    ///
    /// Chamado quando o processo termina inesperadamente ou o transporte
    /// falha. A próxima inicialização começa sem documentos e envia `didOpen`.
    pub fn invalidate_sync(&self) {
        lock_std(&self.sync).clear();
    }

    fn mark_inconsistent(&self, detail: String) {
        self.shared.set_transport_error(detail);
        if self.status() != ServerStatus::Exited {
            self.shared.set_status(ServerStatus::Failed);
        }
    }

    /// Força a próxima mensagem a falhar antes de chegar ao transporte.
    ///
    /// Existe apenas para testar a invalidação de estado sem depender de
    /// corrida com o encerramento do processo. Nunca é chamado em produção.
    #[doc(hidden)]
    pub fn fail_next_send_for_test(&self) {
        self.shared.fail_next_send.store(true, Ordering::SeqCst);
    }

    fn send_sync_notification(&self, notification: SyncNotification) -> Result<(), ServerError> {
        let method = notification.method();
        let params = notification.to_params().map_err(|error| {
            ServerError::Protocol(format!("parâmetros de sincronização inválidos: {error}"))
        })?;
        self.notify_internal(method, Some(params))
    }

    /// Aguarda o próximo evento do servidor.
    pub async fn next_event(&mut self) -> Option<ServerEvent> {
        self.events.recv().await
    }

    /// Tenta obter um evento sem aguardar.
    pub fn try_next_event(&mut self) -> Option<ServerEvent> {
        self.events.try_recv().ok()
    }

    /// Envia uma requisição e espera a resposta correspondente pelo `Id`.
    pub async fn request(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<Value, ServerError> {
        self.ensure_accepting()?;
        self.request_internal(
            method,
            params,
            "requisição LSP",
            self.config.request_timeout,
        )
        .await
    }

    /// Envia uma requisição e desserializa a resposta.
    pub async fn request_typed<P, R>(
        &self,
        method: impl Into<String>,
        params: P,
    ) -> Result<R, ServerError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let params = serde_json::to_value(params).map_err(|error| {
            ServerError::Protocol(format!("parâmetros da requisição inválidos: {error}"))
        })?;
        let value = self.request(method, Some(params)).await?;
        serde_json::from_value(value).map_err(|error| {
            ServerError::Protocol(format!("resposta da requisição inválida: {error}"))
        })
    }

    async fn request_internal(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
        operation: &'static str,
        duration: Duration,
    ) -> Result<Value, ServerError> {
        let id = Id::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let (sender, receiver) = oneshot::channel();
        self.shared.insert_pending(id.clone(), sender);
        let _guard = PendingGuard {
            shared: self.shared.clone(),
            outgoing: self.outgoing.clone(),
            id: id.clone(),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            cancel_on_drop: false,
        };

        let mut request = Request::new(id.clone(), method);
        request.params = params;

        self.send_message(Message::Request(request))?;

        match timeout(duration, receiver).await {
            Ok(Ok(PendingOutcome::Value(value))) => Ok(value),
            Ok(Ok(PendingOutcome::Error(error))) => Err(ServerError::Response(error)),
            Ok(Ok(PendingOutcome::Closed)) | Ok(Err(_)) => Err(ServerError::Closed),
            Err(_) => Err(ServerError::Timeout {
                operation,
                duration,
            }),
        }
    }

    /// Envia uma requisição não bloqueante de navegação para o transporte.
    ///
    /// O resultado pode ser aguardado por outra tarefa; descartar ou cancelar
    /// a requisição limpa o ID pendente e envia `$/cancelRequest`.
    pub fn begin_navigation_request(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<PendingNavigationRequest, ServerError> {
        self.ensure_accepting()?;
        let id = Id::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let (sender, receiver) = oneshot::channel();
        self.shared.insert_pending(id.clone(), sender);
        let guard = PendingGuard {
            shared: self.shared.clone(),
            outgoing: self.outgoing.clone(),
            id: id.clone(),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            cancel_on_drop: true,
        };

        let mut request = Request::new(id.clone(), method);
        request.params = params;
        if let Err(error) = self.send_message(Message::Request(request)) {
            drop(guard);
            return Err(error);
        }

        Ok(PendingNavigationRequest {
            id,
            receiver: Some(receiver),
            guard: Some(guard),
        })
    }

    /// Envia uma notificação.
    pub fn notify(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<(), ServerError> {
        self.ensure_accepting()?;
        self.notify_internal(method, params)
    }

    fn notify_internal(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> Result<(), ServerError> {
        let mut notification = crate::jsonrpc::Notification::new(method);
        notification.params = params;
        self.send_message(Message::Notification(notification))
    }

    /// Responde a uma requisição enviada pelo servidor.
    pub fn respond(&self, response: Response) -> Result<(), ServerError> {
        self.send_message(Message::Response(response))
    }

    /// Responde com sucesso a uma requisição do servidor.
    pub fn respond_ok(&self, id: Id, value: Value) -> Result<(), ServerError> {
        self.respond(Response::success(id, value))
    }

    /// Responde com erro a uma requisição do servidor.
    pub fn respond_error(&self, id: Id, error: ErrorObject) -> Result<(), ServerError> {
        self.respond(Response::failure(id, error))
    }

    fn send_message(&self, message: Message) -> Result<(), ServerError> {
        if self.shared.fail_next_send.swap(false, Ordering::SeqCst) {
            return Err(ServerError::Closed);
        }

        self.outgoing
            .send(Outgoing::Message(message))
            .map_err(|_| ServerError::Closed)
    }

    fn ensure_accepting(&self) -> Result<(), ServerError> {
        match self.status() {
            ServerStatus::Starting | ServerStatus::Running => Ok(()),
            ServerStatus::ShuttingDown => Err(ServerError::ShuttingDown),
            ServerStatus::Exited | ServerStatus::Failed => Err(ServerError::NotRunning),
        }
    }

    /// Encerra o servidor de forma ordenada.
    ///
    /// A operação é idempotente. Se o servidor não responder a `shutdown` ou
    /// não terminar depois de `exit`, o processo é morto e aguardado.
    pub async fn shutdown(&mut self) -> Result<(), ServerError> {
        if self.status() == ServerStatus::Exited && self.shared.process_id().is_none() {
            self.stop_tasks().await;
            return Ok(());
        }

        let was_running = self.status() == ServerStatus::Running;
        self.shared.set_status(ServerStatus::ShuttingDown);

        if was_running {
            let _ = self
                .request_internal("shutdown", None, "shutdown", self.config.shutdown_timeout)
                .await;
            let _ = self.notify_internal("exit", None);
        }

        let _ = self.outgoing.send(Outgoing::Close);

        let Some(mut child) = self.shared.take_process() else {
            self.stop_tasks().await;
            self.shared.set_status(ServerStatus::Exited);
            return Ok(());
        };

        match timeout(self.config.shutdown_timeout, child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                self.stop_tasks().await;
                self.shared.set_status(ServerStatus::Failed);
                return Err(ServerError::Io(error));
            }
            Err(_) => {
                let _ = child.start_kill();
                match timeout(self.config.shutdown_timeout, child.wait()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        self.stop_tasks().await;
                        self.shared.set_status(ServerStatus::Failed);
                        return Err(ServerError::Io(error));
                    }
                    Err(_) => {
                        self.stop_tasks().await;
                        self.shared.set_status(ServerStatus::Failed);
                        return Err(ServerError::Timeout {
                            operation: "encerramento",
                            duration: self.config.shutdown_timeout,
                        });
                    }
                }
            }
        }

        self.stop_tasks().await;
        self.shared.set_status(ServerStatus::Exited);
        Ok(())
    }

    async fn cleanup_after_failure(&mut self) {
        let _ = self.outgoing.send(Outgoing::Close);
        if let Some(mut child) = self.shared.take_process() {
            let _ = child.start_kill();
            let _ = timeout(self.config.shutdown_timeout, child.wait()).await;
        }
        self.stop_tasks().await;
        self.shared.set_status(ServerStatus::Failed);
    }

    async fn stop_tasks(&mut self) {
        stop_task(self.reader_task.take()).await;
        stop_task(self.writer_task.take()).await;
        stop_task(self.stderr_task.take()).await;
    }
}

impl Drop for LspServer {
    fn drop(&mut self) {
        // Limpeza de emergência: não bloqueia e não depende do runtime.
        self.shared.kill_process();
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        if let Some(task) = self.writer_task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

// As tarefas assíncronas são definidas abaixo para manter a API pública no
// topo do módulo.

async fn stop_task(task: Option<JoinHandle<()>>) {
    let Some(mut task) = task else {
        return;
    };

    if timeout(Duration::from_millis(500), &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

async fn reader_loop(mut reader: LspReader<ChildStdout>, shared: Arc<Shared>) {
    loop {
        match reader.read_message().await {
            Ok(Some(Message::Response(response))) => {
                let id = response.id.clone();
                let outcome = match &response.error {
                    crate::jsonrpc::Presence::Value(error) => PendingOutcome::Error(error.clone()),
                    crate::jsonrpc::Presence::Null | crate::jsonrpc::Presence::Absent => {
                        match &response.result {
                            crate::jsonrpc::Presence::Value(value) => {
                                PendingOutcome::Value(value.clone())
                            }
                            crate::jsonrpc::Presence::Null => PendingOutcome::Value(Value::Null),
                            // `Response::validate` garante que existe `result`
                            // ou `error`; este braço é apenas defensivo.
                            crate::jsonrpc::Presence::Absent => PendingOutcome::Closed,
                        }
                    }
                };

                let sender = shared.remove_pending(&id);
                match sender {
                    Some(sender) => {
                        let _ = sender.send(outcome);
                    }
                    None => {
                        let _ = shared.events.send(ServerEvent::UnknownResponse(response));
                    }
                }
            }
            Ok(Some(Message::Notification(notification))) => {
                let _ = shared.events.send(ServerEvent::Notification(notification));
            }
            Ok(Some(Message::Request(request))) => {
                let _ = shared.events.send(ServerEvent::Request(request));
            }
            Ok(None) => break,
            Err(error) => {
                let detail = error.to_string();
                shared.set_transport_error(detail.clone());
                let _ = shared.events.send(ServerEvent::TransportError(detail));
                shared.kill_process();
                break;
            }
        }
    }

    shared.fail_pending();

    let shutting_down = shared.status() == ServerStatus::ShuttingDown;
    let reason = if shutting_down {
        ExitReason::Requested
    } else {
        ExitReason::Unexpected
    };

    if shared.status() != ServerStatus::Failed {
        shared.set_status(ServerStatus::Exited);
    }
    let _ = shared.events.send(ServerEvent::Exited(reason));

    // Se o processo fechou o stdout sem um encerramento solicitado, ele não
    // pode ficar órfão. Durante o `shutdown`, o próprio encerramento é
    // responsável por matar e aguardar o processo.
    if !shutting_down {
        shared.kill_process();
    }
}

async fn writer_loop(
    mut writer: LspWriter<ChildStdin>,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    shared: Arc<Shared>,
) {
    while let Some(message) = outgoing.recv().await {
        match message {
            Outgoing::Message(message) => {
                if let Err(error) = writer.write_message(&message).await {
                    let detail = error.to_string();
                    shared.set_transport_error(detail.clone());
                    let _ = shared.events.send(ServerEvent::TransportError(detail));
                    shared.kill_process();
                    shared.fail_pending();
                    break;
                }
            }
            Outgoing::Close => break,
        }
    }

    // Ao sair, `writer` é descartado e o `stdin` do filho é fechado.
}

async fn stderr_loop(stderr: ChildStderr, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer)
                    .trim_end_matches(['\r', '\n'])
                    .to_owned();
                shared.push_stderr(line);
            }
            Err(_) => break,
        }
    }
}
