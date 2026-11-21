//! Contexto e ciclo de vida das operações de navegação da aplicação.
//!
//! Este módulo mantém somente identificadores e metadados pequenos. Os
//! destinos normalizados continuam pertencendo ao protocolo LSP, e nenhuma
//! operação abre arquivos ou altera o cursor.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Index;
use std::path::PathBuf;
use std::time::Duration;

use nexus_core::buffer::position::CharOffset;
use nexus_lsp::protocol::{NavigationMethod, NavigationResult, Position};
use nexus_lsp::service::{
    NavigationFailure, NavigationOperationId, NavigationOutcome, NavigationRequestContext,
};

use crate::document::DocumentId;

/// Limite determinístico do histórico de navegação mantido em memória.
pub const NAVIGATION_HISTORY_LIMIT: usize = 100;

/// Localização completa o suficiente para restaurar o contexto de navegação.
///
/// `cursor` e `anchor` usam a representação interna (`CharOffset`) já adotada
/// pelo editor; a conversão para UTF-16 só acontece nas requisições LSP. A URI
/// é preservada para diagnósticos e para reabrir um arquivo fechado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationLocation {
    pub document_id: DocumentId,
    pub uri: String,
    pub path: Option<PathBuf>,
    pub cursor: CharOffset,
    pub anchor: Option<CharOffset>,
    pub top_line: usize,
    pub left_col: usize,
    pub workspace: Option<String>,
    pub workspace_generation: u64,
    pub revision: u64,
}

impl NavigationLocation {
    /// Mesma localização significa mesmo documento e mesmo cursor. O viewport
    /// não participa: rolar a tela sem navegar não cria uma localização nova.
    pub fn same_place(&self, other: &Self) -> bool {
        let same_document = self.document_id == other.document_id
            || (self.path.is_some() && self.path == other.path)
            || (!self.uri.is_empty() && self.uri == other.uri);

        same_document && self.cursor == other.cursor
    }
}

/// Contexto de origem capturado antes de enfileirar a requisição.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationOriginContext {
    pub operation_id: NavigationOperationId,
    pub document_id: DocumentId,
    pub method: NavigationMethod,
    pub workspace: String,
    pub language_id: String,
    pub workspace_generation: u64,
    pub session_id: Option<i64>,
    pub uri: String,
    pub position: Position,
    pub document_revision: u64,
    /// Snapshot da origem no momento da requisição, incluindo viewport.
    pub location: NavigationLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationStaleReason {
    CursorMoved,
    ActiveDocumentChanged,
    WorkspaceChanged,
    DocumentEdited,
    DocumentUriChanged,
}

/// Estado apresentado à futura camada de navegação, sem decisão visual.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationOperationStatus {
    Response { result: NavigationResult },
    Unsupported,
    Cancelled,
    TimedOut { duration: Duration },
    SessionEnded,
    TransportFailure { message: String },
    ServerFailure { message: String },
    ProtocolFailure { message: String },
    Failed { message: String },
}

/// Conclusão consumível uma única vez pela camada de aplicação.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationOperationCompletion {
    pub origin: NavigationOriginContext,
    /// Contexto capturado pelo serviço ao despachar a requisição LSP.
    pub lsp_context: Option<NavigationRequestContext>,
    pub context_state: NavigationContextState,
    pub status: NavigationOperationStatus,
}

/// Navegação efetivamente aplicada, com origem e destino completos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationHistoryEntry {
    pub method: NavigationMethod,
    pub origin: NavigationLocation,
    pub target: NavigationLocation,
}

/// Histórico sequencial de navegação com cursor de avanço/retorno.
///
/// `index` é a quantidade de entradas atualmente em vigor: 0 representa a
/// origem da primeira entrada e `len()` o último destino. Voltar e avançar
/// apenas movem esse índice; nenhuma entrada é criada.
#[derive(Debug)]
pub struct NavigationHistory {
    entries: VecDeque<NavigationHistoryEntry>,
    index: usize,
    limit: usize,
}

impl Default for NavigationHistory {
    fn default() -> Self {
        Self::new(NAVIGATION_HISTORY_LIMIT)
    }
}

impl NavigationHistory {
    pub fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            index: 0,
            limit: limit.max(1),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn back_count(&self) -> usize {
        self.index
    }

