use ropey::Rope;

pub mod edit;
mod history;
pub mod position;

pub use history::Transaction;

use edit::{AppliedEdit, BufferError, Edit, TextRange};
use history::History;
use position::CharOffset;

#[derive(Debug)]
struct TransactionSession {
    revision_before: u64,
    initial_content: Rope,
    edits: Vec<AppliedEdit>,
}

#[derive(Debug)]
struct RecoveryState {
    original_error: Option<BufferError>,
    recovery_error: BufferError,
    remaining: Vec<AppliedEdit>,
}

#[derive(Debug)]
struct RollbackFailure {
    error: BufferError,
    remaining: Vec<AppliedEdit>,
}

/// Estado de integridade do Buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferIntegrity {
    Consistent,
    RecoveryPending,
}

/// Diagnóstico do estado de recuperação pendente.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    pub original_error: Option<BufferError>,
    pub recovery_error: BufferError,
    pub pending_edits: usize,
}

#[derive(Debug)]
struct RevisionCounter {
    last_allocated: u64,
}

impl RevisionCounter {
    fn allocate(&mut self) -> Result<u64, BufferError> {
        let revision = self
            .last_allocated
            .checked_add(1)
            .ok_or(BufferError::RevisionExhausted)?;

        self.last_allocated = revision;
        Ok(revision)
    }
}

