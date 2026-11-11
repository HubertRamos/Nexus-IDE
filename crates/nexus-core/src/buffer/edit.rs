use super::position::CharOffset;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: CharOffset,
    pub end: CharOffset,
}

impl TextRange {
    pub fn new(start: CharOffset, end: CharOffset) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub range: TextRange,
    pub inserted_text: String,
}

impl Edit {
    pub fn new(range: TextRange, inserted_text: String) -> Self {
        Self {
            range,
            inserted_text,
        }
    }
}

/// Resultado de uma edição aplicada ao documento.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub range: TextRange,
    pub deleted_text: String,
    pub inserted_text: String,
}

impl AppliedEdit {
    /// Intervalo ocupado pelo texto inserido após a edição.
    pub fn inserted_range(&self) -> TextRange {
        let inserted_len = self.inserted_text.chars().count();
        TextRange::new(
            self.range.start,
            CharOffset(self.range.start.0 + inserted_len),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferError {
    InvalidRange {
        start: usize,
        end: usize,
    },
    OutOfBounds {
        offset: usize,
        len: usize,
    },
    /// Uma operação falhou e a tentativa de recuperar o estado também falhou.
    ///
    /// `original` contém a falha que iniciou a recuperação, quando existir. O
    /// cancelamento de uma sessão, por exemplo, não tem uma falha original:
    /// sua própria reversão é a operação que pode falhar.
    RecoveryFailed {
        original: Option<Box<BufferError>>,
        recovery: Box<BufferError>,
    },
    /// O Buffer está em estado de recuperação pendente e não pode ser usado
    /// como se seu conteúdo fosse íntegro.
    RecoveryRequired,
    /// A operação exige que não exista uma sessão explícita aberta.
    TransactionAlreadyActive,
    /// A operação exige uma sessão explícita aberta.
    NoActiveTransaction,
    /// Não há mais identificadores de revisão disponíveis.
    RevisionExhausted,
    /// A revisão informada não pertence ao ramo atual do histórico.
    InvalidRevision {
        revision: u64,
    },
    /// Falha controlada usada apenas em testes de recuperação.
    #[cfg(any(test, feature = "test-support"))]
    TestFailure,
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferError::InvalidRange { start, end } => {
                write!(formatter, "intervalo inválido: início {start}, fim {end}")
            }
            BufferError::OutOfBounds { offset, len } => {
                write!(
                    formatter,
                    "posição fora dos limites: deslocamento {offset}, comprimento {len}"
                )
            }
            BufferError::RecoveryFailed { original, recovery } => {
                if let Some(original) = original {
                    write!(
                        formatter,
                        "operação falhou ({original}) e a recuperação também falhou ({recovery})"
                    )
                } else {
                    write!(formatter, "falha ao recuperar o documento: {recovery}")
                }
            }
            BufferError::RecoveryRequired => {
                write!(formatter, "o documento está em recuperação pendente")
            }
            BufferError::TransactionAlreadyActive => {
                write!(formatter, "já existe uma sessão de transação ativa")
            }
            BufferError::NoActiveTransaction => {
                write!(formatter, "não existe sessão de transação ativa")
            }
            BufferError::RevisionExhausted => {
                write!(
                    formatter,
                    "não há mais identificadores de revisão disponíveis"
                )
            }
            BufferError::InvalidRevision { revision } => {
                write!(formatter, "revisão inválida: {revision}")
            }
            #[cfg(any(test, feature = "test-support"))]
            BufferError::TestFailure => write!(formatter, "falha controlada de teste"),
        }
    }
}

impl std::error::Error for BufferError {}
