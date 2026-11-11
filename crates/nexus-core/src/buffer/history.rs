use super::edit::{AppliedEdit, TextRange};

/// Conjunto de edições aplicadas como uma única unidade de histórico.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    edits: Vec<AppliedEdit>,
    revision_before: u64,
    revision_after: u64,
}

impl Transaction {
    pub(crate) fn new(edits: Vec<AppliedEdit>, revision_before: u64, revision_after: u64) -> Self {
        Self {
            edits,
            revision_before,
            revision_after,
        }
    }

    /// Edições na ordem em que foram originalmente aplicadas.
    pub fn edits(&self) -> &[AppliedEdit] {
        &self.edits
    }

    pub fn len(&self) -> usize {
        self.edits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Edições necessárias para desfazer, na ordem inversa à aplicação.
    pub(crate) fn undo_edits(&self) -> impl Iterator<Item = (TextRange, &str)> {
        self.edits
            .iter()
            .rev()
            .map(|edit| (edit.inserted_range(), edit.deleted_text.as_str()))
    }

    /// Edições necessárias para refazer, na ordem original de aplicação.
    pub(crate) fn redo_edits(&self) -> impl Iterator<Item = (TextRange, &str)> {
        self.edits
            .iter()
            .map(|edit| (edit.range, edit.inserted_text.as_str()))
    }

    pub(crate) fn revision_before(&self) -> u64 {
        self.revision_before
    }

    pub(crate) fn revision_after(&self) -> u64 {
        self.revision_after
    }

    pub(crate) fn into_edits(self) -> Vec<AppliedEdit> {
        self.edits
    }
}

#[derive(Debug, Default)]
pub(crate) struct History {
    undo: Vec<Transaction>,
    redo: Vec<Transaction>,
}

impl History {
    pub(crate) fn record(&mut self, transaction: Transaction) {
        self.redo.clear();
        self.undo.push(transaction);
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Indica se a revisão pertence ao ramo atual do histórico.
    ///
    /// Revisões de transações descartadas ao criar um novo ramo não são
    /// consideradas válidas.
    pub(crate) fn contains_revision(&self, revision: u64) -> bool {
        self.undo.iter().chain(self.redo.iter()).any(|transaction| {
            transaction.revision_before() == revision || transaction.revision_after() == revision
        })
    }

    pub(crate) fn pop_undo(&mut self) -> Option<Transaction> {
        self.undo.pop()
    }

    pub(crate) fn pop_redo(&mut self) -> Option<Transaction> {
        self.redo.pop()
    }

    pub(crate) fn push_undo(&mut self, transaction: Transaction) {
        self.undo.push(transaction);
    }

    pub(crate) fn push_redo(&mut self, transaction: Transaction) {
        self.redo.push(transaction);
    }
}