#[derive(Debug)]
pub struct Buffer {
    content: Rope,
    history: History,
    session: Option<TransactionSession>,
    recovery: Option<RecoveryState>,
    revisions: RevisionCounter,
    revision: u64,
    saved_revision: u64,
    #[cfg(any(test, feature = "test-support"))]
    apply_failure_after: Option<usize>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    pub fn new() -> Self {
        Self::from_text("")
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            content: Rope::from_str(text),
            history: History::default(),
            session: None,
            recovery: None,
            revisions: RevisionCounter { last_allocated: 0 },
            revision: 0,
            saved_revision: 0,
            #[cfg(any(test, feature = "test-support"))]
            apply_failure_after: None,
        }
    }

    /// Retorna o conteúdo atual se o Buffer estiver íntegro.
    pub fn text(&self) -> Result<String, BufferError> {
        self.ensure_consistent()?;
        Ok(self.content.to_string())
    }

    pub fn len_chars(&self) -> Result<usize, BufferError> {
        self.ensure_consistent()?;
        Ok(self.content.len_chars())
    }

    pub fn len_lines(&self) -> Result<usize, BufferError> {
        self.ensure_consistent()?;
        Ok(self.content.len_lines())
    }

    pub fn is_empty(&self) -> Result<bool, BufferError> {
        self.ensure_consistent()?;
        Ok(self.content.len_chars() == 0)
    }

    pub fn char_at(&self, offset: CharOffset) -> Result<Option<char>, BufferError> {
        self.ensure_consistent()?;
        Ok(self.content.get_char(offset.0))
    }

    /// Retorna a quantidade de linhas do documento.
    ///
    /// Segue a semântica do Ropey: um documento vazio possui uma linha vazia e
    /// um documento terminado em quebra de linha possui uma linha vazia final.
    pub fn line_count(&self) -> Result<usize, BufferError> {
        self.len_lines()
    }

    /// Retorna o offset do primeiro caractere da linha.
    pub fn line_start(&self, line: usize) -> Result<CharOffset, BufferError> {
        self.ensure_consistent()?;

        let lines = self.content.len_lines();
        if line >= lines {
            return Err(BufferError::OutOfBounds {
                offset: line,
                len: lines,
            });
        }

        Ok(CharOffset(self.content.line_to_char(line)))
    }

    /// Retorna o offset imediatamente após o último caractere visível da linha.
    ///
    /// A quebra de linha (`\n` ou `\r\n`) não faz parte do intervalo retornado.
    pub fn line_end(&self, line: usize) -> Result<CharOffset, BufferError> {
        self.ensure_consistent()?;

        let lines = self.content.len_lines();
        if line >= lines {
            return Err(BufferError::OutOfBounds {
                offset: line,
                len: lines,
            });
        }

        let start = self.content.line_to_char(line);
        let text = self.content.line(line);
        let mut len = text.len_chars();

        if len > 0 && text.char(len - 1) == '\n' {
            len -= 1;
            if len > 0 && text.char(len - 1) == '\r' {
                len -= 1;
            }
        }

        Ok(CharOffset(start + len))
    }

    /// Retorna o texto da linha sem a quebra de linha final.
    pub fn line_text(&self, line: usize) -> Result<String, BufferError> {
        self.ensure_consistent()?;

        let lines = self.content.len_lines();
        if line >= lines {
            return Err(BufferError::OutOfBounds {
                offset: line,
                len: lines,
            });
        }

        let mut text = self.content.line(line).to_string();

        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }

        Ok(text)
    }

    /// Retorna o índice da linha que contém o offset informado.
    ///
    /// O offset pode ser igual ao comprimento total do documento; nesse caso
    /// ele pertence à última linha (a linha vazia final, quando existir).
    pub fn line_of_char(&self, offset: CharOffset) -> Result<usize, BufferError> {
        self.ensure_consistent()?;

        let len = self.content.len_chars();
        if offset.0 > len {
            return Err(BufferError::OutOfBounds {
                offset: offset.0,
                len,
            });
        }

        Ok(self.content.char_to_line(offset.0))
    }

    /// Retorna o texto do intervalo semiaberto `[start, end)`.
    pub fn slice(&self, range: TextRange) -> Result<String, BufferError> {
        self.ensure_consistent()?;

        let start = range.start.0;
        let end = range.end.0;
        let len = self.content.len_chars();

        if start > end {
            return Err(BufferError::InvalidRange { start, end });
        }

        if end > len {
            return Err(BufferError::OutOfBounds { offset: end, len });
        }

        Ok(self.content.slice(start..end).to_string())
    }

    /// Retorna o estado de integridade do Buffer.
    pub fn integrity(&self) -> BufferIntegrity {
        if self.recovery.is_some() {
            BufferIntegrity::RecoveryPending
        } else {
            BufferIntegrity::Consistent
        }
    }

    pub fn is_consistent(&self) -> bool {
        self.recovery.is_none()
    }

    /// Retorna diagnóstico da recuperação pendente, se existir.
    pub fn recovery_report(&self) -> Option<RecoveryReport> {
        self.recovery.as_ref().map(|recovery| RecoveryReport {
            original_error: recovery.original_error.clone(),
            recovery_error: recovery.recovery_error.clone(),
            pending_edits: recovery.remaining.len(),
        })
    }

    /// Tenta concluir uma recuperação pendente usando as edições preservadas.
    ///
    /// Se o Buffer já estiver íntegro, retorna `Ok(())`. Se a nova tentativa
    /// falhar, o estado de recuperação é atualizado e o erro diferencia a falha
    /// original da nova falha de recuperação.
    pub fn retry_recovery(&mut self) -> Result<(), BufferError> {
        let Some(recovery) = self.recovery.take() else {
            return Ok(());
        };

        match self.rollback(&recovery.remaining) {
            Ok(()) => Ok(()),
            Err(failure) => {
                let original = recovery.original_error;
                let error = BufferError::RecoveryFailed {
                    original: original.clone().map(Box::new),
                    recovery: Box::new(failure.error.clone()),
                };
                self.enter_recovery(original, failure.error, failure.remaining);
                Err(error)
            }
        }
    }

    /// Inicia uma sessão de transação explícita.
    ///
    /// Durante a sessão, as edições são aplicadas imediatamente ao documento e
    /// acumuladas para formar uma única transação no commit. Os offsets
    /// informados em cada edição são relativos ao estado atual do documento,
    /// incluindo as edições anteriores da própria sessão.
    ///
    /// Sessões aninhadas são rejeitadas com
    /// [`BufferError::TransactionAlreadyActive`].
    pub fn begin_transaction(&mut self) -> Result<(), BufferError> {
        self.ensure_consistent()?;

        if self.session.is_some() {
            return Err(BufferError::TransactionAlreadyActive);
        }

        self.session = Some(TransactionSession {
            revision_before: self.revision,
            initial_content: self.content.clone(),
            edits: Vec::new(),
        });

        Ok(())
    }

    /// Confirma a sessão ativa como uma única transação de histórico.
    ///
    /// Uma sessão vazia ou sem alterações reais retorna `Ok(None)` e não cria
    /// histórico nem revisão. Uma sessão cujo conteúdo final é idêntico ao
    /// conteúdo inicial também é descartada como sem efeito líquido.
    ///
    /// A comparação de efeito líquido usa o `Rope` inicial guardado pela
    /// sessão. O clone do `Rope` é O(1) por compartilhamento estrutural, mas a
    /// comparação final é O(n) e acontece apenas no commit, nunca em
    /// [`Buffer::is_modified`].
    ///
    /// Se não houver sessão ativa, retorna
    /// [`BufferError::NoActiveTransaction`]. Se não for possível obter um novo
    /// identificador de revisão, todas as edições da sessão são revertidas e a
    /// sessão é encerrada.
    pub fn commit_transaction(&mut self) -> Result<Option<Transaction>, BufferError> {
        self.ensure_consistent()?;

        let Some(session) = self.session.take() else {
            return Err(BufferError::NoActiveTransaction);
        };

        if session.edits.is_empty() {
            return Ok(None);
        }

        if session.edits.len() > 1 && self.content == session.initial_content {
            return Ok(None);
        }

        let revision_after = match self.revisions.allocate() {
            Ok(revision) => revision,
            Err(error) => {
                return Err(self.rollback_after_failure(error, &session.edits));
            }
        };

        self.revision = revision_after;

        let transaction = Transaction::new(session.edits, session.revision_before, revision_after);
        self.history.record(transaction.clone());

        Ok(Some(transaction))
    }

    /// Cancela a sessão ativa, revertendo as edições em ordem inversa.
    ///
    /// O cancelamento não cria histórico nem incrementa a revisão. Se a
    /// reversão falhar, o Buffer entra em recuperação pendente e o erro
    /// [`BufferError::RecoveryFailed`] é retornado. Se não houver sessão ativa,
    /// retorna [`BufferError::NoActiveTransaction`].
    pub fn cancel_transaction(&mut self) -> Result<(), BufferError> {
        self.ensure_consistent()?;

        let Some(session) = self.session.take() else {
            return Err(BufferError::NoActiveTransaction);
        };

        match self.rollback(&session.edits) {
            Ok(()) => Ok(()),
            Err(failure) => {
                let error = BufferError::RecoveryFailed {
                    original: None,
                    recovery: Box::new(failure.error.clone()),
                };
                self.enter_recovery(None, failure.error, failure.remaining);
                Err(error)
            }
        }
    }

    /// Indica se existe uma sessão explícita aberta.
    pub fn has_active_transaction(&self) -> bool {
        self.session.is_some()
    }

    /// Aplica todas as edições como uma única transação.
    ///
    /// Se alguma edição falhar, as edições já aplicadas são revertidas antes de
    /// o erro ser retornado. Uma transação sem alterações reais não gera
    /// histórico.
    ///
    /// Este método é rejeitado com
    /// [`BufferError::TransactionAlreadyActive`] enquanto uma sessão explícita
    /// estiver aberta. Nesse caso, nenhuma edição é aplicada.
    pub fn apply_transaction(
        &mut self,
        edits: impl IntoIterator<Item = Edit>,
    ) -> Result<Option<Transaction>, BufferError> {
        self.ensure_consistent()?;

        if self.session.is_some() {
            return Err(BufferError::TransactionAlreadyActive);
        }

        let edits: Vec<Edit> = edits.into_iter().collect();
        let revision_before = self.revision;
        let applied = self.apply_edits_atomically(&edits)?;

        if applied.is_empty() {
            return Ok(None);
        }

        let revision_after = match self.revisions.allocate() {
            Ok(revision) => revision,
            Err(error) => {
                return Err(self.rollback_after_failure(error, &applied));
            }
        };

        self.revision = revision_after;

        let transaction = Transaction::new(applied, revision_before, revision_after);
        self.history.record(transaction.clone());

        Ok(Some(transaction))
    }

    pub fn apply_edit(&mut self, edit: Edit) -> Result<Option<AppliedEdit>, BufferError> {
        self.ensure_consistent()?;

        if self.session.is_some() {
            return self.apply_edit_in_session(edit);
        }

        let transaction = self.apply_transaction(vec![edit])?;

        Ok(transaction.and_then(|transaction| transaction.into_edits().into_iter().next()))
    }

    pub fn insert(
        &mut self,
        offset: CharOffset,
        text: &str,
    ) -> Result<Option<AppliedEdit>, BufferError> {
        self.apply_edit(Edit::new(TextRange::new(offset, offset), text.to_owned()))
    }

    pub fn delete(&mut self, range: TextRange) -> Result<Option<AppliedEdit>, BufferError> {
        self.apply_edit(Edit::new(range, String::new()))
    }

    pub fn replace(
        &mut self,
        range: TextRange,
        text: &str,
    ) -> Result<Option<AppliedEdit>, BufferError> {
        self.apply_edit(Edit::new(range, text.to_owned()))
    }

    /// Desfaz a última transação.
    ///
    /// Retorna `true` quando uma transação foi desfeita e `false` quando não
    /// havia nada para desfazer. É rejeitado com
    /// [`BufferError::TransactionAlreadyActive`] enquanto uma sessão explícita
    /// estiver aberta.
    pub fn undo(&mut self) -> Result<bool, BufferError> {
        self.ensure_consistent()?;

        if self.session.is_some() {
            return Err(BufferError::TransactionAlreadyActive);
        }

        let Some(transaction) = self.history.pop_undo() else {
            return Ok(false);
        };

        if let Err(error) = self.undo_transaction(&transaction) {
            self.history.push_undo(transaction);
            return Err(error);
        }

        self.revision = transaction.revision_before();
        self.history.push_redo(transaction);

        Ok(true)
    }

    /// Reaplica a última transação desfeita.
    ///
    /// Retorna `true` quando uma transação foi refeita e `false` quando não
    /// havia nada para refazer. É rejeitado com
    /// [`BufferError::TransactionAlreadyActive`] enquanto uma sessão explícita
    /// estiver aberta.
    pub fn redo(&mut self) -> Result<bool, BufferError> {
        self.ensure_consistent()?;

        if self.session.is_some() {
            return Err(BufferError::TransactionAlreadyActive);
        }

        let Some(transaction) = self.history.pop_redo() else {
            return Ok(false);
        };

        if let Err(error) = self.redo_transaction(&transaction) {
            self.history.push_redo(transaction);
            return Err(error);
        }

        self.revision = transaction.revision_after();
        self.history.push_undo(transaction);

        Ok(true)
    }

    pub fn can_undo(&self) -> bool {
        self.is_consistent() && self.session.is_none() && self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.is_consistent() && self.session.is_none() && self.history.can_redo()
    }

    /// Retorna a revisão confirmada atual.
    ///
    /// Durante uma sessão explícita, as edições já aplicadas ao documento ainda
    /// não possuem uma revisão própria; este valor permanece sendo o da revisão
    /// anterior ao início da sessão até o commit.
    ///
    /// Em recuperação pendente, este é apenas o último valor confirmado antes
    /// da falha; ele não garante que o conteúdo atual corresponda a essa
    /// revisão.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Retorna a revisão marcada como último estado salvo confirmado.
    pub fn saved_revision(&self) -> u64 {
        self.saved_revision
    }

    /// Indica se o documento difere do último estado marcado como salvo.
    ///
    /// Edições não confirmadas de uma sessão explícita contam como modificação
    /// enquanto a sessão estiver aberta, pois o efeito líquido ainda não foi
    /// avaliado. No commit, uma sessão cujo conteúdo final seja idêntico ao
    /// inicial é descartada e deixa de contar como modificação.
    ///
    /// Em recuperação pendente retorna `true`: o conteúdo não pode ser
    /// considerado equivalente ao estado salvo.
    pub fn is_modified(&self) -> bool {
        if self.recovery.is_some() {
            return true;
        }

        self.revision != self.saved_revision
            || self
                .session
                .as_ref()
                .is_some_and(|session| !session.edits.is_empty())
    }

    /// Marca o conteúdo atual como não salvo sem criar uma edição.
    ///
    /// É usado quando o conteúdo não veio de uma gravação confirmada, como um
    /// documento restaurado de uma cópia de recuperação. A revisão de conteúdo
    /// permanece a mesma; apenas o último estado salvo deixa de coincidir com
    /// ela. Um [`Buffer::mark_saved`] posterior volta a considerar o documento
    /// limpo.
    pub fn mark_unsaved(&mut self) -> Result<(), BufferError> {
        self.ensure_consistent()?;

        if self.session.is_some() {
            return Err(BufferError::TransactionAlreadyActive);
        }

        if self.saved_revision == self.revision {
            self.saved_revision = self.revision.wrapping_add(1);
        }

        Ok(())
    }

    /// Marca a revisão atual como o último estado salvo confirmado.
    ///
    /// É equivalente a [`Buffer::mark_saved_at`] com a revisão atual. Sessões
    /// explícitas abertas são rejeitadas com
    /// [`BufferError::TransactionAlreadyActive`], pois a revisão atual ainda
    /// representa o último estado confirmado, não as edições não confirmadas.
    pub fn mark_saved(&mut self) -> Result<(), BufferError> {
        self.mark_saved_at(self.revision)
    }

    /// Marca uma revisão específica como o último estado salvo confirmado.
    ///
    /// A revisão deve ser a revisão atual ou uma revisão alcançável pelas
    /// pilhas de Undo/Redo do ramo atual. Revisões desconhecidas ou de ramos
    /// descartados são rejeitadas com [`BufferError::InvalidRevision`].
    ///
    /// A revisão informada é marcada exatamente como recebida. Se o documento
    /// já tiver avançado para outra revisão, o estado salvo continua sendo a
    /// revisão antiga e [`Buffer::is_modified`] permanece `true`.
    ///
    /// Sessões explícitas abertas são rejeitadas, pois ainda não existe uma
    /// revisão confirmada correspondente ao conteúdo atual. Recuperação
    /// pendente também é rejeitada com [`BufferError::RecoveryRequired`].
    pub fn mark_saved_at(&mut self, revision: u64) -> Result<(), BufferError> {
        self.ensure_consistent()?;

        if self.session.is_some() {
            return Err(BufferError::TransactionAlreadyActive);
        }

        if revision != self.revision && !self.history.contains_revision(revision) {
            return Err(BufferError::InvalidRevision { revision });
        }

        self.saved_revision = revision;

        Ok(())
    }

    fn apply_edit_at(
        &mut self,
        range: TextRange,
        inserted_text: &str,
    ) -> Result<Option<AppliedEdit>, BufferError> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(remaining) = self.apply_failure_after {
            if remaining == 0 {
                self.apply_failure_after = None;
                return Err(BufferError::TestFailure);
            }
            self.apply_failure_after = Some(remaining - 1);
        }

        let start = range.start.0;
        let end = range.end.0;
        let len = self.content.len_chars();

        if start > end {
            return Err(BufferError::InvalidRange { start, end });
        }

        if end > len {
            return Err(BufferError::OutOfBounds { offset: end, len });
        }

        let deleted_text = self.content.slice(start..end).to_string();

        if deleted_text == inserted_text {
            return Ok(None);
        }

        if start < end {
            self.content.remove(start..end);
        }

        if !inserted_text.is_empty() {
            self.content.insert(start, inserted_text);
        }

        Ok(Some(AppliedEdit {
            range,
            deleted_text,
            inserted_text: inserted_text.to_owned(),
        }))
    }

    fn apply_edit_in_session(&mut self, edit: Edit) -> Result<Option<AppliedEdit>, BufferError> {
        let mut session = match self.session.take() {
            Some(session) => session,
            None => return Err(BufferError::NoActiveTransaction),
        };

        let applied = self.apply_edit_at(edit.range, &edit.inserted_text);

        if let Ok(Some(applied_edit)) = &applied {
            session.edits.push(applied_edit.clone());
        }

        self.session = Some(session);
        applied
    }

    #[cfg(test)]
    fn set_revision_counter(&mut self, last_allocated: u64) {
        self.revisions.last_allocated = last_allocated;
    }

    #[cfg(any(test, feature = "test-support"))]
    fn set_apply_failure_after(&mut self, calls: usize) {
        self.apply_failure_after = Some(calls);
    }

    /// Coloca o Buffer em recuperação pendente de forma controlada.
    ///
    /// Disponível apenas com a feature `test-support`, usada pelos testes da
    /// TUI. Não faz parte da API de produção.
    #[cfg(feature = "test-support")]
    pub fn force_recovery_pending_for_tests(&mut self) {
        if self.session.is_some() {
            let _ = self.cancel_transaction();
        }

        if self.recovery.is_some() {
            return;
        }

        let start = self.content.len_chars();
        let _ = self.begin_transaction();
        let _ = self.insert(CharOffset(start), "d");
        let _ = self.insert(CharOffset(start + 1), "e");
        self.set_apply_failure_after(1);
        let _ = self.cancel_transaction();
    }

    fn apply_edits_atomically(&mut self, edits: &[Edit]) -> Result<Vec<AppliedEdit>, BufferError> {
        let mut applied = Vec::new();

        for edit in edits {
            match self.apply_edit_at(edit.range, &edit.inserted_text) {
                Ok(Some(applied_edit)) => applied.push(applied_edit),
                Ok(None) => {}
                Err(error) => {
                    return Err(self.rollback_after_failure(error, &applied));
                }
            }
        }

        Ok(applied)
    }

    fn undo_transaction(&mut self, transaction: &Transaction) -> Result<(), BufferError> {
        let mut applied = Vec::new();

        for (range, text) in transaction.undo_edits() {
            match self.apply_edit_at(range, text) {
                Ok(Some(applied_edit)) => applied.push(applied_edit),
                Ok(None) => {}
                Err(error) => {
                    return Err(self.rollback_after_failure(error, &applied));
                }
            }
        }

        Ok(())
    }

    fn redo_transaction(&mut self, transaction: &Transaction) -> Result<(), BufferError> {
        let mut applied = Vec::new();

        for (range, text) in transaction.redo_edits() {
            match self.apply_edit_at(range, text) {
                Ok(Some(applied_edit)) => applied.push(applied_edit),
                Ok(None) => {}
                Err(error) => {
                    return Err(self.rollback_after_failure(error, &applied));
                }
            }
        }

        Ok(())
    }

    fn rollback_after_failure(
        &mut self,
        original_error: BufferError,
        applied: &[AppliedEdit],
    ) -> BufferError {
        match self.rollback(applied) {
            Ok(()) => original_error,
            Err(failure) => {
                let error = BufferError::RecoveryFailed {
                    original: Some(Box::new(original_error.clone())),
                    recovery: Box::new(failure.error.clone()),
                };
                self.enter_recovery(Some(original_error), failure.error, failure.remaining);
                error
            }
        }
    }

    fn rollback(&mut self, applied: &[AppliedEdit]) -> Result<(), RollbackFailure> {
        for index in (0..applied.len()).rev() {
            let edit = &applied[index];

            if let Err(error) = self.apply_edit_at(edit.inserted_range(), &edit.deleted_text) {
                return Err(RollbackFailure {
                    error,
                    remaining: applied[..=index].to_vec(),
                });
            }
        }

        Ok(())
    }

    fn enter_recovery(
        &mut self,
        original_error: Option<BufferError>,
        recovery_error: BufferError,
        remaining: Vec<AppliedEdit>,
    ) {
        self.recovery = Some(RecoveryState {
            original_error,
            recovery_error,
            remaining,
        });
    }

    fn ensure_consistent(&self) -> Result<(), BufferError> {
        if self.recovery.is_some() {
            Err(BufferError::RecoveryRequired)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod read_tests {
    use super::*;

    #[test]
    fn empty_buffer_has_one_empty_line() {
        let buffer = Buffer::new();

        assert_eq!(buffer.line_count().unwrap(), 1);
        assert_eq!(buffer.line_start(0).unwrap(), CharOffset(0));
        assert_eq!(buffer.line_end(0).unwrap(), CharOffset(0));
        assert_eq!(buffer.line_text(0).unwrap(), "");
        assert_eq!(buffer.line_of_char(CharOffset(0)).unwrap(), 0);
    }

    #[test]
    fn reads_lines_without_line_endings() {
        let buffer = Buffer::from_text("alpha\nbeta\ngamma");

        assert_eq!(buffer.line_count().unwrap(), 3);
        assert_eq!(buffer.line_text(0).unwrap(), "alpha");
        assert_eq!(buffer.line_text(1).unwrap(), "beta");
        assert_eq!(buffer.line_text(2).unwrap(), "gamma");
        assert_eq!(buffer.line_start(1).unwrap(), CharOffset(6));
        assert_eq!(buffer.line_end(1).unwrap(), CharOffset(10));
    }

    #[test]
    fn trailing_newline_creates_empty_last_line() {
        let buffer = Buffer::from_text("alpha\n");

        assert_eq!(buffer.line_count().unwrap(), 2);
        assert_eq!(buffer.line_text(0).unwrap(), "alpha");
        assert_eq!(buffer.line_text(1).unwrap(), "");
        assert_eq!(buffer.line_start(1).unwrap(), CharOffset(6));
        assert_eq!(buffer.line_end(1).unwrap(), CharOffset(6));
    }

    #[test]
    fn handles_crlf_without_exposing_carriage_return() {
        let buffer = Buffer::from_text("alpha\r\nbeta\r\n");

        assert_eq!(buffer.line_count().unwrap(), 3);
        assert_eq!(buffer.line_text(0).unwrap(), "alpha");
        assert_eq!(buffer.line_text(1).unwrap(), "beta");
        assert_eq!(buffer.line_text(2).unwrap(), "");
        assert_eq!(buffer.line_start(1).unwrap(), CharOffset(7));
        assert_eq!(buffer.line_end(1).unwrap(), CharOffset(11));
    }

    #[test]
    fn handles_unicode_offsets() {
        let buffer = Buffer::from_text("café\n界");

        assert_eq!(buffer.line_start(0).unwrap(), CharOffset(0));
        assert_eq!(buffer.line_end(0).unwrap(), CharOffset(4));
        assert_eq!(buffer.line_start(1).unwrap(), CharOffset(5));
        assert_eq!(buffer.line_end(1).unwrap(), CharOffset(6));
        assert_eq!(buffer.line_text(1).unwrap(), "界");
        assert_eq!(buffer.line_of_char(CharOffset(5)).unwrap(), 1);
    }

    #[test]
    fn slices_ranges_by_char_offsets() {
        let buffer = Buffer::from_text("café\n界");

        assert_eq!(
            buffer
                .slice(TextRange::new(CharOffset(1), CharOffset(4)))
                .unwrap(),
            "afé"
        );
        assert_eq!(
            buffer
                .slice(TextRange::new(CharOffset(4), CharOffset(5)))
                .unwrap(),
            "\n"
        );
    }

    #[test]
    fn localized_reads_reject_invalid_positions() {
        let buffer = Buffer::from_text("abc");

        assert!(matches!(
            buffer.line_start(1),
            Err(BufferError::OutOfBounds { offset: 1, len: 1 })
        ));
        assert!(matches!(
            buffer.line_end(1),
            Err(BufferError::OutOfBounds { offset: 1, len: 1 })
        ));
        assert!(matches!(
            buffer.line_text(1),
            Err(BufferError::OutOfBounds { offset: 1, len: 1 })
        ));
        assert!(matches!(
            buffer.line_of_char(CharOffset(4)),
            Err(BufferError::OutOfBounds { offset: 4, len: 3 })
        ));
        assert!(matches!(
            buffer.slice(TextRange::new(CharOffset(2), CharOffset(1))),
            Err(BufferError::InvalidRange { start: 2, end: 1 })
        ));
        assert!(matches!(
            buffer.slice(TextRange::new(CharOffset(0), CharOffset(4))),
            Err(BufferError::OutOfBounds { offset: 4, len: 3 })
        ));
    }

    #[test]
    fn localized_reads_require_consistent_buffer() {
        let mut buffer = Buffer::from_text("abc");
        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(3), "d").unwrap();
        buffer.insert(CharOffset(4), "e").unwrap();
        buffer.set_apply_failure_after(1);

        assert!(matches!(
            buffer.cancel_transaction(),
            Err(BufferError::RecoveryFailed { .. })
        ));

        assert!(matches!(
            buffer.line_count(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.line_start(0),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.line_end(0),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.line_text(0),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.line_of_char(CharOffset(0)),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.slice(TextRange::new(CharOffset(0), CharOffset(1))),
            Err(BufferError::RecoveryRequired)
        ));
    }
}

#[cfg(test)]
mod buffer_tests {
    use super::*;

    #[test]
    fn new_buffer_is_empty() {
        let buffer = Buffer::new();

        assert_eq!(buffer.text().unwrap(), "");
        assert_eq!(buffer.len_chars().unwrap(), 0);
        assert!(buffer.is_empty().unwrap());
    }

    #[test]
    fn from_text_stores_content() {
        let buffer = Buffer::from_text("Nexus");

        assert_eq!(buffer.text().unwrap(), "Nexus");
        assert!(!buffer.is_empty().unwrap());
    }

    #[test]
    fn reports_lengths() {
        let buffer = Buffer::from_text("a\nbc");

        assert_eq!(buffer.len_chars().unwrap(), 4);
        assert_eq!(buffer.len_lines().unwrap(), 2);
    }

    #[test]
    fn reads_chars_by_offset() {
        let buffer = Buffer::from_text("café");

        assert_eq!(buffer.char_at(CharOffset(3)).unwrap(), Some('é'));
        assert_eq!(buffer.char_at(CharOffset(4)).unwrap(), None);
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    #[test]
    fn inserts_text() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.insert(CharOffset(5), " IDE").unwrap();

        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
    }

    #[test]
    fn deletes_text() {
        let mut buffer = Buffer::from_text("Nexus IDE");

        buffer
            .delete(TextRange::new(CharOffset(0), CharOffset(6)))
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "IDE");
    }

    #[test]
    fn replaces_text() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(5)), "Nexus IDE")
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
    }

    #[test]
    fn handles_unicode_offsets() {
        let mut buffer = Buffer::from_text("café");

        buffer.insert(CharOffset(4), "!").unwrap();

        assert_eq!(buffer.text().unwrap(), "café!");
    }

    #[test]
    fn rejects_invalid_range_without_modifying_buffer() {
        let mut buffer = Buffer::from_text("Nexus");
        let original = buffer.text().unwrap();

        let result = buffer.delete(TextRange::new(CharOffset(4), CharOffset(2)));

        assert!(matches!(result, Err(BufferError::InvalidRange { .. })));

        assert_eq!(buffer.text().unwrap(), original);
    }

    #[test]
    fn rejects_out_of_bounds_without_modifying_buffer() {
        let mut buffer = Buffer::from_text("Nexus");
        let original = buffer.text().unwrap();

        let result = buffer.insert(CharOffset(10), "!");

        assert!(matches!(result, Err(BufferError::OutOfBounds { .. })));

        assert_eq!(buffer.text().unwrap(), original);
    }

    #[test]
    fn empty_edit_does_not_modify_buffer() {
        let mut buffer = Buffer::from_text("Nexus");

        let result = buffer.insert(CharOffset(2), "").unwrap();

        assert!(result.is_none());
        assert_eq!(buffer.text().unwrap(), "Nexus");
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    #[test]
    fn undo_insertion() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.insert(CharOffset(5), " IDE").unwrap();

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Nexus");
        assert!(buffer.can_redo());
    }

    #[test]
    fn undo_deletion() {
        let mut buffer = Buffer::from_text("Nexus IDE");

        buffer
            .delete(TextRange::new(CharOffset(0), CharOffset(6)))
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "IDE");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
    }

    #[test]
    fn undo_replacement() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(5)), "Nexus IDE")
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Nexus");
    }

    #[test]
    fn redo_insertion() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.insert(CharOffset(5), " IDE").unwrap();
        assert!(buffer.undo().unwrap());

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
    }

    #[test]
    fn redo_deletion() {
        let mut buffer = Buffer::from_text("Nexus IDE");

        buffer
            .delete(TextRange::new(CharOffset(0), CharOffset(6)))
            .unwrap();
        assert!(buffer.undo().unwrap());

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "IDE");
    }

    #[test]
    fn redo_replacement() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(5)), "Nexus IDE")
            .unwrap();
        assert!(buffer.undo().unwrap());

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
    }

    #[test]
    fn undo_transaction_with_multiple_edits() {
        let mut buffer = Buffer::from_text("");

        let transaction = buffer
            .apply_transaction(vec![
                Edit::new(
                    TextRange::new(CharOffset(0), CharOffset(0)),
                    "Hello".to_owned(),
                ),
                Edit::new(
                    TextRange::new(CharOffset(5), CharOffset(5)),
                    " Nexus".to_owned(),
                ),
            ])
            .unwrap()
            .unwrap();

        assert_eq!(transaction.len(), 2);
        assert_eq!(buffer.text().unwrap(), "Hello Nexus");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "");
    }

    #[test]
    fn redo_transaction_with_multiple_edits() {
        let mut buffer = Buffer::from_text("");

        buffer
            .apply_transaction(vec![
                Edit::new(
                    TextRange::new(CharOffset(0), CharOffset(0)),
                    "Hello".to_owned(),
                ),
                Edit::new(
                    TextRange::new(CharOffset(5), CharOffset(5)),
                    " Nexus".to_owned(),
                ),
            ])
            .unwrap();

        assert!(buffer.undo().unwrap());
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Hello Nexus");
    }

    #[test]
    fn undo_applies_edits_in_reverse_order() {
        let mut buffer = Buffer::from_text("abc");

        buffer
            .apply_transaction(vec![
                Edit::new(
                    TextRange::new(CharOffset(0), CharOffset(1)),
                    "XY".to_owned(),
                ),
                Edit::new(TextRange::new(CharOffset(1), CharOffset(2)), "Z".to_owned()),
            ])
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "XZbc");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "abc");

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "XZbc");
    }

    #[test]
    fn new_edit_invalidates_redo_history() {
        let mut buffer = Buffer::from_text("");

        buffer.insert(CharOffset(0), "a").unwrap();
        assert!(buffer.undo().unwrap());
        assert!(buffer.can_redo());

        buffer.insert(CharOffset(0), "b").unwrap();

        assert!(!buffer.can_redo());
        assert!(!buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "b");
    }

    #[test]
    fn empty_transaction_does_not_create_history() {
        let mut buffer = Buffer::from_text("Nexus");

        let result = buffer.apply_transaction(Vec::new()).unwrap();

        assert!(result.is_none());
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
        assert_eq!(buffer.revision(), 0);
    }

    #[test]
    fn edit_without_real_change_does_not_create_history() {
        let mut buffer = Buffer::from_text("Nexus");

        let inserted = buffer.insert(CharOffset(2), "").unwrap();
        let replaced = buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(5)), "Nexus")
            .unwrap();

        assert!(inserted.is_none());
        assert!(replaced.is_none());
        assert_eq!(buffer.text().unwrap(), "Nexus");
        assert!(!buffer.can_undo());
        assert_eq!(buffer.revision(), 0);
    }

    #[test]
    fn failed_transaction_restores_document_and_history() {
        let mut buffer = Buffer::from_text("Nexus");
        let original = buffer.text().unwrap();

        let result = buffer.apply_transaction(vec![
            Edit::new(
                TextRange::new(CharOffset(5), CharOffset(5)),
                " IDE".to_owned(),
            ),
            Edit::new(
                TextRange::new(CharOffset(100), CharOffset(100)),
                "!".to_owned(),
            ),
        ]);

        assert!(matches!(result, Err(BufferError::OutOfBounds { .. })));
        assert_eq!(buffer.text().unwrap(), original);
        assert_eq!(buffer.len_chars().unwrap(), 5);
        assert!(!buffer.can_undo());
        assert_eq!(buffer.revision(), 0);
    }

    #[test]
    fn revisions_track_modification_state() {
        let mut buffer = Buffer::from_text("Nexus");

        assert_eq!(buffer.revision(), 0);
        assert_eq!(buffer.saved_revision(), 0);
        assert!(!buffer.is_modified());

        buffer.insert(CharOffset(5), " IDE").unwrap();

        assert_eq!(buffer.revision(), 1);
        assert!(buffer.is_modified());

        buffer.mark_saved().unwrap();

        assert_eq!(buffer.saved_revision(), 1);
        assert!(!buffer.is_modified());

        buffer.insert(CharOffset(9), "!").unwrap();

        assert_eq!(buffer.revision(), 2);
        assert!(buffer.is_modified());

        assert!(buffer.undo().unwrap());

        assert_eq!(buffer.revision(), 1);
        assert!(!buffer.is_modified());

        assert!(buffer.redo().unwrap());

        assert_eq!(buffer.revision(), 2);
        assert!(buffer.is_modified());
    }

    #[test]
    fn undo_and_redo_with_unicode() {
        let mut buffer = Buffer::from_text("café");

        buffer.insert(CharOffset(4), " ☕").unwrap();
        assert_eq!(buffer.text().unwrap(), "café ☕");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "café");

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "café ☕");

        buffer
            .replace(TextRange::new(CharOffset(3), CharOffset(4)), "e")
            .unwrap();
        assert_eq!(buffer.text().unwrap(), "cafe ☕");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "café ☕");

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "cafe ☕");
    }

    #[test]
    fn undo_and_redo_with_multiple_lines() {
        let original = "linha 1\nlinha 2\nlinha 3";
        let mut buffer = Buffer::from_text(original);

        buffer
            .delete(TextRange::new(CharOffset(7), CharOffset(8)))
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "linha 1linha 2\nlinha 3");
        assert_eq!(buffer.len_lines().unwrap(), 2);

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), original);
        assert_eq!(buffer.len_lines().unwrap(), 3);

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "linha 1linha 2\nlinha 3");
        assert_eq!(buffer.len_lines().unwrap(), 2);
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[test]
    fn begins_and_commits_session() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.begin_transaction().unwrap();
        assert!(buffer.has_active_transaction());

        buffer.insert(CharOffset(5), " IDE").unwrap();

        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
        assert_eq!(buffer.revision(), 0);
        assert!(buffer.is_modified());
        assert!(!buffer.can_undo());

        let transaction = buffer.commit_transaction().unwrap().unwrap();

        assert_eq!(transaction.len(), 1);
        assert!(!buffer.has_active_transaction());
        assert_eq!(buffer.revision(), 1);
        assert_eq!(buffer.text().unwrap(), "Nexus IDE");
        assert!(buffer.can_undo());
    }

    #[test]
    fn multiple_insertions_form_a_single_transaction() {
        let mut buffer = Buffer::from_text("");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(0), "Hello").unwrap();
        buffer.insert(CharOffset(5), " World").unwrap();
        buffer.insert(CharOffset(11), "!").unwrap();

        let transaction = buffer.commit_transaction().unwrap().unwrap();

        assert_eq!(transaction.len(), 3);
        assert_eq!(buffer.text().unwrap(), "Hello World!");
        assert_eq!(buffer.revision(), 1);
    }

    #[test]
    fn undo_reverts_the_entire_session() {
        let mut buffer = Buffer::from_text("");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(0), "Hello").unwrap();
        buffer.insert(CharOffset(5), " World").unwrap();
        buffer.commit_transaction().unwrap();

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "");
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.can_undo());
        assert!(buffer.can_redo());
    }

    #[test]
    fn redo_reapplies_the_entire_session() {
        let mut buffer = Buffer::from_text("");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(0), "Hello").unwrap();
        buffer.insert(CharOffset(5), " World").unwrap();
        buffer.commit_transaction().unwrap();
        assert!(buffer.undo().unwrap());

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Hello World");
        assert_eq!(buffer.revision(), 1);
    }

    #[test]
    fn cancel_reverts_session_without_history_or_revision() {
        let original = "Nexus";
        let mut buffer = Buffer::from_text(original);

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(5), " IDE").unwrap();
        buffer
            .delete(TextRange::new(CharOffset(0), CharOffset(5)))
            .unwrap();
        assert_eq!(buffer.text().unwrap(), " IDE");

        buffer.cancel_transaction().unwrap();

        assert_eq!(buffer.text().unwrap(), original);
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.is_modified());
        assert!(!buffer.has_active_transaction());
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
    }

    #[test]
    fn empty_session_does_not_create_history_or_revision() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.begin_transaction().unwrap();
        let result = buffer.commit_transaction().unwrap();

        assert!(result.is_none());
        assert!(!buffer.has_active_transaction());
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.can_undo());
        assert!(!buffer.is_modified());
    }

    #[test]
    fn session_without_real_changes_does_not_create_history() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.begin_transaction().unwrap();
        let inserted = buffer.insert(CharOffset(2), "").unwrap();
        let replaced = buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(5)), "Nexus")
            .unwrap();
        let result = buffer.commit_transaction().unwrap();

        assert!(inserted.is_none());
        assert!(replaced.is_none());
        assert!(result.is_none());
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.can_undo());
    }

    #[test]
    fn nested_session_is_rejected() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.begin_transaction().unwrap();

        assert!(matches!(
            buffer.begin_transaction(),
            Err(BufferError::TransactionAlreadyActive)
        ));
        assert!(buffer.has_active_transaction());

        buffer.cancel_transaction().unwrap();
    }

    #[test]
    fn undo_during_session_is_rejected() {
        let mut buffer = Buffer::from_text("Nexus");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(5), " IDE").unwrap();

        assert!(matches!(
            buffer.undo(),
            Err(BufferError::TransactionAlreadyActive)
        ));
        assert!(!buffer.can_undo());
        assert_eq!(buffer.text().unwrap(), "Nexus IDE");

        buffer.cancel_transaction().unwrap();
        assert_eq!(buffer.text().unwrap(), "Nexus");
    }

    #[test]
    fn redo_during_session_is_rejected() {
        let mut buffer = Buffer::from_text("");

        buffer.insert(CharOffset(0), "a").unwrap();
        assert!(buffer.undo().unwrap());
        assert!(buffer.can_redo());

        buffer.begin_transaction().unwrap();
        assert!(!buffer.can_redo());
        assert!(matches!(
            buffer.redo(),
            Err(BufferError::TransactionAlreadyActive)
        ));

        buffer.cancel_transaction().unwrap();

        assert!(buffer.can_redo());
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "a");
    }

    #[test]
    fn apply_transaction_during_session_is_rejected() {
        let mut buffer = Buffer::from_text("");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(0), "a").unwrap();

        let result = buffer.apply_transaction(vec![Edit::new(
            TextRange::new(CharOffset(1), CharOffset(1)),
            "b".to_owned(),
        )]);

        assert!(matches!(result, Err(BufferError::TransactionAlreadyActive)));
        assert_eq!(buffer.text().unwrap(), "a");
        assert!(buffer.has_active_transaction());

        let transaction = buffer.commit_transaction().unwrap().unwrap();

        assert_eq!(transaction.len(), 1);
        assert_eq!(buffer.text().unwrap(), "a");
    }

    #[test]
    fn failed_edit_leaves_session_consistent() {
        let mut buffer = Buffer::from_text("");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(0), "Hello").unwrap();

        let result = buffer.delete(TextRange::new(CharOffset(0), CharOffset(100)));

        assert!(matches!(result, Err(BufferError::OutOfBounds { .. })));
        assert_eq!(buffer.text().unwrap(), "Hello");
        assert!(buffer.has_active_transaction());
        assert_eq!(buffer.revision(), 0);

        buffer.insert(CharOffset(5), "!").unwrap();
        let transaction = buffer.commit_transaction().unwrap().unwrap();

        assert_eq!(transaction.len(), 2);
        assert_eq!(buffer.text().unwrap(), "Hello!");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "");
    }

    #[test]
    fn deletions_and_replacements_inside_session() {
        let original = "Nexus IDE";
        let mut buffer = Buffer::from_text(original);

        buffer.begin_transaction().unwrap();
        buffer
            .delete(TextRange::new(CharOffset(5), CharOffset(6)))
            .unwrap();
        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(5)), "Nexus Core")
            .unwrap();
        buffer.insert(CharOffset(10), " ").unwrap();

        assert_eq!(buffer.text().unwrap(), "Nexus Core IDE");

        buffer.commit_transaction().unwrap();

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), original);

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Nexus Core IDE");
    }

    #[test]
    fn unicode_and_multiple_lines_inside_session() {
        let original = "café\nlinha";
        let mut buffer = Buffer::from_text(original);

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(4), " ☕").unwrap();
        buffer
            .delete(TextRange::new(CharOffset(6), CharOffset(7)))
            .unwrap();
        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(4)), "CAFÉ")
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "CAFÉ ☕linha");
        assert_eq!(buffer.len_lines().unwrap(), 1);

        buffer.commit_transaction().unwrap();

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), original);
        assert_eq!(buffer.len_lines().unwrap(), 2);

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "CAFÉ ☕linha");
        assert_eq!(buffer.len_lines().unwrap(), 1);
    }

    #[test]
    fn committed_session_invalidates_redo_history() {
        let mut buffer = Buffer::from_text("");

        buffer.insert(CharOffset(0), "a").unwrap();
        assert!(buffer.undo().unwrap());
        assert!(buffer.can_redo());

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(0), "b").unwrap();
        buffer.commit_transaction().unwrap();

        assert!(!buffer.can_redo());
        assert!(!buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "b");
    }

    #[test]
    fn mark_old_revision_as_saved_after_advancing() {
        let mut buffer = Buffer::from_text("a");

        buffer.insert(CharOffset(1), "b").unwrap();
        buffer.insert(CharOffset(2), "c").unwrap();

        assert_eq!(buffer.revision(), 2);

        buffer.mark_saved_at(1).unwrap();

        assert_eq!(buffer.saved_revision(), 1);
        assert_eq!(buffer.revision(), 2);
        assert!(buffer.is_modified());
    }

    #[test]
    fn undo_returns_to_saved_revision() {
        let mut buffer = Buffer::from_text("a");

        buffer.insert(CharOffset(1), "b").unwrap();
        buffer.mark_saved_at(1).unwrap();
        buffer.insert(CharOffset(2), "c").unwrap();

        assert!(buffer.is_modified());

        assert!(buffer.undo().unwrap());

        assert_eq!(buffer.revision(), 1);
        assert_eq!(buffer.saved_revision(), 1);
        assert!(!buffer.is_modified());
        assert_eq!(buffer.text().unwrap(), "ab");
    }

    #[test]
    fn redo_leaves_saved_revision() {
        let mut buffer = Buffer::from_text("a");

        buffer.insert(CharOffset(1), "b").unwrap();
        buffer.mark_saved_at(1).unwrap();
        buffer.insert(CharOffset(2), "c").unwrap();
        assert!(buffer.undo().unwrap());
        assert!(!buffer.is_modified());

        assert!(buffer.redo().unwrap());

        assert_eq!(buffer.revision(), 2);
        assert_eq!(buffer.saved_revision(), 1);
        assert!(buffer.is_modified());
        assert_eq!(buffer.text().unwrap(), "abc");
    }

    #[test]
    fn mark_saved_at_rejects_unknown_revision() {
        let mut buffer = Buffer::from_text("");

        buffer.insert(CharOffset(0), "a").unwrap();

        assert!(matches!(
            buffer.mark_saved_at(999),
            Err(BufferError::InvalidRevision { revision: 999 })
        ));
        assert_eq!(buffer.saved_revision(), 0);
    }

    #[test]
    fn mark_saved_at_rejects_discarded_branch_revision() {
        let mut buffer = Buffer::from_text("");

        buffer.insert(CharOffset(0), "a").unwrap();
        assert!(buffer.undo().unwrap());
        buffer.insert(CharOffset(0), "b").unwrap();

        assert!(matches!(
            buffer.mark_saved_at(1),
            Err(BufferError::InvalidRevision { revision: 1 })
        ));
    }

    #[test]
    fn marking_saved_during_session_is_rejected() {
        let mut buffer = Buffer::from_text("a");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(1), "b").unwrap();

        assert!(buffer.is_modified());
        assert!(matches!(
            buffer.mark_saved(),
            Err(BufferError::TransactionAlreadyActive)
        ));
        assert!(matches!(
            buffer.mark_saved_at(0),
            Err(BufferError::TransactionAlreadyActive)
        ));

        buffer.cancel_transaction().unwrap();
        assert!(!buffer.is_modified());
    }

    #[test]
    fn commit_and_cancel_without_session_are_rejected() {
        let mut buffer = Buffer::from_text("Nexus");

        assert!(matches!(
            buffer.commit_transaction(),
            Err(BufferError::NoActiveTransaction)
        ));
        assert!(matches!(
            buffer.cancel_transaction(),
            Err(BufferError::NoActiveTransaction)
        ));
    }

    #[test]
    fn revision_exhaustion_rolls_back_single_edit() {
        let mut buffer = Buffer::from_text("abc");
        buffer.set_revision_counter(u64::MAX);

        let result = buffer.insert(CharOffset(3), "d");

        assert!(matches!(result, Err(BufferError::RevisionExhausted)));
        assert_eq!(buffer.text().unwrap(), "abc");
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.can_undo());
        assert!(!buffer.is_modified());
    }

    #[test]
    fn revision_exhaustion_rolls_back_entire_session() {
        let mut buffer = Buffer::from_text("abc");
        buffer.set_revision_counter(u64::MAX);

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(3), "d").unwrap();
        assert_eq!(buffer.text().unwrap(), "abcd");

        let result = buffer.commit_transaction();

        assert!(matches!(result, Err(BufferError::RevisionExhausted)));
        assert_eq!(buffer.text().unwrap(), "abc");
        assert!(!buffer.has_active_transaction());
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.can_undo());
        assert!(!buffer.is_modified());
    }

    #[test]
    fn revision_ids_are_not_reused_after_exhaustion() {
        let mut buffer = Buffer::from_text("");
        buffer.set_revision_counter(u64::MAX - 1);

        buffer.insert(CharOffset(0), "a").unwrap();
        assert_eq!(buffer.revision(), u64::MAX);

        let result = buffer.insert(CharOffset(1), "b");

        assert!(matches!(result, Err(BufferError::RevisionExhausted)));
        assert_eq!(buffer.text().unwrap(), "a");
        assert_eq!(buffer.revision(), u64::MAX);
        assert!(buffer.can_undo());
    }
}