    pub fn forward_count(&self) -> usize {
        self.entries.len().saturating_sub(self.index)
    }

    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.index < self.entries.len()
    }

    /// Localização atual segundo o histórico, inclusive na origem inicial.
    pub fn current_location(&self) -> Option<&NavigationLocation> {
        if self.index == 0 {
            self.entries.front().map(|entry| &entry.origin)
        } else {
            self.entries.get(self.index - 1).map(|entry| &entry.target)
        }
    }

    pub fn back_target(&self) -> Option<&NavigationLocation> {
        if self.index == 0 {
            None
        } else {
            self.entries.get(self.index - 1).map(|entry| &entry.origin)
        }
    }

    pub fn forward_target(&self) -> Option<&NavigationLocation> {
        self.entries.get(self.index).map(|entry| &entry.target)
    }

    /// Confirma um retorno já restaurado com sucesso.
    pub fn commit_back(&mut self) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        true
    }

    /// Confirma um avanço já restaurado com sucesso.
    pub fn commit_forward(&mut self) -> bool {
        if self.index >= self.entries.len() {
            return false;
        }
        self.index += 1;
        true
    }

    /// Registra uma navegação aplicada.
    ///
    /// Uma nova navegação sempre descarta o ramo futuro. Uma repetição que
    /// parte da localização atual e termina nela mesma é ignorada; posições
    /// distintas no mesmo arquivo continuam sendo preservadas. Se o usuário
    /// trocou de aba ou moveu o cursor sem navegar, a origem real difere da
    /// localização do histórico e a nova navegação continua válida.
    pub fn record(&mut self, entry: NavigationHistoryEntry) -> bool {
        let current = self.current_location().cloned();
        self.entries.truncate(self.index);

        if current.is_some_and(|location| {
            location.same_place(&entry.origin) && entry.origin.same_place(&entry.target)
        }) {
            return false;
        }

        if self.entries.len() >= self.limit {
            self.entries.pop_front();
            self.index = self.index.saturating_sub(1);
        }

        self.entries.push_back(entry);
        self.index += 1;
        true
    }

    pub fn entries_mut(&mut self) -> &mut VecDeque<NavigationHistoryEntry> {
        &mut self.entries
    }
}

impl Index<usize> for NavigationHistory {
    type Output = NavigationHistoryEntry;

    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationContextState {
    Current,
    Stale(Vec<NavigationStaleReason>),
    OriginDocumentMissing,
    SessionReplaced,
    ContextMismatch,
}

#[derive(Debug, Default)]
pub struct NavigationOperationRegistry {
    pending: BTreeMap<NavigationOperationId, NavigationOriginContext>,
    completed: VecDeque<NavigationOperationCompletion>,
}

impl NavigationOperationRegistry {
    pub fn register(&mut self, context: NavigationOriginContext) -> bool {
        if self.pending.contains_key(&context.operation_id) {
            return false;
        }
        self.pending.insert(context.operation_id, context);
        true
    }

    pub fn context(&self, operation_id: NavigationOperationId) -> Option<&NavigationOriginContext> {
        self.pending.get(&operation_id)
    }

    pub fn complete(
        &mut self,
        operation_id: NavigationOperationId,
        lsp_context: Option<NavigationRequestContext>,
        context_state: NavigationContextState,
        status: NavigationOperationStatus,
    ) -> bool {
        let Some(origin) = self.pending.remove(&operation_id) else {
            return false;
        };
        self.completed.push_back(NavigationOperationCompletion {
            origin,
            lsp_context,
            context_state,
            status,
        });
        true
    }

    pub fn cancel(&mut self, operation_id: NavigationOperationId) -> bool {
        self.complete(
            operation_id,
            None,
            NavigationContextState::Current,
            NavigationOperationStatus::Cancelled,
        )
    }

    pub fn invalidate_uri(&mut self, uri: &str) -> Vec<NavigationOperationId> {
        let matching: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, context)| context.uri == uri)
            .map(|(id, _)| *id)
            .collect();
        for operation_id in &matching {
            self.complete(
                *operation_id,
                None,
                NavigationContextState::OriginDocumentMissing,
                NavigationOperationStatus::Cancelled,
            );
        }
        matching
    }

    pub fn cancel_all(&mut self) -> Vec<NavigationOperationId> {
        let pending: Vec<_> = self.pending.keys().copied().collect();
        for operation_id in &pending {
            self.cancel(*operation_id);
        }
        pending
    }

    pub fn take_completed(&mut self) -> Vec<NavigationOperationCompletion> {
        self.completed.drain(..).collect()
    }

    /// Retira uma conclusão específica, usada quando a própria interface vai
    /// aplicá-la imediatamente (por exemplo, F12).
    pub fn take_completed_for(
        &mut self,
        operation_id: NavigationOperationId,
    ) -> Option<NavigationOperationCompletion> {
        let index = self
            .completed
            .iter()
            .position(|completion| completion.origin.operation_id == operation_id)?;
        self.completed.remove(index)
    }
}