#[cfg(test)]
mod consolidation_tests {
    use super::*;

    #[test]
    fn insertion_then_removal_in_session_has_no_net_effect() {
        let mut buffer = Buffer::from_text("abc");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(3), "xyz").unwrap();
        buffer
            .delete(TextRange::new(CharOffset(3), CharOffset(6)))
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "abc");
        assert!(buffer.is_modified());

        let result = buffer.commit_transaction().unwrap();

        assert!(result.is_none());
        assert_eq!(buffer.text().unwrap(), "abc");
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.is_modified());
        assert!(!buffer.can_undo());
    }

    #[test]
    fn removal_then_insertion_in_session_is_one_transaction() {
        let mut buffer = Buffer::from_text("abc");

        buffer.begin_transaction().unwrap();
        buffer
            .delete(TextRange::new(CharOffset(0), CharOffset(3)))
            .unwrap();
        buffer.insert(CharOffset(0), "xyz").unwrap();
        let transaction = buffer.commit_transaction().unwrap().unwrap();

        assert_eq!(transaction.len(), 2);
        assert_eq!(buffer.text().unwrap(), "xyz");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "abc");

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "xyz");
    }

    #[test]
    fn consecutive_edits_over_same_region_round_trip() {
        let mut buffer = Buffer::from_text("abc");

        buffer.begin_transaction().unwrap();
        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(1)), "X")
            .unwrap();
        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(1)), "Y")
            .unwrap();
        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(2)), "Z")
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "Zc");

        let transaction = buffer.commit_transaction().unwrap().unwrap();
        assert_eq!(transaction.len(), 3);

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "abc");

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "Zc");
    }

    #[test]
    fn redo_is_preserved_after_session_cancel() {
        let mut buffer = Buffer::from_text("");

        buffer.insert(CharOffset(0), "a").unwrap();
        assert!(buffer.undo().unwrap());
        assert!(buffer.can_redo());

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(0), "b").unwrap();
        buffer.cancel_transaction().unwrap();

        assert_eq!(buffer.text().unwrap(), "");
        assert!(buffer.can_redo());
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "a");
    }

    #[test]
    fn net_zero_session_restores_unmodified_state() {
        let mut buffer = Buffer::from_text("abc");
        buffer.mark_saved().unwrap();

        buffer.begin_transaction().unwrap();
        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(3)), "xyz")
            .unwrap();
        buffer
            .replace(TextRange::new(CharOffset(0), CharOffset(3)), "abc")
            .unwrap();

        assert!(buffer.is_modified());
        assert_eq!(buffer.revision(), 0);

        let result = buffer.commit_transaction().unwrap();

        assert!(result.is_none());
        assert_eq!(buffer.text().unwrap(), "abc");
        assert_eq!(buffer.revision(), 0);
        assert_eq!(buffer.saved_revision(), 0);
        assert!(!buffer.is_modified());
        assert!(!buffer.can_undo());
    }

    #[test]
    fn failed_undo_restores_state_and_preserves_history() {
        let mut buffer = Buffer::from_text("");

        buffer
            .apply_transaction(vec![
                Edit::new(TextRange::new(CharOffset(0), CharOffset(0)), "a".to_owned()),
                Edit::new(TextRange::new(CharOffset(1), CharOffset(1)), "b".to_owned()),
            ])
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "ab");
        assert_eq!(buffer.revision(), 1);

        buffer.set_apply_failure_after(1);

        let result = buffer.undo();

        assert!(matches!(result, Err(BufferError::TestFailure)));
        assert_eq!(buffer.text().unwrap(), "ab");
        assert_eq!(buffer.revision(), 1);
        assert_eq!(buffer.saved_revision(), 0);
        assert!(buffer.is_modified());
        assert!(buffer.can_undo());
        assert!(!buffer.can_redo());

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "");
        assert_eq!(buffer.revision(), 0);
        assert!(buffer.can_redo());
    }

    #[test]
    fn failed_redo_restores_state_and_preserves_history() {
        let mut buffer = Buffer::from_text("");

        buffer
            .apply_transaction(vec![
                Edit::new(TextRange::new(CharOffset(0), CharOffset(0)), "a".to_owned()),
                Edit::new(TextRange::new(CharOffset(1), CharOffset(1)), "b".to_owned()),
            ])
            .unwrap();

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "");
        assert_eq!(buffer.revision(), 0);

        buffer.set_apply_failure_after(1);

        let result = buffer.redo();

        assert!(matches!(result, Err(BufferError::TestFailure)));
        assert_eq!(buffer.text().unwrap(), "");
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.can_undo());
        assert!(buffer.can_redo());

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "ab");
        assert_eq!(buffer.revision(), 1);
    }

    fn poisoned_by_session_rollback() -> Buffer {
        let mut buffer = Buffer::from_text("abc");

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(3), "d").unwrap();
        buffer.insert(CharOffset(4), "e").unwrap();

        buffer.set_apply_failure_after(1);

        let result = buffer.cancel_transaction();

        assert!(matches!(
            result,
            Err(BufferError::RecoveryFailed { original: None, .. })
        ));

        buffer
    }

    #[test]
    fn failed_session_rollback_enters_recovery_pending() {
        let buffer = poisoned_by_session_rollback();
        let report = buffer.recovery_report().unwrap();

        assert_eq!(buffer.integrity(), BufferIntegrity::RecoveryPending);
        assert!(!buffer.is_consistent());
        assert!(report.original_error.is_none());
        assert_eq!(report.recovery_error, BufferError::TestFailure);
        assert_eq!(report.pending_edits, 1);

        // A reversão parcial removeu "e", mas "d" continua aplicado.
        assert_eq!(buffer.content.to_string(), "abcd");
        assert_eq!(buffer.revision(), 0);
        assert_eq!(buffer.saved_revision(), 0);
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
        assert!(buffer.is_modified());
    }

    #[test]
    fn recovery_pending_blocks_normal_operations_and_reads() {
        let mut buffer = poisoned_by_session_rollback();

        assert!(matches!(buffer.text(), Err(BufferError::RecoveryRequired)));
        assert!(matches!(
            buffer.len_chars(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.len_lines(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.is_empty(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.char_at(CharOffset(0)),
            Err(BufferError::RecoveryRequired)
        ));

        assert!(matches!(
            buffer.insert(CharOffset(0), "x"),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.delete(TextRange::new(CharOffset(0), CharOffset(1))),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.replace(TextRange::new(CharOffset(0), CharOffset(1)), "x"),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.apply_edit(Edit::new(
                TextRange::new(CharOffset(0), CharOffset(0)),
                "x".to_owned()
            )),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.apply_transaction(Vec::new()),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.begin_transaction(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.commit_transaction(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.cancel_transaction(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(buffer.undo(), Err(BufferError::RecoveryRequired)));
        assert!(matches!(buffer.redo(), Err(BufferError::RecoveryRequired)));
        assert!(matches!(
            buffer.mark_saved(),
            Err(BufferError::RecoveryRequired)
        ));
        assert!(matches!(
            buffer.mark_saved_at(0),
            Err(BufferError::RecoveryRequired)
        ));

        assert_eq!(buffer.revision(), 0);
        assert_eq!(buffer.saved_revision(), 0);
        assert_eq!(buffer.integrity(), BufferIntegrity::RecoveryPending);
    }

    #[test]
    fn failed_recovery_preserves_revision_and_underlying_history() {
        let mut buffer = Buffer::from_text("base");

        buffer.insert(CharOffset(4), "-1").unwrap();
        assert_eq!(buffer.revision(), 1);
        assert!(buffer.history.can_undo());

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(6), "-2").unwrap();
        buffer.insert(CharOffset(8), "-3").unwrap();

        buffer.set_apply_failure_after(1);

        assert!(matches!(
            buffer.cancel_transaction(),
            Err(BufferError::RecoveryFailed { original: None, .. })
        ));

        assert_eq!(buffer.revision(), 1);
        assert_eq!(buffer.saved_revision(), 0);
        assert!(buffer.history.can_undo());
        assert!(!buffer.history.can_redo());
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
        assert!(buffer.recovery_report().is_some());
    }

    #[test]
    fn retry_recovery_completes_pending_rollback() {
        let mut buffer = poisoned_by_session_rollback();

        assert_eq!(buffer.content.to_string(), "abcd");
        assert!(!buffer.is_consistent());

        buffer.retry_recovery().unwrap();

        assert!(buffer.is_consistent());
        assert_eq!(buffer.integrity(), BufferIntegrity::Consistent);
        assert!(buffer.recovery_report().is_none());
        assert_eq!(buffer.text().unwrap(), "abc");
        assert_eq!(buffer.revision(), 0);
        assert_eq!(buffer.saved_revision(), 0);
        assert!(!buffer.is_modified());
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());

        // O Buffer volta a aceitar operações normais.
        buffer.insert(CharOffset(3), "d").unwrap();
        assert_eq!(buffer.text().unwrap(), "abcd");
        assert_eq!(buffer.revision(), 1);
        assert!(buffer.can_undo());
    }

    #[test]
    fn retry_recovery_after_consistent_buffer_is_a_no_op() {
        let mut buffer = Buffer::from_text("abc");

        buffer.retry_recovery().unwrap();

        assert!(buffer.is_consistent());
        assert_eq!(buffer.text().unwrap(), "abc");
        assert_eq!(buffer.revision(), 0);
    }

    #[test]
    fn failed_retry_preserves_original_failure_and_allows_new_attempt() {
        let mut buffer = Buffer::from_text("abc");
        buffer.set_revision_counter(u64::MAX);

        buffer.begin_transaction().unwrap();
        buffer.insert(CharOffset(3), "d").unwrap();

        // A alocação da revisão falha imediatamente e, em seguida, a reversão
        // controlada também falha, caracterizando uma falha de recuperação.
        buffer.set_apply_failure_after(0);

        assert!(matches!(
            buffer.commit_transaction(),
            Err(BufferError::RecoveryFailed {
                original: Some(ref original),
                recovery,
            }) if **original == BufferError::RevisionExhausted
                && *recovery == BufferError::TestFailure
        ));

        let report = buffer.recovery_report().unwrap();
        assert_eq!(report.original_error, Some(BufferError::RevisionExhausted));
        assert_eq!(report.recovery_error, BufferError::TestFailure);
        assert_eq!(report.pending_edits, 1);
        assert_eq!(buffer.content.to_string(), "abcd");

        // Uma nova tentativa também falha, mas mantém a falha original.
        buffer.set_apply_failure_after(0);

        assert!(matches!(
            buffer.retry_recovery(),
            Err(BufferError::RecoveryFailed {
                original: Some(ref original),
                recovery,
            }) if **original == BufferError::RevisionExhausted
                && *recovery == BufferError::TestFailure
        ));
        assert!(!buffer.is_consistent());
        assert_eq!(buffer.content.to_string(), "abcd");
        assert!(matches!(buffer.text(), Err(BufferError::RecoveryRequired)));

        // Sem nova falha controlada, a recuperação conclui a reversão.
        buffer.retry_recovery().unwrap();

        assert!(buffer.is_consistent());
        assert_eq!(buffer.text().unwrap(), "abc");
        assert_eq!(buffer.revision(), 0);
        assert_eq!(buffer.saved_revision(), 0);
        assert!(!buffer.is_modified());
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
    }
}