pub fn outcome_status(outcome: NavigationOutcome) -> NavigationOperationStatus {
    match outcome {
        NavigationOutcome::Result(result) => NavigationOperationStatus::Response { result },
        NavigationOutcome::Unsupported => NavigationOperationStatus::Unsupported,
        NavigationOutcome::Cancelled => NavigationOperationStatus::Cancelled,
        NavigationOutcome::TimedOut { duration } => {
            NavigationOperationStatus::TimedOut { duration }
        }
        NavigationOutcome::SessionEnded => NavigationOperationStatus::SessionEnded,
        NavigationOutcome::Failed { failure } => match failure {
            NavigationFailure::Transport(message) => {
                NavigationOperationStatus::TransportFailure { message }
            }
            NavigationFailure::Protocol(message) => {
                NavigationOperationStatus::ProtocolFailure { message }
            }
            NavigationFailure::Server(message) => {
                NavigationOperationStatus::ServerFailure { message }
            }
            NavigationFailure::Internal(message) => NavigationOperationStatus::Failed { message },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_lsp::protocol::Position;

    fn context(operation_id: u64) -> NavigationOriginContext {
        NavigationOriginContext {
            operation_id: NavigationOperationId(operation_id),
            document_id: DocumentId::new(),
            method: NavigationMethod::Definition,
            workspace: "/workspace".to_owned(),
            language_id: "rust".to_owned(),
            workspace_generation: 1,
            session_id: Some(7),
            uri: "file:///workspace/src/main.rs".to_owned(),
            position: Position {
                line: 2,
                character: 4,
            },
            document_revision: 3,
            location: location(CharOffset(4)),
        }
    }

    fn location(cursor: CharOffset) -> NavigationLocation {
        NavigationLocation {
            document_id: DocumentId::new(),
            uri: "file:///workspace/src/main.rs".to_owned(),
            path: Some(PathBuf::from("/workspace/src/main.rs")),
            cursor,
            anchor: None,
            top_line: 0,
            left_col: 0,
            workspace: Some("/workspace".to_owned()),
            workspace_generation: 1,
            revision: 3,
        }
    }

    fn lsp_context(operation_id: NavigationOperationId) -> NavigationRequestContext {
        NavigationRequestContext {
            operation_id,
            workspace: "/workspace".to_owned(),
            language_id: "rust".to_owned(),
            session_id: Some(7),
            method: NavigationMethod::Definition,
            uri: "file:///workspace/src/main.rs".to_owned(),
            document_version: Some(3),
            position: Position {
                line: 2,
                character: 4,
            },
        }
    }

    #[test]
    fn simultaneous_operations_complete_out_of_order_and_are_consumed_once() {
        let mut registry = NavigationOperationRegistry::default();
        let first = context(1);
        let second = context(2);
        assert!(registry.register(first.clone()));
        assert!(registry.register(second.clone()));

        assert!(registry.complete(
            second.operation_id,
            Some(lsp_context(second.operation_id)),
            NavigationContextState::Current,
            NavigationOperationStatus::Response {
                result: NavigationResult::Empty,
            },
        ));
        assert!(registry.complete(
            first.operation_id,
            Some(lsp_context(first.operation_id)),
            NavigationContextState::Stale(vec![NavigationStaleReason::CursorMoved]),
            NavigationOperationStatus::TimedOut {
                duration: Duration::from_secs(1),
            },
        ));

        let completions = registry.take_completed();
        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].origin.operation_id, second.operation_id);
        assert_eq!(
            completions[0].status,
            NavigationOperationStatus::Response {
                result: NavigationResult::Empty,
            }
        );
        assert_eq!(completions[1].origin.operation_id, first.operation_id);
        assert_eq!(
            completions[1].context_state,
            NavigationContextState::Stale(vec![NavigationStaleReason::CursorMoved])
        );
        assert!(registry.take_completed().is_empty());
        assert!(registry.context(first.operation_id).is_none());
    }

    #[test]
    fn invalidating_a_document_cancels_its_pending_operations_only() {
        let mut registry = NavigationOperationRegistry::default();
        let mut first = context(10);
        first.uri = "file:///workspace/one.rs".to_owned();
        let mut second = context(11);
        second.uri = "file:///workspace/two.rs".to_owned();
        registry.register(first.clone());
        registry.register(second.clone());

        assert_eq!(
            registry.invalidate_uri(&first.uri),
            vec![first.operation_id]
        );
        assert!(registry.context(first.operation_id).is_none());
        assert!(registry.context(second.operation_id).is_some());

        let completion = registry.take_completed().remove(0);
        assert_eq!(
            completion.context_state,
            NavigationContextState::OriginDocumentMissing
        );
        assert_eq!(completion.status, NavigationOperationStatus::Cancelled);
    }

    fn place(uri: &str, cursor: usize) -> NavigationLocation {
        NavigationLocation {
            document_id: DocumentId::new(),
            uri: uri.to_owned(),
            path: Some(PathBuf::from(uri.trim_start_matches("file://"))),
            cursor: CharOffset(cursor),
            anchor: None,
            top_line: 0,
            left_col: 0,
            workspace: Some("/workspace".to_owned()),
            workspace_generation: 1,
            revision: 1,
        }
    }

    fn entry(origin: &NavigationLocation, target: &NavigationLocation) -> NavigationHistoryEntry {
        NavigationHistoryEntry {
            method: NavigationMethod::Definition,
            origin: origin.clone(),
            target: target.clone(),
        }
    }

    #[test]
    fn history_traverses_a_b_c_and_back_forward_do_not_record_entries() {
        let a = place("file:///a.rs", 0);
        let b = place("file:///b.rs", 10);
        let c = place("file:///c.rs", 20);
        let mut history = NavigationHistory::new(10);

        assert!(history.record(entry(&a, &b)));
        assert!(history.record(entry(&b, &c)));
        assert_eq!(history.len(), 2);
        assert_eq!(history.back_count(), 2);
        assert_eq!(history.forward_count(), 0);
        assert_eq!(history.current_location(), Some(&c));

        assert_eq!(history.back_target(), Some(&b));
        assert!(history.commit_back());
        assert_eq!(history.current_location(), Some(&b));
        assert_eq!(history.back_target(), Some(&a));
        assert!(history.commit_back());
        assert_eq!(history.current_location(), Some(&a));
        assert!(!history.can_go_back());
        assert!(!history.commit_back());

        assert_eq!(history.forward_target(), Some(&b));
        assert!(history.commit_forward());
        assert_eq!(history.current_location(), Some(&b));
        assert_eq!(history.forward_target(), Some(&c));
        assert!(history.commit_forward());
        assert_eq!(history.current_location(), Some(&c));
        assert!(!history.can_go_forward());
        assert!(!history.commit_forward());

        assert_eq!(history.len(), 2, "voltar/avançar não cria entradas");
    }

    #[test]
    fn a_new_navigation_after_going_back_discards_forward_entries() {
        let a = place("file:///a.rs", 0);
        let b = place("file:///b.rs", 10);
        let c = place("file:///c.rs", 20);
        let d = place("file:///d.rs", 30);
        let mut history = NavigationHistory::new(10);

        history.record(entry(&a, &b));
        history.record(entry(&b, &c));
        assert!(history.commit_back());
        assert_eq!(history.current_location(), Some(&b));

        assert!(history.record(entry(&b, &d)));
        assert_eq!(history.len(), 2);
        assert_eq!(history.current_location(), Some(&d));
        assert!(!history.can_go_forward());
        assert_eq!(history.back_target(), Some(&b));
        assert!(history.commit_back());
        assert_eq!(history.back_target(), Some(&a));
    }

    #[test]
    fn history_deduplicates_identical_locations_but_keeps_distinct_positions() {
        let a = place("file:///a.rs", 0);
        let b = place("file:///b.rs", 10);
        let b_other_line = place("file:///b.rs", 25);
        let mut history = NavigationHistory::new(10);

        assert!(history.record(entry(&a, &b)));
        assert!(
            !history.record(entry(&b, &b)),
            "navegação para a mesma localização não cria entrada"
        );
        assert_eq!(history.len(), 1);

        assert!(history.record(entry(&b, &b_other_line)));
        assert_eq!(
            history.len(),
            2,
            "posições diferentes no mesmo arquivo contam"
        );
        assert_eq!(history.current_location(), Some(&b_other_line));
    }

    #[test]
    fn history_limit_is_deterministic_and_keeps_the_most_recent_entries() {
        let mut history = NavigationHistory::new(3);
        let places: Vec<_> = (0..6).map(|index| place("file:///a.rs", index)).collect();

        for pair in places.windows(2) {
            assert!(history.record(entry(&pair[0], &pair[1])));
        }

        assert_eq!(history.len(), 3);
        assert_eq!(history.limit(), 3);
        assert_eq!(history.back_count(), 3);
        assert_eq!(history.current_location(), Some(&places[5]));

        assert!(history.commit_back());
        assert_eq!(history.current_location(), Some(&places[4]));
        assert!(history.commit_back());
        assert_eq!(history.current_location(), Some(&places[3]));
        assert!(history.commit_back());
        assert_eq!(history.current_location(), Some(&places[2]));
        assert!(!history.can_go_back());
    }
}
